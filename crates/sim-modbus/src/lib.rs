//! Modbus TCP emulation for GivEnergy data-adapter protocol.
//!
//! Implements the proprietary GivEnergy MBAP variant used by the Wi-Fi dongle.
//! Standard Modbus frames are wrapped inside a transparent-message envelope:
//!
#![allow(clippy::needless_range_loop)]
//! ```text
//! Bytes 0-1:    Transaction ID      — fixed 0x5959
//! Bytes 2-3:    Protocol ID         — fixed 0x0001
//! Bytes 4-5:    Length              — bytes after this field
//! Byte  6:      Unit ID             — fixed 0x01
//! Byte  7:      Function ID         — 0x02 (transparent message)
//! Bytes 8-17:   Data-adapter serial — 10 bytes, Latin-1, space-padded
//! Bytes 18-25:  Padding             — big-endian u64 value 8
//! Byte  26:     Slave address       — 0x32 (reads), 0x11 (writes)
//! Byte  27:     Inner function code — 0x03/0x04 (read), 0x06 (write)
//! Bytes 28+:    Inner payload       — register address + count/values
//! Last 2 bytes: CRC-16/Modbus over bytes 26..end-2
//! ```
//!
//! Supports:
//! - Inner function 0x03 — Read Holding Registers
//! - Inner function 0x04 — Read Input Registers
//! - Inner function 0x06 — Write Single Register

use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ---------------------------------------------------------------------------
// GivEnergy frame constants
// ---------------------------------------------------------------------------

/// Fixed transaction ID for GivEnergy frames.
pub const TRANSACTION_ID: u16 = 0x5959;

/// Fixed protocol ID for GivEnergy frames.
pub const PROTOCOL_ID: u16 = 0x0001;

/// Fixed unit ID for GivEnergy frames.
pub const UNIT_ID: u8 = 0x01;

/// Heartbeat main function code.
pub const FUNC_HEARTBEAT: u8 = 0x01;

/// Transparent-message function code used by the GivEnergy data adapter.
pub const FUNC_TRANSPARENT: u8 = 0x02;

/// Length of the serial field (Latin-1, space-padded).
pub const SERIAL_LEN: usize = 10;

/// GivEnergy header size: tx_id(2) + proto_id(2) + length(2) + unit_id(1)
/// + func_id(1) + serial(10) + padding(8) = 26 bytes.
pub const HEADER_SIZE: usize = 2 + 2 + 2 + 1 + 1 + SERIAL_LEN + 8;

// ---------------------------------------------------------------------------
// Inner Modbus function codes
// ---------------------------------------------------------------------------

pub const FC_READ_HOLDING: u8 = 0x03;
pub const FC_READ_INPUT: u8 = 0x04;
pub const FC_READ_METER: u8 = 0x16;
pub const FC_WRITE_SINGLE: u8 = 0x06;

// ---------------------------------------------------------------------------
// Exception codes
// ---------------------------------------------------------------------------

pub const EC_ILLEGAL_FUNCTION: u8 = 0x01;
pub const EC_ILLEGAL_DATA_ADDRESS: u8 = 0x02;

// ---------------------------------------------------------------------------
// CRC
// ---------------------------------------------------------------------------

/// CRC-16/Modbus lookup table.
const CRC_TABLE: crc::Crc<u16> = crc::Crc::<u16>::new(&crc::CRC_16_MODBUS);

/// Calculate CRC-16/Modbus.
pub fn crc16(data: &[u8]) -> u16 {
    CRC_TABLE.checksum(data)
}

// ---------------------------------------------------------------------------
// Command type
// ---------------------------------------------------------------------------

/// A sender for commands produced by Modbus writes.
pub type CommandSender = tokio::sync::mpsc::UnboundedSender<ModbusCommand>;

/// A command originating from a Modbus write operation.
#[derive(Debug, Clone)]
pub struct ModbusCommand {
    /// The register address that was written.
    pub address: u16,
    /// The value that was written.
    pub value: u16,
}

// ---------------------------------------------------------------------------
// Frame helpers
// ---------------------------------------------------------------------------

/// Build a GivEnergy response frame wrapping an inner Modbus PDU.
///
/// The inner PDU = slave_address + function_code + payload.
/// CRC-16/Modbus is appended over the inner PDU.
pub fn build_response(serial: &[u8; SERIAL_LEN], slave: u8, func: u8, payload: &[u8]) -> Vec<u8> {
    build_response_with_padding(serial, slave, func, payload, 0x8A)
}

/// Build a GivEnergy response frame with explicit transparent padding byte value.
pub fn build_response_with_padding(
    serial: &[u8; SERIAL_LEN],
    slave: u8,
    func: u8,
    payload: &[u8],
    padding: u8,
) -> Vec<u8> {
    // Inner PDU: slave + func + payload + CRC
    let mut inner = Vec::with_capacity(2 + payload.len() + 2);
    inner.push(slave);
    inner.push(func);
    inner.extend_from_slice(payload);
    let crc = crc16(&inner);
    inner.extend_from_slice(&crc.to_le_bytes());

    // GivEnergy header
    let length = (1 + 1 + SERIAL_LEN + 8 + inner.len()) as u16;
    let mut frame = Vec::with_capacity(HEADER_SIZE + inner.len());
    frame.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    frame.extend_from_slice(&PROTOCOL_ID.to_be_bytes());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(UNIT_ID);
    frame.push(FUNC_TRANSPARENT);
    frame.extend_from_slice(serial);
    frame.extend_from_slice(&(padding as u64).to_be_bytes());
    frame.extend_from_slice(&inner);
    frame
}

/// Build a GivEnergy heartbeat response frame.
pub fn build_heartbeat_response(serial: &[u8; SERIAL_LEN]) -> Vec<u8> {
    let mut frame = Vec::with_capacity(HEADER_SIZE + SERIAL_LEN);
    frame.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    frame.extend_from_slice(&PROTOCOL_ID.to_be_bytes());
    // length = unit_id(1) + func_id(1) + serial(10) = 12
    let length: u16 = 12;
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(UNIT_ID);
    frame.push(FUNC_HEARTBEAT);
    frame.extend_from_slice(serial);
    frame
}

/// Build a GivEnergy error response frame.
pub fn build_error_response(
    serial: &[u8; SERIAL_LEN],
    slave: u8,
    func: u8,
    exception: u8,
) -> Vec<u8> {
    build_response_with_padding(serial, slave, func | 0x80, &[exception], 0x12)
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Run a GivEnergy-compatible Modbus TCP server.
///
/// Listens for GivEnergy proprietary frames and responds in kind.
/// Supports multi-slave addressing for battery BMS modules:
/// - Slave 0x32: inverter registers + battery #1 BMS (IR 60-119)
/// - Slave 0x33-0x36: battery modules 2-5 BMS (IR 60-119)
pub async fn run_modbus_server(
    addr: SocketAddr,
    register_store: std::sync::Arc<tokio::sync::Mutex<sim_registers::RegisterStore>>,
    command_tx: CommandSender,
    battery_state: std::sync::Arc<tokio::sync::Mutex<Vec<sim_models::BatteryState>>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("GivEnergy Modbus TCP server listening on {addr}");

    loop {
        let (mut stream, peer) = listener.accept().await?;
        tracing::info!("Modbus connection from {peer}");
        let store = register_store.clone();
        let cmd_tx = command_tx.clone();
        let batt_state = battery_state.clone();

        tokio::spawn(async move {
            let mut buf = [0u8; 512];
            let mut pending: Vec<u8> = Vec::new();

            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => n,
                    Err(e) => {
                        tracing::warn!("Modbus read error from {peer}: {e}");
                        break;
                    }
                };

                pending.extend_from_slice(&buf[..n]);

                // Process complete frames
                loop {
                    // Need at least the GivEnergy header (26 bytes)
                    if pending.len() < HEADER_SIZE {
                        break;
                    }

                    // Validate envelope
                    let tx_id = u16::from_be_bytes([pending[0], pending[1]]);
                    if tx_id != TRANSACTION_ID {
                        tracing::warn!("Invalid transaction ID 0x{tx_id:04X}, dropping connection");
                        return;
                    }

                    let proto_id = u16::from_be_bytes([pending[2], pending[3]]);
                    if proto_id != PROTOCOL_ID {
                        tracing::warn!("Invalid protocol ID 0x{proto_id:04X}, dropping connection");
                        return;
                    }

                    let func_id = pending[7];

                    let length = u16::from_be_bytes([pending[4], pending[5]]) as usize;
                    let frame_len = 6 + length; // bytes 0-5 + everything after

                    if pending.len() < frame_len {
                        break; // incomplete frame, wait for more data
                    }

                    let frame = &pending[..frame_len];

                    // Extract serial (bytes 8-17)
                    let mut serial = [b' '; SERIAL_LEN];
                    serial.copy_from_slice(&frame[8..8 + SERIAL_LEN]);

                    // Handle heartbeat (main function 0x01)
                    if func_id == FUNC_HEARTBEAT {
                        let resp = build_heartbeat_response(&serial);
                        let _ = stream.write_all(&resp).await;
                        pending.drain(..frame_len);
                        continue;
                    }

                    if func_id != FUNC_TRANSPARENT {
                        tracing::warn!("Invalid function ID 0x{func_id:02X}, dropping connection");
                        return;
                    }

                    // Inner PDU starts at byte 26 (HEADER_SIZE)
                    let inner_pdu = &frame[HEADER_SIZE..];

                    // Inner PDU: slave(1) + func(1) + payload + CRC(2)
                    if inner_pdu.len() < 4 {
                        // Need at least slave + func + CRC
                        tracing::warn!("Inner PDU too short: {} bytes", inner_pdu.len());
                        pending.drain(..frame_len);
                        continue;
                    }

                    let slave = inner_pdu[0];
                    let inner_func = inner_pdu[1];
                    let inner_payload = &inner_pdu[2..inner_pdu.len() - 2]; // strip CRC

                    // Verify CRC
                    let crc_received = u16::from_le_bytes([
                        inner_pdu[inner_pdu.len() - 2],
                        inner_pdu[inner_pdu.len() - 1],
                    ]);
                    let crc_calc = crc16(&inner_pdu[..inner_pdu.len() - 2]);
                    if crc_received != crc_calc {
                        tracing::warn!(
                            "CRC mismatch: received 0x{crc_received:04X}, calculated 0x{crc_calc:04X}"
                        );
                        // Continue processing — lenient like the reference library
                    }

                    match inner_func {
                        FC_READ_HOLDING | FC_READ_INPUT | FC_READ_METER => {
                            if inner_payload.len() < 4 {
                                tracing::warn!("Read request payload too short");
                                let resp = build_error_response(
                                    &serial,
                                    slave,
                                    inner_func,
                                    EC_ILLEGAL_DATA_ADDRESS,
                                );
                                let _ = stream.write_all(&resp).await;
                                pending.drain(..frame_len);
                                continue;
                            }
                            let start_addr =
                                u16::from_be_bytes([inner_payload[0], inner_payload[1]]);
                            let count = u16::from_be_bytes([inner_payload[2], inner_payload[3]]);

                            if count == 0 || count > 60 {
                                let resp = build_error_response(
                                    &serial,
                                    slave,
                                    inner_func,
                                    EC_ILLEGAL_DATA_ADDRESS,
                                );
                                let _ = stream.write_all(&resp).await;
                                pending.drain(..frame_len);
                                continue;
                            }

                            // Check if this is a meter read (IR 60-89 on meter slaves 0x01-0x08)
                            let is_meter = (0x01..=0x08).contains(&slave)
                                && inner_func == FC_READ_INPUT
                                && (60..90).contains(&start_addr);

                            if is_meter {
                                // Serve meter registers from the shared store
                                let space = sim_registers::RegisterSpace::Input;
                                let store_guard = store.lock().await;
                                let mut data = Vec::with_capacity(count as usize * 2);
                                for i in 0..count {
                                    let val = store_guard
                                        .read_by_space(start_addr + i, space)
                                        .unwrap_or(0);
                                    data.extend_from_slice(&val.to_be_bytes());
                                }
                                drop(store_guard);
                                let mut resp_payload =
                                    Vec::with_capacity(SERIAL_LEN + 4 + data.len());
                                resp_payload.extend_from_slice(&serial);
                                resp_payload.extend_from_slice(&start_addr.to_be_bytes());
                                resp_payload.extend_from_slice(&count.to_be_bytes());
                                resp_payload.extend_from_slice(&data);
                                let resp = build_response_with_padding(
                                    &serial,
                                    slave,
                                    inner_func,
                                    &resp_payload,
                                    0x8A,
                                );
                                let _ = stream.write_all(&resp).await;
                                pending.drain(..frame_len);
                                continue;
                            }

                            // Check if this is a battery BMS read (IR 60-119 on battery slaves)
                            let battery_index = if slave == 0x32 {
                                // Battery #1 BMS: IR 60-119 on inverter slave
                                if inner_func == FC_READ_INPUT && (60..120).contains(&start_addr) {
                                    Some(0usize)
                                } else {
                                    None
                                }
                            } else if (0x33..=0x36).contains(&slave) {
                                // Additional batteries: IR 60-119 on battery slaves
                                if inner_func == FC_READ_INPUT && (60..120).contains(&start_addr) {
                                    Some((slave - 0x33 + 1) as usize)
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            let reg_data = if let Some(batt_idx) = battery_index {
                                // Serve BMS data from battery state
                                let batts = batt_state.lock().await;
                                if let Some(battery) = batts.get(batt_idx) {
                                    let store_guard = store.lock().await;
                                    let bms = store_guard.project_battery_bms(battery, batt_idx);
                                    drop(store_guard);
                                    let mut data = Vec::with_capacity(count as usize * 2);
                                    for i in 0..count {
                                        let idx =
                                            start_addr.saturating_sub(60) as usize + i as usize;
                                        let val = if idx < 60 { bms[idx] } else { 0 };
                                        data.extend_from_slice(&val.to_be_bytes());
                                    }
                                    data
                                } else {
                                    // No battery at this index — return zeros
                                    vec![0u8; count as usize * 2]
                                }
                            } else {
                                // Normal register read from store
                                let space = if inner_func == FC_READ_INPUT {
                                    sim_registers::RegisterSpace::Input
                                } else {
                                    sim_registers::RegisterSpace::Holding
                                };
                                let store_guard = store.lock().await;
                                let mut data = Vec::with_capacity(count as usize * 2);
                                for i in 0..count {
                                    let val = store_guard
                                        .read_by_space(start_addr + i, space)
                                        .unwrap_or(0);
                                    data.extend_from_slice(&val.to_be_bytes());
                                }
                                data
                            };

                            // Build read response payload:
                            // serial(10) + base_register(2) + register_count(2) + data(N×2)
                            let mut resp_payload =
                                Vec::with_capacity(SERIAL_LEN + 4 + reg_data.len());
                            resp_payload.extend_from_slice(&serial);
                            resp_payload.extend_from_slice(&start_addr.to_be_bytes());
                            resp_payload.extend_from_slice(&count.to_be_bytes());
                            resp_payload.extend_from_slice(&reg_data);

                            let resp = build_response_with_padding(
                                &serial,
                                slave,
                                inner_func,
                                &resp_payload,
                                0x8A,
                            );
                            tracing::debug!(
                                "Read response: slave=0x{:02X} fn=0x{:02X} start={} count={}",
                                slave,
                                inner_func,
                                start_addr,
                                count
                            );
                            let _ = stream.write_all(&resp).await;
                        }

                        FC_WRITE_SINGLE => {
                            if inner_payload.len() < 4 {
                                let resp = build_error_response(
                                    &serial,
                                    slave,
                                    inner_func,
                                    EC_ILLEGAL_DATA_ADDRESS,
                                );
                                let _ = stream.write_all(&resp).await;
                                pending.drain(..frame_len);
                                continue;
                            }
                            let address = u16::from_be_bytes([inner_payload[0], inner_payload[1]]);
                            let value = u16::from_be_bytes([inner_payload[2], inner_payload[3]]);

                            let success = {
                                let mut store = store.lock().await;
                                store.write(address, value)
                            };

                            if success {
                                // Write response: serial(10) + register(2) + value(2)
                                let resp_payload = {
                                    let mut p = Vec::with_capacity(SERIAL_LEN + 4);
                                    p.extend_from_slice(&serial);
                                    p.extend_from_slice(&address.to_be_bytes());
                                    p.extend_from_slice(&value.to_be_bytes());
                                    p
                                };
                                let resp =
                                    build_response(&serial, slave, inner_func, &resp_payload);
                                let _ = stream.write_all(&resp).await;
                                let _ = cmd_tx.send(ModbusCommand { address, value });
                                tracing::info!("Modbus write: addr={address}, value={value}");
                            } else {
                                let resp = build_error_response(
                                    &serial,
                                    slave,
                                    inner_func,
                                    EC_ILLEGAL_DATA_ADDRESS,
                                );
                                let _ = stream.write_all(&resp).await;
                                tracing::warn!("Modbus write rejected: addr={address} (read-only)");
                            }
                        }

                        _ => {
                            tracing::warn!("Unsupported inner function code 0x{inner_func:02X}");
                            let resp = build_error_response(
                                &serial,
                                slave,
                                inner_func,
                                EC_ILLEGAL_FUNCTION,
                            );
                            let _ = stream.write_all(&resp).await;
                        }
                    }

                    pending.drain(..frame_len);
                }
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Standard Modbus TCP server for GivEVC (Electric Vehicle Charger)
// ---------------------------------------------------------------------------
// The EVC uses STANDARD Modbus TCP (NOT the proprietary GivEnergy framing).
// It serves HR 0-119 on a separate port (default 8898).

const EVC_PORT: u16 = 8898;

fn evc_state_to_registers(evc: &sim_models::EvcState) -> Vec<u16> {
    let mut regs = vec![0u16; 120];
    regs[0] = evc.charging_state;
    regs[2] = evc.cable_status;
    regs[4] = evc.error_code;
    regs[6] = (evc.current_l1 * 100.0) as u16;
    regs[8] = (evc.current_l2 * 100.0) as u16;
    regs[10] = (evc.current_l3 * 100.0) as u16;
    regs[13] = evc.active_power_w as u16;
    regs[17] = (evc.active_power_w as u16).min(evc.charge_current_setting.saturating_mul(230));
    regs[20] = 0;
    regs[24] = 0;
    regs[91] = evc.charge_current_setting;
    regs[93] = evc.charge_control;
    regs[95] = evc.charging_mode;
    regs
}

fn registers_to_evc(_regs: &[u16], addr: u16, value: u16, evc: &mut sim_models::EvcState) {
    match addr {
        91 => evc.charge_current_setting = value.clamp(6, 32),
        93 => evc.charge_control = value.min(2),
        95 => evc.charging_mode = value.min(2),
        _ => {}
    }
}

/// Run a standard Modbus TCP server for the EVC.
pub async fn run_evc_modbus_server(
    evc_state: std::sync::Arc<tokio::sync::Mutex<sim_models::EvcState>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let addr: std::net::SocketAddr = format!("0.0.0.0:{EVC_PORT}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("EVC Modbus TCP server (standard) listening on {addr}");

    loop {
        let (mut stream, peer) = listener.accept().await?;
        let evc = evc_state.clone();
        tracing::info!("EVC client connected from {peer}");

        tokio::spawn(async move {
            let mut buf = [0u8; 1024];
            loop {
                let n = match stream.read(&mut buf).await {
                    Ok(0) => return,
                    Ok(n) => n,
                    Err(_) => return,
                };

                if n < 8 {
                    continue;
                }

                let trans_id = u16::from_be_bytes([buf[0], buf[1]]);
                let proto_id = u16::from_be_bytes([buf[2], buf[3]]);
                let _len = u16::from_be_bytes([buf[4], buf[5]]);
                let unit_id = buf[6];
                let func = buf[7];

                if proto_id != 0 {
                    continue; // Not Modbus TCP
                }

                match func {
                    0x03 => {
                        // Read Holding Registers
                        if n < 12 {
                            continue;
                        }
                        let start = u16::from_be_bytes([buf[8], buf[9]]);
                        let count = u16::from_be_bytes([buf[10], buf[11]]);
                        if count == 0 || count > 120 || start + count > 120 {
                            let resp = build_evc_error(trans_id, unit_id, func, 0x02);
                            let _ = stream.write_all(&resp).await;
                            continue;
                        }
                        let guard = evc.lock().await;
                        let regs = evc_state_to_registers(&guard);
                        drop(guard);
                        let byte_count = (count * 2) as u8;
                        let mut resp = Vec::with_capacity(9 + count as usize * 2);
                        resp.extend_from_slice(&trans_id.to_be_bytes());
                        resp.extend_from_slice(&0x0000u16.to_be_bytes());
                        let payload_len = 2 + count as usize * 2;
                        resp.extend_from_slice(&(payload_len as u16).to_be_bytes());
                        resp.push(unit_id);
                        resp.push(func);
                        resp.push(byte_count);
                        for i in start..start + count {
                            resp.extend_from_slice(&regs[i as usize].to_be_bytes());
                        }
                        let _ = stream.write_all(&resp).await;
                    }
                    0x06 => {
                        // Write Single Register
                        if n < 12 {
                            continue;
                        }
                        let addr_r = u16::from_be_bytes([buf[8], buf[9]]);
                        let value = u16::from_be_bytes([buf[10], buf[11]]);
                        if addr_r >= 120 {
                            let resp = build_evc_error(trans_id, unit_id, func, 0x02);
                            let _ = stream.write_all(&resp).await;
                            continue;
                        }
                        {
                            let mut guard = evc.lock().await;
                            registers_to_evc(&[], addr_r, value, &mut guard);
                        }
                        // Echo response
                        let mut resp = Vec::with_capacity(12);
                        resp.extend_from_slice(&trans_id.to_be_bytes());
                        resp.extend_from_slice(&0x0000u16.to_be_bytes());
                        resp.extend_from_slice(&0x0006u16.to_be_bytes());
                        resp.push(unit_id);
                        resp.push(func);
                        resp.extend_from_slice(&addr_r.to_be_bytes());
                        resp.extend_from_slice(&value.to_be_bytes());
                        let _ = stream.write_all(&resp).await;
                        tracing::info!("EVC write: addr={addr_r}, value={value}");
                    }
                    0x10 => {
                        // Write Multiple Registers
                        if n < 13 {
                            continue;
                        }
                        let addr_r = u16::from_be_bytes([buf[8], buf[9]]);
                        let count = u16::from_be_bytes([buf[10], buf[11]]);
                        if count == 0 || addr_r + count > 120 {
                            let resp = build_evc_error(trans_id, unit_id, func, 0x02);
                            let _ = stream.write_all(&resp).await;
                            continue;
                        }
                        {
                            let mut guard = evc.lock().await;
                            for i in 0..count as usize {
                                let idx = 13 + i * 2;
                                if idx + 1 < n {
                                    let v = u16::from_be_bytes([buf[idx], buf[idx + 1]]);
                                    registers_to_evc(&[], addr_r + i as u16, v, &mut guard);
                                }
                            }
                        }
                        let mut resp = Vec::with_capacity(12);
                        resp.extend_from_slice(&trans_id.to_be_bytes());
                        resp.extend_from_slice(&0x0000u16.to_be_bytes());
                        resp.extend_from_slice(&0x0006u16.to_be_bytes());
                        resp.push(unit_id);
                        resp.push(func);
                        resp.extend_from_slice(&addr_r.to_be_bytes());
                        resp.extend_from_slice(&count.to_be_bytes());
                        let _ = stream.write_all(&resp).await;
                        tracing::info!("EVC multi-write: addr={addr_r}, count={count}");
                    }
                    _ => {
                        let resp = build_evc_error(trans_id, unit_id, func, 0x01);
                        let _ = stream.write_all(&resp).await;
                    }
                }
            }
        });
    }
}

fn build_evc_error(trans_id: u16, unit_id: u8, func: u8, code: u8) -> Vec<u8> {
    let mut resp = Vec::with_capacity(9);
    resp.extend_from_slice(&trans_id.to_be_bytes());
    resp.extend_from_slice(&0x0000u16.to_be_bytes());
    resp.extend_from_slice(&0x0003u16.to_be_bytes());
    resp.push(unit_id);
    resp.push(func | 0x80);
    resp.push(code);
    resp
}
