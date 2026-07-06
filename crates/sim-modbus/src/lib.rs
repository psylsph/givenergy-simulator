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

/// Determine the valid CT meter slave address range from the device type code
/// stored in the register store (HR 0 = ge_hr_device_type, composite key 10000).
/// Single-phase inverters have 1 CT clamp (slave 0x01); three-phase have 3.
/// This is read at runtime so the Modbus server adapts to plant changes.
pub fn meter_slaves_from_store(
    store: &sim_registers::RegisterStore,
) -> std::ops::RangeInclusive<u8> {
    // DTC is at holding register 0
    let dtc = store.read(0).unwrap_or(0x2001);
    let family = dtc >> 8;
    // Family 4x = ThreePhase, 6x = ACThreePhase → 3 CTs; all others → 1 CT
    if family == 0x40 || family == 0x60 {
        1..=3
    } else {
        1..=1
    }
}

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

/// Default inverter serial number (10 bytes, Latin-1, space-padded).
/// The outer envelope always carries the data-adapter (dongle) serial from
/// the client request. The inner response payload must carry the *inverter*
/// serial so the client can identify which inverter is responding.
pub const INVERTER_SERIAL: [u8; SERIAL_LEN] = *b"GE0000000 ";

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

/// Build a GivEnergy heartbeat request frame.
///
/// Real dongles send this ~every 3 min. The client echoes it back verbatim.
/// After 3 missed responses the dongle closes the TCP connection (~9 min).
///
/// Frame: `tx_id(0x5959) + proto(0x0001) + length(2) + unit_id(0x01) + func(0x01)`
pub fn build_heartbeat_request() -> Vec<u8> {
    let mut frame = Vec::with_capacity(8);
    frame.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    frame.extend_from_slice(&PROTOCOL_ID.to_be_bytes());
    let length: u16 = 2; // unit_id(1) + func(1)
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(UNIT_ID);
    frame.push(FUNC_HEARTBEAT);
    frame
}

/// Build a GivEnergy error response frame.
/// GivEnergy error responses MUST prepend the inverter serial (10 bytes) to
/// the exception code. Real clients parse the inner PDU payload expecting
/// serial(10) + exception(1) — omitting serial causes a struct underrun.
pub fn build_error_response(
    serial: &[u8; SERIAL_LEN],
    slave: u8,
    func: u8,
    exception: u8,
) -> Vec<u8> {
    // Inner payload uses INVERTER_SERIAL (not data-adapter serial)
    let mut payload = Vec::with_capacity(SERIAL_LEN + 1);
    payload.extend_from_slice(&INVERTER_SERIAL);
    payload.push(exception);
    build_response_with_padding(serial, slave, func | 0x80, &payload, 0x12)
}

// ---------------------------------------------------------------------------
// Server
// ---------------------------------------------------------------------------

/// Run a GivEnergy-compatible Modbus TCP server.
///
/// Listens for GivEnergy proprietary frames and responds in kind.
/// Supports multi-slave addressing for battery BMS modules:
/// - Slave 0x32: inverter registers + battery #1 BMS (IR 60-119)
/// - Slave 0x33-0x37: battery modules 2-6 BMS (IR 60-119) — LV protocol
/// - Slave 0xA0: HV BMS discovery (IR 60-64; IR 61 = number of BCUs)
/// - Slave 0x70: HV BCU cluster (IR 60-119; aggregates all modules)
/// - Slave 0x50-0x57: HV BMU per-module cells (IR 60-119; one per module)
///
/// CT meter slave addresses are determined at runtime from the DTC in the
/// register store (HR 0), so the server adapts to inverter type changes.
pub async fn run_modbus_server(
    addr: SocketAddr,
    register_store: std::sync::Arc<tokio::sync::Mutex<sim_registers::RegisterStore>>,
    command_tx: CommandSender,
    battery_state: std::sync::Arc<tokio::sync::Mutex<Vec<sim_models::BatteryState>>>,
    dongle_mode: std::sync::Arc<std::sync::Mutex<sim_models::DongleMisbehaviourMode>>,
) -> Result<(), Box<dyn std::error::Error>> {
    let listener = TcpListener::bind(addr).await?;
    tracing::info!("GivEnergy Modbus TCP server listening on {addr}");

    loop {
        let (mut stream, peer) = listener.accept().await?;
        tracing::info!("Modbus connection from {peer}");
        let store = register_store.clone();
        let cmd_tx = command_tx.clone();
        let batt_state = battery_state.clone();
        let dongle = dongle_mode.clone();

        tokio::spawn(async move {
            // DropConnection mode: close immediately without any response.
            if *dongle.lock().unwrap() == sim_models::DongleMisbehaviourMode::DropConnection {
                tracing::warn!("Dongle misbehaviour: dropping connection from {peer}");
                return;
            }

            let mut buf = [0u8; 512];
            let mut pending: Vec<u8> = Vec::new();
            // Stale-data cache: snapshot of register values from the first read.
            let stale_cache: std::sync::Arc<std::sync::Mutex<Option<Vec<u16>>>> =
                std::sync::Arc::new(std::sync::Mutex::new(None));
            // Heartbeat: real dongles send one ~every 3 min, close after 3
            // missed responses. We send every 3 minutes.
            let mut heartbeat_interval = tokio::time::interval(std::time::Duration::from_secs(180));
            heartbeat_interval.tick().await; // skip the immediate first tick
            let mut missed_heartbeats: u8 = 0;
            const MAX_MISSED_HEARTBEATS: u8 = 3;

            loop {
                tokio::select! {
                    read_result = stream.read(&mut buf) => {
                        let n = match read_result {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(e) => {
                                tracing::warn!("Modbus read error from {peer}: {e}");
                                break;
                            }
                        };

                        // Any data from client resets the missed-heartbeat counter
                        // (the client echoes our heartbeat back, which counts as data).
                        missed_heartbeats = 0;
                        pending.extend_from_slice(&buf[..n]);

                // Process complete frames
                loop {
                    // Need at least 8 bytes (5959 header + length + unit_id + func_id)
                    // to identify the frame type. Heartbeat frames are only 8 bytes;
                    // transparent-message frames are >= HEADER_SIZE (26 bytes).
                    if pending.len() < 8 {
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

                    // Handle heartbeat echo from client (func 0x01).
                    // Real dongles send heartbeat requests; clients echo them
                    // back verbatim. Just consume and move on — the missed-
                    // heartbeat counter is already reset by the data-ready path.
                    if func_id == FUNC_HEARTBEAT {
                        tracing::trace!("Received heartbeat echo from {peer}");
                        pending.drain(..frame_len);
                        continue;
                    }

                    // All other frames must be transparent messages (func 0x02)
                    // which have the full 26-byte header.
                    if func_id != FUNC_TRANSPARENT {
                        tracing::warn!("Invalid function ID 0x{func_id:02X}, dropping connection");
                        return;
                    }

                    if pending.len() < HEADER_SIZE {
                        break;
                    }

                    let frame = &pending[..frame_len];

                    // Extract data-adapter serial (bytes 8-17)
                    let mut serial = [b' '; SERIAL_LEN];
                    serial.copy_from_slice(&frame[8..8 + SERIAL_LEN]);

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

                            // Check if this is a CT clamp meter read (IR 60-89 on meter slaves).
                            // The valid slave range is determined at runtime from the inverter DTC
                            // in the register store, so it adapts to plant type changes.
                            let meter_slaves = {
                                let guard = store.lock().await;
                                meter_slaves_from_store(&guard)
                            };
                            let is_meter = meter_slaves.contains(&slave)
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
                                resp_payload.extend_from_slice(&INVERTER_SERIAL);
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

                            // If the slave is in the meter range (0x01-0x08) but NOT a
                            // valid CT meter for this inverter type, reject the request.
                            // Without this guard, the normal register read path below
                            // would serve CT data for slaves that shouldn't exist.
                            if (1..=8).contains(&slave) && !meter_slaves.contains(&slave) {
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

                            // Determine if this read targets a battery device.
                            // LV BMS protocol: slaves 0x32-0x37 (IR 60-119 per module).
                            // HV cluster protocol: slave 0xA0 (discovery),
                            //   0x70+i (BCU cluster), 0x50+m (BMU per-module cells).
                            // HV inverters (DTC family 4/8) are discovered via the
                            // cluster path; LV inverters via 0x32-0x37. Both are
                            // served unconditionally — clients only probe the path
                            // matching the inverter's DTC.
                            enum BatteryRead {
                                LvBms(usize),
                                HvBmsDiscovery,
                                HvBcu,
                                HvBmu(usize),
                            }
                            let battery_read = if inner_func == FC_READ_INPUT {
                                if slave == 0xA0 && (60..65).contains(&start_addr) {
                                    Some(BatteryRead::HvBmsDiscovery)
                                } else if slave == 0x70 && (60..120).contains(&start_addr) {
                                    Some(BatteryRead::HvBcu)
                                } else if (0x50..=0x6F).contains(&slave)
                                    && (60..120).contains(&start_addr)
                                {
                                    Some(BatteryRead::HvBmu((slave - 0x50) as usize))
                                } else if slave == 0x32 && (60..120).contains(&start_addr) {
                                    Some(BatteryRead::LvBms(0usize))
                                } else if (0x33..=0x37).contains(&slave)
                                    && (60..120).contains(&start_addr)
                                {
                                    Some(BatteryRead::LvBms((slave - 0x33 + 1) as usize))
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            // For Input-register reads into the gateway aggregation bank
                            // (IR 1600-1859), reject entirely unmapped sub-ranges with an
                            // error so client bounds-checks behave — zero-filled unmapped
                            // banks are indistinguishable from "no gateway present".
                            // Mixed ranges (some mapped, some gaps) still get zeros for gaps.
                            if inner_func == FC_READ_INPUT
                                && (1600..1860).contains(&start_addr)
                            {
                                let store_guard = store.lock().await;
                                let has_defs = store_guard.has_any_def_in_range(
                                    start_addr,
                                    count,
                                    sim_registers::RegisterSpace::Input,
                                );
                                drop(store_guard);
                                if !has_defs {
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
                            }

                            // Some holding-register banks are model-specific. If the
                            // configured device lacks the bank, return an exception
                            // instead of phantom zeros so clients fall back to the right
                            // capability set. Examples: HR 1000-1124 only exists on
                            // three-phase inverters; HR 318-320 battery pause is absent
                            // on single-phase AC Coupled inverters per giv_tcp Model.AC.
                            if inner_func == FC_READ_HOLDING {
                                let absent = store
                                    .lock()
                                    .await
                                    .holding_range_absent_for_device(start_addr, count);
                                if absent {
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
                            }

                            let reg_data = if let Some(br) = battery_read {
                                let batts = batt_state.lock().await;
                                let store_guard = store.lock().await;
                                // Single-stack model: one BCU manages all modules.
                                let src: Vec<u16> = match br {
                                    BatteryRead::LvBms(idx) => match batts.get(idx) {
                                        Some(b) => store_guard.project_battery_bms(b, idx).to_vec(),
                                        None => vec![0u16; 60],
                                    },
                                    BatteryRead::HvBmsDiscovery => {
                                        store_guard.project_battery_bms_discovery(1).to_vec()
                                    }
                                    BatteryRead::HvBcu => {
                                        store_guard.project_battery_bcu(&batts).to_vec()
                                    }
                                    BatteryRead::HvBmu(idx) => match batts.get(idx) {
                                        Some(b) => store_guard.project_battery_bmu(b, idx).to_vec(),
                                        None => vec![0u16; 60],
                                    },
                                };
                                drop(store_guard);
                                let mut data = Vec::with_capacity(count as usize * 2);
                                for i in 0..count {
                                    let idx = start_addr.saturating_sub(60) as usize + i as usize;
                                    let val = src.get(idx).copied().unwrap_or(0);
                                    data.extend_from_slice(&val.to_be_bytes());
                                }
                                data
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

                            // Apply dongle misbehaviour mode to the register data.
                            let reg_data = {
                                let mode = *dongle.lock().unwrap();
                                match mode {
                                    sim_models::DongleMisbehaviourMode::Off => reg_data,
                                    sim_models::DongleMisbehaviourMode::EmptyData => {
                                        vec![0u8; count as usize * 2]
                                    }
                                    sim_models::DongleMisbehaviourMode::GarbageData => {
                                        let mut garbage = Vec::with_capacity(count as usize * 2);
                                        for _ in 0..count {
                                            let val: u16 = fastrand::u16(..);
                                            garbage.extend_from_slice(&val.to_be_bytes());
                                        }
                                        garbage
                                    }
                                    sim_models::DongleMisbehaviourMode::Intermittent => {
                                        if fastrand::bool() {
                                            vec![0u8; count as usize * 2]
                                        } else {
                                            reg_data
                                        }
                                    }
                                    sim_models::DongleMisbehaviourMode::StaleData => {
                                        // Check if we already have a snapshot (lock is
                                        // dropped before any .await).
                                        let has_snapshot = {
                                            let cache = stale_cache.lock().unwrap();
                                            cache.is_some()
                                        };
                                        if has_snapshot {
                                            let cache = stale_cache.lock().unwrap();
                                            let snapshot = cache.as_ref().unwrap();
                                            let mut stale = Vec::with_capacity(count as usize * 2);
                                            for i in 0..count {
                                                let idx = start_addr as usize + i as usize;
                                                let val = snapshot.get(idx).copied().unwrap_or(0);
                                                stale.extend_from_slice(&val.to_be_bytes());
                                            }
                                            stale
                                        } else {
                                            // First read: snapshot the entire register space.
                                            let snapshot: Vec<u16> = {
                                                let store_guard = store.lock().await;
                                                let mut snap = vec![0u16; 10000];
                                                for addr in 0..10000u16 {
                                                    if let Some(v) = store_guard.read(addr) {
                                                        snap[addr as usize] = v;
                                                    }
                                                }
                                                snap
                                            };
                                            let mut cache = stale_cache.lock().unwrap();
                                            *cache = Some(snapshot);
                                            reg_data
                                        }
                                    }
                                    // DropConnection is handled at connection accept time.
                                    sim_models::DongleMisbehaviourMode::DropConnection => {
                                        // Should never reach here, but be safe.
                                        reg_data
                                    }
                                }
                            };

                            // Build read response payload:
                            // serial(10) + base_register(2) + register_count(2) + data(N×2)
                            // Inner payload uses INVERTER_SERIAL, not data-adapter serial
                            let mut resp_payload =
                                Vec::with_capacity(SERIAL_LEN + 4 + reg_data.len());
                            resp_payload.extend_from_slice(&INVERTER_SERIAL);
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
                                // Inner payload uses INVERTER_SERIAL, not data-adapter serial
                                let resp_payload = {
                                    let mut p = Vec::with_capacity(SERIAL_LEN + 4);
                                    p.extend_from_slice(&INVERTER_SERIAL);
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

                    _ = heartbeat_interval.tick() => {
                        // Send heartbeat request like a real dongle.
                        // Client should echo it back, resetting missed_heartbeats.
                        let hb = build_heartbeat_request();
                        if stream.write_all(&hb).await.is_err() {
                            tracing::warn!("Heartbeat send failed for {peer}, closing");
                            break;
                        }
                        missed_heartbeats += 1;
                        if missed_heartbeats >= MAX_MISSED_HEARTBEATS {
                            tracing::warn!(
                                "{MAX_MISSED_HEARTBEATS} heartbeats unanswered for {peer}, closing"
                            );
                            break;
                        }
                        tracing::trace!("Sent heartbeat to {peer} (missed={missed_heartbeats})");
                    }
                } // end select!
            } // end main loop
        });
    }
}

// ---------------------------------------------------------------------------
// Standard Modbus TCP server for GivEVC (Electric Vehicle Charger)
// ---------------------------------------------------------------------------
// The EVC uses STANDARD Modbus TCP (NOT the proprietary GivEnergy framing).
// It serves HR 0-114 (115 holding registers) on a configurable port
// (default 5020 in the simulator; real GivEVC hardware uses port 502).
//
// The server accepts a port parameter and auto-falls back to 5020 if
// binding to a privileged port (< 1024) fails with EACCES.
//
// Override with `GIVSIM_EVC_PORT=<port>` env var, or grant the capability:
//   sudo setcap 'cap_net_bind_service=+ep' target/debug/sim-tauri
//
// Register map matches GivTCP evc.py / EVCLut.evc_lut.
// Two standard reads cover the full map:
//   read_holding_registers(0, 60)   // HR 0-59
//   read_holding_registers(60, 55)  // HR 60-114

const EVC_REG_COUNT: u16 = 115;

fn evc_state_to_registers(evc: &sim_models::EvcState) -> Vec<u16> {
    let mut regs = vec![0u16; EVC_REG_COUNT as usize];

    // HR 0: Charging_State enum
    regs[0] = evc.charging_state;
    // HR 2: Connection_Status (0=Not Connected, 1=Connected)
    regs[2] = evc.connection_status;
    // HR 4: Error_Code
    regs[4] = evc.error_code;
    // HR 6: Current_L1 (÷10 Amps)
    regs[6] = evc.current_l1 as u16;
    // HR 8: Current_L2 (÷10 Amps)
    regs[8] = evc.current_l2 as u16;
    // HR 10: Current_L3 (÷10 Amps)
    regs[10] = evc.current_l3 as u16;
    // HR 13: Active_Power (Watts)
    regs[13] = evc.active_power_w as u16;
    // HR 17: Active_Power_L1 (Watts)
    regs[17] = evc.active_power_l1 as u16;
    // HR 20: Active_Power_L2 (Watts)
    regs[20] = evc.active_power_l2 as u16;
    // HR 24: Active_Power_L3 (Watts)
    regs[24] = evc.active_power_l3 as u16;
    // HR 29: Meter_Energy (÷10 kWh)
    regs[29] = (evc.meter_energy_kwh * 10.0) as u16;
    // HR 32: Evse_Max_Current (Amps)
    regs[32] = evc.evse_max_current;
    // HR 34: Evse_Min_Current (Amps)
    regs[34] = evc.evse_min_current;
    // HR 36: Charge_Limit (÷10 Amps)
    regs[36] = (evc.charge_limit * 10.0) as u16;
    // HR 38-68: Serial_Number (ASCII, each register = one char, stop at 0)
    let serial_bytes = evc.serial_number.as_bytes();
    for (i, &b) in serial_bytes.iter().take(31).enumerate() {
        regs[38 + i] = b as u16;
    }
    // HR 72: Charge_Session_Energy (÷10 kWh, raw = kWh×10)
    regs[72] = (evc.session_energy_kwh * 10.0) as u16;
    // HR 79: Charge_Session_Duration (seconds)
    regs[79] = (evc.session_duration_secs & 0xFFFF) as u16;
    // HR 93: Plug_and_Go (0=enable, 1=disable)
    regs[93] = evc.plug_and_go;
    // HR 94: Charge_Control (0=Ready, 1=Start, 2=Stop)
    regs[94] = evc.charge_control;
    // HR 109: Voltage_L1 (÷10 V)
    regs[109] = (evc.voltage_l1 * 10.0) as u16;
    // HR 111: Voltage_L2 (÷10 V)
    regs[111] = (evc.voltage_l2 * 10.0) as u16;
    // HR 113: Voltage_L3 (÷10 V)
    regs[113] = (evc.voltage_l3 * 10.0) as u16;

    regs
}

/// Attempt to bind a TCP listener to the given port.
/// Returns `Ok(listener)` on success, or `Err((errno, msg))` on failure.
async fn try_bind(port: u16) -> Result<tokio::net::TcpListener, (i32, String)> {
    let addr: std::net::SocketAddr = format!("0.0.0.0:{port}").parse().unwrap();
    match tokio::net::TcpListener::bind(addr).await {
        Ok(listener) => Ok(listener),
        Err(e) => {
            let errno = e.raw_os_error().unwrap_or(0);
            Err((errno, e.to_string()))
        }
    }
}

fn registers_to_evc(addr: u16, value: u16, evc: &mut sim_models::EvcState) {
    match addr {
        // HR 91: Charge current limit (raw, ×10 deci-Amps)
        91 => evc.charge_current_limit = value.max(60), // min 6.0A
        // HR 93: Plug and Go (0=enable, 1=disable)
        93 => evc.plug_and_go = value.min(1),
        // HR 95: Charge control (0=Ready, 1=Start, 2=Stop)
        // Note: writes to HR 95 map to charge_control in our state
        // (GivTCP uses HR 94 as read, HR 95 as write for charge_control)
        95 => evc.charge_control = value.min(2),
        // HR 97-102: System time — ignored in simulation (time is simulated)
        97..=102 => { /* time writes ignored */ }
        _ => {}
    }
}

/// Run a standard Modbus TCP server for the EVC on the given port.
///
/// If binding fails with EACCES (port < 1024 without CAP_NET_BIND_SERVICE),
/// automatically falls back to 5020 with a warning.
pub async fn run_evc_modbus_server(
    evc_state: std::sync::Arc<tokio::sync::Mutex<sim_models::EvcState>>,
    port: u16,
) -> Result<(), Box<dyn std::error::Error>> {
    // Try the configured port. If EACCES (privileged port without
    // CAP_NET_BIND_SERVICE), auto-fallback to 5020.
    let listener = match try_bind(port).await {
        Ok(l) => l,
        Err((errno, msg)) => {
            if errno == 13 && port < 1024 && std::env::var("GIVSIM_EVC_PORT").ok().is_none() {
                tracing::warn!(
                    "Cannot bind to port {port} (EACCES: {msg}), \
                     falling back to 5020. Set GIVSIM_EVC_PORT to override, \
                     or run: sudo setcap cap_net_bind_service=+ep target/debug/sim-tauri"
                );
                try_bind(5020)
                    .await
                    .map_err(|(_, m)| format!("Fallback bind failed: {m}"))?
            } else {
                return Err(format!("Cannot bind to port {port}: {msg} (errno={errno})").into());
            }
        }
    };

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
                        if count == 0 || count > EVC_REG_COUNT || start + count > EVC_REG_COUNT {
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
                        let payload_len = 3 + count as usize * 2;
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
                        if addr_r >= EVC_REG_COUNT {
                            let resp = build_evc_error(trans_id, unit_id, func, 0x02);
                            let _ = stream.write_all(&resp).await;
                            continue;
                        }
                        {
                            let mut guard = evc.lock().await;
                            registers_to_evc(addr_r, value, &mut guard);
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
                        if count == 0 || addr_r + count > EVC_REG_COUNT {
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
                                    registers_to_evc(addr_r + i as u16, v, &mut guard);
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

#[cfg(test)]
mod tests {
    use super::evc_state_to_registers;
    use sim_models::EvcState;

    fn evc_with_session_energy(kwh: f64) -> EvcState {
        EvcState {
            session_energy_kwh: kwh,
            ..Default::default()
        }
    }

    #[test]
    fn hr72_session_energy_is_deci_kwh_times_10() {
        // HR 72 must be encoded as kWh×10 (deci-kWh) on the wire so that
        // clients which decode HR 72 as value÷10 (GivTCP / HEM) read the
        // correct session energy: 12.7 kWh → raw 127.
        let regs = evc_state_to_registers(&evc_with_session_energy(12.7));
        assert_eq!(regs[72], 127, "12.7 kWh must project to HR72 = 127 (÷10)");
    }

    #[test]
    fn hr72_session_energy_resolution_and_truncation() {
        // Fresh session reads 0.
        assert_eq!(evc_state_to_registers(&evc_with_session_energy(0.0))[72], 0);
        // Sub-kWh resolution survives: 0.1 kWh → raw 1 (the point of ×10).
        assert_eq!(evc_state_to_registers(&evc_with_session_energy(0.1))[72], 1);
        // Deci-kWh truncation: 12.749 × 10 = 127.49 → as u16 → 127.
        assert_eq!(
            evc_state_to_registers(&evc_with_session_energy(12.749))[72],
            127
        );
    }

    #[test]
    fn hr72_matches_sibling_deci_scales() {
        // HR 72 (×10) follows the same deci encoding as HR 29 (meter energy)
        // and HR 36 (charge limit), confirming the convention is uniform
        // across EVC energy/current registers.
        let evc = EvcState {
            session_energy_kwh: 12.7,
            meter_energy_kwh: 12.7,
            charge_limit: 32.0,
            ..Default::default()
        };
        let regs = evc_state_to_registers(&evc);
        assert_eq!(regs[72], 127); // session energy ×10
        assert_eq!(regs[29], 127); // meter energy ×10
        assert_eq!(regs[36], 320); // charge limit ×10
    }
}
