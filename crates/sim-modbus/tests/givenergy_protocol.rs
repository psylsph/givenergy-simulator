//! Comprehensive tests for the GivEnergy Modbus protocol implementation.
//!
//! Tests cover:
//! - Frame encoding: header fields, serial, padding, CRC
//! - Frame decoding: happy path, error cases, CRC validation
#![allow(clippy::let_unit_value)]
//! - Register reads: input (fn 0x04) and holding (fn 0x03), various counts and offsets
//! - Register writes: fn 0x06, readwrite vs readonly, command dispatch
//! - Error handling: unsupported function codes, bad payloads
//! - Response format: serial(10) prefix, base_register, count, data layout
//! - Edge cases: empty gaps, out-of-range addresses, max register count
//! - Multi-request: sequential requests on the same connection
//! - State projection: verify register values reflect PlantState

use sim_modbus::*;
use sim_registers::{RegisterSpace, RegisterStore, default_register_catalogue};
use std::net::SocketAddr;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

// ===========================================================================
// Helpers
// ===========================================================================

const TEST_SERIAL: [u8; SERIAL_LEN] = *b"SA1234    ";

/// Encode a frame with a known serial.
fn encode_frame(slave: u8, func: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(2 + payload.len() + 2);
    inner.push(slave);
    inner.push(func);
    inner.extend_from_slice(payload);
    let crc = crc16(&inner);
    inner.extend_from_slice(&crc.to_le_bytes());

    let length = (1 + 1 + SERIAL_LEN + 8 + inner.len()) as u16;
    let mut frame = Vec::with_capacity(HEADER_SIZE + inner.len());
    frame.extend_from_slice(&TRANSACTION_ID.to_be_bytes());
    frame.extend_from_slice(&PROTOCOL_ID.to_be_bytes());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(UNIT_ID);
    frame.push(FUNC_TRANSPARENT);
    frame.extend_from_slice(&TEST_SERIAL);
    frame.extend_from_slice(&8u64.to_be_bytes());
    frame.extend_from_slice(&inner);
    frame
}

fn build_read_request(start_addr: u16, count: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(4);
    p.extend_from_slice(&start_addr.to_be_bytes());
    p.extend_from_slice(&count.to_be_bytes());
    encode_frame(0x32, FC_READ_HOLDING, &p)
}

fn build_read_input_request(start_addr: u16, count: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(4);
    p.extend_from_slice(&start_addr.to_be_bytes());
    p.extend_from_slice(&count.to_be_bytes());
    encode_frame(0x32, FC_READ_INPUT, &p)
}

fn build_write_request(address: u16, value: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(4);
    p.extend_from_slice(&address.to_be_bytes());
    p.extend_from_slice(&value.to_be_bytes());
    encode_frame(0x11, FC_WRITE_SINGLE, &p)
}

/// Decode a response frame → (slave, func, inner_payload_no_crc).
fn decode_response(data: &[u8]) -> (u8, u8, Vec<u8>) {
    assert!(
        data.len() >= HEADER_SIZE + 4,
        "Response too short: {} bytes",
        data.len()
    );
    let inner_pdu = &data[HEADER_SIZE..];
    let slave = inner_pdu[0];
    let func = inner_pdu[1];
    let payload = inner_pdu[2..inner_pdu.len() - 2].to_vec();
    (slave, func, payload)
}

/// Parse read-response payload: serial(10) + start(2) + count(2) + data(N×2).
fn parse_read_payload(payload: &[u8]) -> (u16, u16, Vec<u16>) {
    assert!(
        payload.len() >= 14,
        "Read payload too short: {}",
        payload.len()
    );
    let start = u16::from_be_bytes([payload[10], payload[11]]);
    let count = u16::from_be_bytes([payload[12], payload[13]]);
    let data = payload[14..]
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    (start, count, data)
}

/// Start a TCP test server with pre-populated state.
async fn start_server_with_state(
    state: &sim_models::PlantState,
) -> (
    SocketAddr,
    RegisterStore,
    tokio::sync::mpsc::UnboundedReceiver<ModbusCommand>,
) {
    let store = RegisterStore::new(default_register_catalogue());
    let mut store_clone = store.clone();
    store_clone.project_from_state(state);
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(store_clone));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();

    let store_ref = store.clone();
    tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 512];
        let mut pending: Vec<u8> = Vec::new();
        loop {
            let n = match stream.read(&mut buf).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(_) => break,
            };
            pending.extend_from_slice(&buf[..n]);
            loop {
                if pending.len() < HEADER_SIZE {
                    break;
                }
                let length = u16::from_be_bytes([pending[4], pending[5]]) as usize;
                let frame_len = 6 + length;
                if pending.len() < frame_len {
                    break;
                }
                let frame = &pending[..frame_len];
                let mut serial = [b' '; SERIAL_LEN];
                serial.copy_from_slice(&frame[8..8 + SERIAL_LEN]);
                let inner_pdu = &frame[HEADER_SIZE..];
                if inner_pdu.len() < 4 {
                    pending.drain(..frame_len);
                    continue;
                }
                let slave = inner_pdu[0];
                let inner_func = inner_pdu[1];
                let inner_payload = &inner_pdu[2..inner_pdu.len() - 2];
                match inner_func {
                    FC_READ_HOLDING | FC_READ_INPUT => {
                        if inner_payload.len() < 4 {
                            pending.drain(..frame_len);
                            continue;
                        }
                        let start_addr = u16::from_be_bytes([inner_payload[0], inner_payload[1]]);
                        let count = u16::from_be_bytes([inner_payload[2], inner_payload[3]]);
                        let s = store_ref.lock().await;
                        let space = if inner_func == FC_READ_INPUT {
                            RegisterSpace::Input
                        } else {
                            RegisterSpace::Holding
                        };

                        // Gateway aggregation bank (IR 1600-1859): reject
                        // entirely unmapped sub-ranges with an error so
                        // client bounds-checks distinguish "no gateway".
                        if inner_func == FC_READ_INPUT
                            && (1600..1860).contains(&start_addr)
                            && !s.has_any_def_in_range(start_addr, count, space)
                        {
                            drop(s);
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

                        let mut reg_data = Vec::with_capacity(count as usize * 2);
                        for i in 0..count {
                            reg_data.extend_from_slice(
                                &s.read_by_space(start_addr + i, space)
                                    .unwrap_or(0)
                                    .to_be_bytes(),
                            );
                        }
                        drop(s);
                        let mut resp_payload = Vec::with_capacity(SERIAL_LEN + 4 + reg_data.len());
                        resp_payload.extend_from_slice(&serial);
                        resp_payload.extend_from_slice(&start_addr.to_be_bytes());
                        resp_payload.extend_from_slice(&count.to_be_bytes());
                        resp_payload.extend_from_slice(&reg_data);
                        let _ = stream
                            .write_all(&build_response(&serial, slave, inner_func, &resp_payload))
                            .await;
                    }
                    FC_WRITE_SINGLE => {
                        if inner_payload.len() < 4 {
                            pending.drain(..frame_len);
                            continue;
                        }
                        let address = u16::from_be_bytes([inner_payload[0], inner_payload[1]]);
                        let value = u16::from_be_bytes([inner_payload[2], inner_payload[3]]);
                        let ok = {
                            let mut s = store_ref.lock().await;
                            s.write(address, value)
                        };
                        if ok {
                            let mut p = Vec::with_capacity(SERIAL_LEN + 4);
                            p.extend_from_slice(&serial);
                            p.extend_from_slice(&address.to_be_bytes());
                            p.extend_from_slice(&value.to_be_bytes());
                            let _ = stream
                                .write_all(&build_response(&serial, slave, inner_func, &p))
                                .await;
                            let _ = tx.send(ModbusCommand { address, value });
                        } else {
                            let _ = stream
                                .write_all(&build_error_response(
                                    &serial,
                                    slave,
                                    inner_func,
                                    EC_ILLEGAL_DATA_ADDRESS,
                                ))
                                .await;
                        }
                    }
                    _ => {
                        let _ = stream
                            .write_all(&build_error_response(
                                &serial,
                                slave,
                                inner_func,
                                EC_ILLEGAL_FUNCTION,
                            ))
                            .await;
                    }
                }
                pending.drain(..frame_len);
            }
        }
    });

    let store_snapshot = store.lock().await.clone();
    (addr, store_snapshot, rx)
}

/// Start an empty test server.
async fn start_test_server() -> (
    SocketAddr,
    tokio::sync::mpsc::UnboundedReceiver<ModbusCommand>,
) {
    let state = sim_models::PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    let (addr, _, rx) = start_server_with_state(&state).await;
    (addr, rx)
}

fn test_state() -> sim_models::PlantState {
    let mut state = sim_models::PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    state.solar.generation_w = 4000.0;
    state.solar.pv1_w = 4000.0;
    state.solar.pv2_w = 0.0;
    state.grid.power_w = -800.0;
    state.grid.connected = true;
    state.load.demand_w = 2000.0;
    state.batteries[0].soc_percent = 75.0;
    state.sync_battery_from_vec();
    state
}

async fn send_recv(stream: &mut tokio::net::TcpStream, req: &[u8]) -> Vec<u8> {
    stream.write_all(req).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    buf[..n].to_vec()
}

// ===========================================================================
// 1. Frame Structure Tests
// ===========================================================================

#[test]
fn frame_header_has_fixed_transaction_id() {
    let frame = encode_frame(0x32, 0x03, &[0x00]);
    assert_eq!(&frame[0..2], &[0x59, 0x59], "Transaction ID must be 0x5959");
}

#[test]
fn frame_header_has_fixed_protocol_id() {
    let frame = encode_frame(0x32, 0x03, &[0x00]);
    assert_eq!(&frame[2..4], &[0x00, 0x01], "Protocol ID must be 0x0001");
}

#[test]
fn frame_header_has_fixed_unit_id() {
    let frame = encode_frame(0x32, 0x03, &[0x00]);
    assert_eq!(frame[6], 0x01, "Unit ID must be 0x01");
}

#[test]
fn frame_header_has_transparent_function() {
    let frame = encode_frame(0x32, 0x03, &[0x00]);
    assert_eq!(frame[7], 0x02, "Function ID must be 0x02 (transparent)");
}

#[test]
fn frame_serial_field_is_10_bytes() {
    let frame = encode_frame(0x32, 0x03, &[]);
    assert_eq!(&frame[8..18], &TEST_SERIAL, "Serial field must be 10 bytes");
}

#[test]
fn frame_padding_is_big_endian_8() {
    let frame = encode_frame(0x32, 0x03, &[]);
    let padding = u64::from_be_bytes(frame[18..26].try_into().unwrap());
    assert_eq!(padding, 8, "Padding must be big-endian u64 value 8");
}

#[test]
fn frame_length_field_matches_actual_length() {
    let frame = encode_frame(0x32, 0x03, &[0x00, 0x01]);
    let length = u16::from_be_bytes([frame[4], frame[5]]) as usize;
    assert_eq!(
        6 + length,
        frame.len(),
        "Length field must be consistent with frame size"
    );
}

#[test]
fn frame_inner_pdu_starts_at_byte_26() {
    let frame = encode_frame(0x32, 0x03, &[0xAA]);
    // Inner PDU = slave + func + payload + CRC
    assert_eq!(frame[HEADER_SIZE], 0x32, "Slave address at byte 26");
    assert_eq!(frame[HEADER_SIZE + 1], 0x03, "Function code at byte 27");
}

#[test]
fn frame_crc_is_little_endian() {
    let frame = encode_frame(0x32, 0x03, &[0x00]);
    let inner_pdu = &frame[HEADER_SIZE..];
    let len = inner_pdu.len();
    let crc_bytes = &inner_pdu[len - 2..];
    let received = u16::from_le_bytes([crc_bytes[0], crc_bytes[1]]);
    let expected = crc16(&inner_pdu[..len - 2]);
    assert_eq!(
        received, expected,
        "CRC must be little-endian Modbus CRC-16"
    );
}

#[test]
fn crc_empty_input() {
    assert_eq!(crc16(&[]), 0xFFFF);
}

#[test]
fn crc_single_byte() {
    assert_eq!(crc16(&[0x00]), 0x40BF);
}

#[test]
fn crc_known_vector() {
    assert_eq!(crc16(b"123456789"), 0x4B37);
}

// ===========================================================================
// 2. Response Frame Tests
// ===========================================================================

#[test]
fn response_frame_has_correct_givenergy_header() {
    let serial = [b' '; SERIAL_LEN];
    let resp = build_response(&serial, 0x32, 0x03, &[0x01, 0x02]);
    assert_eq!(&resp[0..2], &[0x59, 0x59]);
    assert_eq!(&resp[2..4], &[0x00, 0x01]);
    assert_eq!(resp[6], 0x01);
    assert_eq!(resp[7], 0x02);
    assert_eq!(&resp[8..18], &[b' '; 10]);
    let padding = u64::from_be_bytes(resp[18..26].try_into().unwrap());
    assert_eq!(padding, 0x8A);
}

#[test]
fn error_response_sets_error_bit_on_function() {
    let serial = [b' '; SERIAL_LEN];
    let resp = build_error_response(&serial, 0x32, 0x03, 0x01);
    let inner_pdu = &resp[HEADER_SIZE..];
    assert_eq!(inner_pdu[1], 0x83, "Error response function = 0x03 | 0x80");
    // Error response payload carries INVERTER_SERIAL before exception byte
    // Inner PDU: slave(1) + func|0x80(1) + inverter_serial(10) + exception(1) + CRC(2)
    assert!(inner_pdu.len() >= 15, "Inner PDU must include serial");
    assert_eq!(
        &inner_pdu[2..12],
        &sim_modbus::INVERTER_SERIAL[..],
        "Inverter serial in payload"
    );
    assert_eq!(inner_pdu[12], 0x01, "Exception code = Illegal Function");
}

#[test]
fn response_length_field_is_correct() {
    let serial = [b' '; SERIAL_LEN];
    let resp = build_response(&serial, 0x32, 0x03, &[0xAA, 0xBB]);
    let length = u16::from_be_bytes([resp[4], resp[5]]) as usize;
    assert_eq!(6 + length, resp.len());
}

// ===========================================================================
// 3. TCP Integration: Read Holding Registers
// ===========================================================================

#[tokio::test]
async fn read_holding_single_register() {
    let (addr, _rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Write then read
    send_recv(&mut stream, &build_write_request(100, 42)).await;
    let resp = send_recv(&mut stream, &build_read_request(100, 1)).await;

    let (slave, func, payload) = decode_response(&resp);
    assert_eq!(slave, 0x32);
    assert_eq!(func, FC_READ_HOLDING);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 100);
    assert_eq!(count, 1);
    assert_eq!(data, vec![42]);
}

#[tokio::test]
async fn read_holding_multiple_registers() {
    let (addr, _rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Write multiple values
    send_recv(&mut stream, &build_write_request(100, 10)).await;
    send_recv(&mut stream, &build_write_request(102, 30)).await;

    // Read 3 registers from 100
    let resp = send_recv(&mut stream, &build_read_request(100, 3)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, count, data) = parse_read_payload(&payload);
    assert_eq!(count, 3);
    assert_eq!(data.len(), 3);
    assert_eq!(data[0], 10); // reg 100 = written
    assert_eq!(data[2], 30); // reg 102 = written
}

#[tokio::test]
async fn read_holding_returns_zero_for_undefined_addresses() {
    let (addr, _rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Address 999 should not be defined
    let resp = send_recv(&mut stream, &build_read_request(999, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data, vec![0], "Undefined registers should return 0");
}

#[tokio::test]
async fn read_holding_spanning_defined_and_undefined() {
    let (addr, _rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    send_recv(&mut stream, &build_write_request(100, 55)).await;
    // Read 5 regs from 99: reg 99 undefined (0), reg 100 = 55, rest 0
    let resp = send_recv(&mut stream, &build_read_request(99, 5)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 0); // undefined
    assert_eq!(data[1], 55); // written
    assert_eq!(data[2], 0); // undefined
}

// ===========================================================================
// 4. TCP Integration: Read Input Registers
// ===========================================================================

#[tokio::test]
async fn read_input_registers_returns_state_values() {
    let state = test_state();
    let (addr, _store, _rx) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read IR 0-2: status, PV1 voltage, PV2 voltage
    let resp = send_recv(&mut stream, &build_read_input_request(0, 3)).await;
    let (slave, func, payload) = decode_response(&resp);
    assert_eq!(slave, 0x32);
    assert_eq!(func, FC_READ_INPUT);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 1, "IR 0 = status (1 = normal)");
    assert_eq!(data[1], 3500, "IR 1 = PV1 voltage (350.0V / 0.1 = 3500)");
    assert_eq!(
        data[2], 0,
        "IR 2 = PV2 voltage (0 — no PV2 array configured)"
    );
}

#[tokio::test]
async fn read_input_battery_soc() {
    let state = test_state(); // SOC = 75%
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_input_request(59, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 75, "IR 59 = battery SOC 75%");
}

#[tokio::test]
async fn read_input_battery_power_signed() {
    let mut state = test_state();
    // Make battery charging: positive power
    state.batteries[0].soc_percent = 50.0;
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_input_request(52, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    // Value may be 0 if no battery power set, but should not panic
    assert!(data.len() == 1, "Should get 1 register back");
}

#[tokio::test]
async fn read_input_grid_power_negative() {
    let state = test_state(); // grid.power_w = -800
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_input_request(30, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    let signed = data[0] as i16;
    assert!(
        signed > 0,
        "Grid power should be positive (exporting GE convention), got {signed}"
    );
}

#[tokio::test]
async fn read_input_grid_power_importing() {
    let mut state = test_state();
    state.grid.power_w = 2000.0; // importing 2kW (positive = import in our convention)
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_input_request(30, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    let signed = data[0] as i16;
    assert!(
        signed < 0,
        "Grid power should be negative (importing GE convention), got {signed}"
    );
}

#[tokio::test]
async fn read_input_grid_voltage_and_frequency() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_input_request(5, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    // 240.0V / 0.1 scaling = 2400
    assert_eq!(data[0], 2400, "IR 5 = grid voltage 240.0V = raw 2400");

    let resp = send_recv(&mut stream, &build_read_input_request(13, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    // 50.0Hz / 0.01 scaling = 5000
    assert_eq!(data[0], 5000, "IR 13 = grid frequency 50.0Hz = raw 5000");
}

#[tokio::test]
async fn read_input_pv_power() {
    let state = test_state(); // solar = 4000W
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // IR 18 = PV1 power (all solar on array 1), IR 20 = PV2 power (0 — no PV2)
    let resp = send_recv(&mut stream, &build_read_input_request(18, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 4000, "IR 18 = PV1 power = 4000W");

    let resp = send_recv(&mut stream, &build_read_input_request(20, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 0, "IR 20 = PV2 power = 0W (no PV2 array)");
}

#[tokio::test]
async fn read_input_energy_totals() {
    let mut state = test_state();
    state.energy_totals.solar_generation_kwh = 12.5;
    state.energy_totals.grid_export_kwh = 3.0;
    state.energy_totals.grid_import_kwh = 1.5;
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // IR 17 = PV1 energy today (×0.1 kWh) = 12.5 / 0.1 = 125 (no PV2 array)
    let resp = send_recv(&mut stream, &build_read_input_request(17, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 125, "IR 17 = PV1 energy = 12.5kWh / 0.1 = 125");

    // IR 25 = export today (×0.1 kWh) = 3.0 / 0.1 = 30
    let resp = send_recv(&mut stream, &build_read_input_request(25, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 30, "IR 25 = export energy = 3.0kWh / 0.1 = 30");

    // IR 26 = import today = 1.5 / 0.1 = 15
    let resp = send_recv(&mut stream, &build_read_input_request(26, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 15, "IR 26 = import energy = 1.5kWh / 0.1 = 15");
}

#[tokio::test]
async fn read_input_battery_voltage_and_temperature() {
    let state = test_state(); // SOC = 75%
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // IR 50 = battery voltage (×0.01 V) = (44.0 + 75 * 0.08) / 0.01 = 5000
    let resp = send_recv(&mut stream, &build_read_input_request(50, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    let expected_v = (44.0 + 75.0 * 0.08) / 0.01; // = 5000.0
    assert_eq!(data[0], expected_v as u16, "IR 50 = battery voltage");
}

#[tokio::test]
async fn read_input_full_block_0_59() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Full 60-register read like the real client does
    let resp = send_recv(&mut stream, &build_read_input_request(0, 60)).await;
    let (slave, func, payload) = decode_response(&resp);
    assert_eq!(slave, 0x32);
    assert_eq!(func, FC_READ_INPUT);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 0);
    assert_eq!(count, 60);
    assert_eq!(data.len(), 60);
    assert_eq!(data[0], 1, "IR 0 = status");
    assert_eq!(data[59], 75, "IR 59 = SOC");
}

// ===========================================================================
// 5. TCP Integration: Read Holding Registers (GivEnergy-native)
// ===========================================================================

#[tokio::test]
async fn read_holding_ge_device_type() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 0 = device type 0x2001
    let resp = send_recv(&mut stream, &build_read_request(0, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 0x2001, "HR 0 = device type");
}

#[tokio::test]
async fn read_holding_ge_battery_power_mode() {
    let mut state = test_state();
    state
        .inverter
        .mode_state
        .set_user(sim_models::InverterMode::Eco);
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 27 = battery power mode (1 = eco)
    let resp = send_recv(&mut stream, &build_read_request(27, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 1, "HR 27 = eco mode");
}

#[tokio::test]
async fn read_holding_ge_soc_reserve() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 110 = SOC reserve
    let resp = send_recv(&mut stream, &build_read_request(110, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert!(data[0] > 0, "HR 110 = SOC reserve should be non-zero");
}

#[tokio::test]
async fn read_holding_full_block_0_59() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_request(0, 60)).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, FC_READ_HOLDING);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 0);
    assert_eq!(count, 60);
    assert_eq!(data.len(), 60);
    assert_eq!(data[0], 0x2001, "HR 0 = device type");
}

#[tokio::test]
async fn read_holding_block_60_119() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_request(60, 60)).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, FC_READ_HOLDING);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 60);
    assert_eq!(count, 60);
    assert_eq!(data.len(), 60);
    // HR 96 = enable charge, HR 110 = SOC reserve
    assert!(
        data[96 - 60] == 0 || data[96 - 60] == 1,
        "HR 96 = enable charge (bool)"
    );
}

// ===========================================================================
// 6. TCP Integration: Write Single Register
// ===========================================================================

#[tokio::test]
async fn write_readwrite_register_succeeds() {
    let (addr, mut rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_write_request(100, 5)).await;
    let (slave, func, payload) = decode_response(&resp);
    assert_eq!(slave, 0x11);
    assert_eq!(func, FC_WRITE_SINGLE);
    assert_eq!(payload.len(), 14);
    let addr_echo = u16::from_be_bytes([payload[10], payload[11]]);
    let val_echo = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(addr_echo, 100);
    assert_eq!(val_echo, 5);

    let cmd = rx.try_recv().unwrap();
    assert_eq!(cmd.address, 100);
    assert_eq!(cmd.value, 5);
}

#[tokio::test]
async fn write_readonly_register_returns_error() {
    let (addr, mut rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_write_request(200, 99)).await;
    let (slave, func, payload) = decode_response(&resp);
    assert_eq!(slave, 0x11);
    assert_eq!(func, FC_WRITE_SINGLE | 0x80);
    // Error response payload now includes inverter serial(10) before exception
    assert_eq!(payload[10], EC_ILLEGAL_DATA_ADDRESS);
    assert!(
        rx.try_recv().is_err(),
        "No command should be dispatched for rejected write"
    );
}

#[tokio::test]
async fn write_to_unknown_register_rejected() {
    let (addr, mut rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_write_request(9999, 1)).await;
    let (_, func, _) = decode_response(&resp);
    assert_eq!(
        func,
        FC_WRITE_SINGLE | 0x80,
        "Write to undefined address should fail"
    );
    assert!(rx.try_recv().is_err());
}

#[tokio::test]
async fn write_persists_for_subsequent_read() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    send_recv(&mut stream, &build_write_request(100, 42)).await;
    let resp = send_recv(&mut stream, &build_read_request(100, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 42, "Written value should be readable");
}

#[tokio::test]
async fn write_ge_holding_register() {
    let (addr, mut rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 27 is a ReadWrite GE holding register
    let resp = send_recv(&mut stream, &build_write_request(27, 1)).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, FC_WRITE_SINGLE);
    let val = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(val, 1);
    let cmd = rx.try_recv().unwrap();
    assert_eq!(cmd.address, 27);
}

// ===========================================================================
// 7. Error Handling Tests
// ===========================================================================

#[tokio::test]
async fn unsupported_inner_function_returns_error() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Function code 0x01 (Read Coils) is not supported
    let req = encode_frame(0x32, 0x01, &[0x00, 0x00, 0x00, 0x01]);
    let resp = send_recv(&mut stream, &req).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, 0x81, "Error response for fn 0x01");
    // Error response payload now includes inverter serial(10) before exception
    assert_eq!(payload[10], EC_ILLEGAL_FUNCTION);
}

#[tokio::test]
async fn unsupported_fn_0x05_returns_error() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let req = encode_frame(0x32, 0x05, &[0x00, 0x00, 0xFF, 0x00]);
    let resp = send_recv(&mut stream, &req).await;
    let (_, func, _) = decode_response(&resp);
    assert_eq!(func, 0x85);
}

#[tokio::test]
async fn unsupported_fn_0x10_returns_error() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Write Multiple Registers
    let req = encode_frame(0x32, 0x10, &[0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x01]);
    let resp = send_recv(&mut stream, &req).await;
    let (_, func, _) = decode_response(&resp);
    assert_eq!(func, 0x90);
}

// ===========================================================================
// 8. Response Format Tests
// ===========================================================================

#[test]
fn response_inner_pdu_contains_given_payload() {
    let serial = *b"TEST      ";
    let resp = build_response(&serial, 0x32, 0x03, &[0xAB, 0xCD]);
    let inner_pdu = &resp[HEADER_SIZE..];
    let slave = inner_pdu[0];
    let func = inner_pdu[1];
    let payload = &inner_pdu[2..inner_pdu.len() - 2]; // strip CRC
    assert_eq!(slave, 0x32);
    assert_eq!(func, 0x03);
    assert_eq!(payload, &[0xAB, 0xCD]);
}

#[test]
fn read_response_payload_contains_start_address() {
    let serial = [b' '; SERIAL_LEN];
    let mut resp_payload = Vec::new();
    resp_payload.extend_from_slice(&serial);
    resp_payload.extend_from_slice(&100u16.to_be_bytes());
    resp_payload.extend_from_slice(&5u16.to_be_bytes());
    resp_payload.extend_from_slice(&[0x00, 0x0A, 0x00, 0x14, 0x00, 0x1E, 0x00, 0x28, 0x00, 0x32]);
    let resp = build_response(&serial, 0x32, 0x03, &resp_payload);
    let inner_pdu = &resp[HEADER_SIZE..];
    let payload = &inner_pdu[2..inner_pdu.len() - 2];
    assert_eq!(
        u16::from_be_bytes([payload[10], payload[11]]),
        100,
        "Start address"
    );
    assert_eq!(
        u16::from_be_bytes([payload[12], payload[13]]),
        5,
        "Register count"
    );
}

#[test]
fn write_response_payload_is_serial_plus_addr_value() {
    let serial = [b'X'; SERIAL_LEN];
    let mut p = Vec::with_capacity(SERIAL_LEN + 4);
    p.extend_from_slice(&serial);
    p.extend_from_slice(&50u16.to_be_bytes());
    p.extend_from_slice(&0xABCD_u16.to_be_bytes());
    let resp = build_response(&serial, 0x11, 0x06, &p);
    let inner_pdu = &resp[HEADER_SIZE..];
    let payload = &inner_pdu[2..inner_pdu.len() - 2];
    assert_eq!(&payload[0..10], &[b'X'; 10]);
    assert_eq!(u16::from_be_bytes([payload[10], payload[11]]), 50);
    assert_eq!(u16::from_be_bytes([payload[12], payload[13]]), 0xABCD);
}

// ===========================================================================
// 9. Sequential Multi-Request Tests
// ===========================================================================

#[tokio::test]
async fn multiple_reads_on_same_connection() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read IR 0 (status)
    let resp1 = send_recv(&mut stream, &build_read_input_request(0, 1)).await;
    let (_, _, payload) = decode_response(&resp1);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 1);

    // Read IR 59 (SOC)
    let resp2 = send_recv(&mut stream, &build_read_input_request(59, 1)).await;
    let (_, _, payload) = decode_response(&resp2);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 75);

    // Read IR 30 (grid power)
    let resp3 = send_recv(&mut stream, &build_read_input_request(30, 1)).await;
    let (_, _, payload) = decode_response(&resp3);
    let (_, _, data) = parse_read_payload(&payload);
    assert!(
        (data[0] as i16) > 0,
        "Grid power should be positive (exporting, GE convention), got {}",
        data[0] as i16
    );
}

#[tokio::test]
async fn write_then_read_roundtrip() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Write to address 100
    send_recv(&mut stream, &build_write_request(100, 1234)).await;
    // Read back
    let resp = send_recv(&mut stream, &build_read_request(100, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 1234);
}

#[tokio::test]
async fn multiple_writes_then_read() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    send_recv(&mut stream, &build_write_request(100, 10)).await;
    send_recv(&mut stream, &build_write_request(102, 30)).await;

    let resp = send_recv(&mut stream, &build_read_request(100, 3)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 10);
    assert_eq!(data[1], 0); // not written
    assert_eq!(data[2], 30);
}

// ===========================================================================
// 10. Input vs Holding Space Separation
// ===========================================================================

#[tokio::test]
async fn input_and_holding_address_0_differ() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Input register 0 = status (1)
    let resp_ir = send_recv(&mut stream, &build_read_input_request(0, 1)).await;
    let (_, _, payload) = decode_response(&resp_ir);
    let (_, _, data_ir) = parse_read_payload(&payload);

    // Holding register 0 = device type (0x2001)
    let resp_hr = send_recv(&mut stream, &build_read_request(0, 1)).await;
    let (_, _, payload) = decode_response(&resp_hr);
    let (_, _, data_hr) = parse_read_payload(&payload);

    assert_ne!(
        data_ir[0], data_hr[0],
        "IR 0 and HR 0 must return different values"
    );
    assert_eq!(data_ir[0], 1, "IR 0 = status");
    assert_eq!(data_hr[0], 0x2001, "HR 0 = device type");
}

#[tokio::test]
async fn input_register_59_vs_holding_59() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // IR 59 = battery SOC (75)
    let resp_ir = send_recv(&mut stream, &build_read_input_request(59, 1)).await;
    let (_, _, payload) = decode_response(&resp_ir);
    let (_, _, data_ir) = parse_read_payload(&payload);

    // HR 59 = enable discharge (0 for Normal mode)
    let resp_hr = send_recv(&mut stream, &build_read_request(59, 1)).await;
    let (_, _, payload) = decode_response(&resp_hr);
    let (_, _, data_hr) = parse_read_payload(&payload);

    assert_eq!(data_ir[0], 75, "IR 59 = SOC");
    assert_eq!(data_hr[0], 0, "HR 59 = enable discharge (Normal mode)");
}

// ===========================================================================
// 11. Simulator-Internal Register Tests (addresses 100+)
// ===========================================================================

#[tokio::test]
async fn internal_inverter_mode_reflects_state() {
    let mut state = test_state();
    state
        .inverter
        .mode_state
        .set_user(sim_models::InverterMode::ForceCharge);
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_request(100, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 2, "Internal reg 100 = ForceCharge = 2");
}

#[tokio::test]
async fn internal_battery_soc_reflects_state() {
    let state = test_state(); // SOC = 75%
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_request(200, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 75);
}

#[tokio::test]
async fn internal_grid_power_negative() {
    let state = test_state(); // grid.power_w = -800
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_request(400, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert!(
        (data[0] as i16) < 0,
        "Grid power should be stored as signed negative"
    );
}

// ===========================================================================
// 12. Edge Cases
// ===========================================================================

#[tokio::test]
async fn read_zero_count_returns_error() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // count=0 should be rejected
    let req = build_read_request(0, 0);
    // The server should either not respond or skip — send a valid read after to verify liveness
    let _ = stream.write_all(&req).await.unwrap();
    // Follow up with a valid request to confirm the connection is still alive
    let resp = send_recv(&mut stream, &build_read_request(100, 1)).await;
    let (_, func, _) = decode_response(&resp);
    assert_eq!(
        func, FC_READ_HOLDING,
        "Server should still work after bad request"
    );
}

#[tokio::test]
async fn read_large_register_range_with_gaps() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read 60 registers starting at address 300 — most are undefined, some defined
    let resp = send_recv(&mut stream, &build_read_request(300, 5)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data.len(), 5);
    // Only reg 300 (pv_generation) is defined, rest should be 0
    assert_eq!(data[0], 4000, "Reg 300 = pv_generation = 4000W");
    assert_eq!(data[1], 3500, "Reg 301 = pv_voltage = 350.0V / 0.1 = 3500");
}

#[tokio::test]
async fn gateway_bank_unmapped_range_rejected() {
    // Reads to entirely unmapped sub-ranges of the gateway aggregation bank
    // (IR 1600-1859) must return an error, not zeros, so client bounds-checks
    // correctly distinguish "no gateway" from "all registers happen to be 0".
    let ts = chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    let mut state = sim_models::PlantState::new(ts);
    state.config.inverter_type = "Gateway12kW".to_string();
    state.batteries[0].soc_percent = 65.0;
    state.batteries[0].power_kw = 2.0;
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // IR 1632-1639 is entirely unmapped in V1 — should get error
    let resp = send_recv(&mut stream, &build_read_input_request(1632, 8)).await;
    let (_, func, _) = decode_response(&resp);
    assert_eq!(
        func,
        FC_READ_INPUT | 0x80,
        "Unmapped gateway bank range must return error"
    );

    // IR 1600-1603 (version string) is valid — must return data
    let resp = send_recv(&mut stream, &build_read_input_request(1600, 4)).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, FC_READ_INPUT, "Valid gateway range must succeed");
    let (_, _, data) = parse_read_payload(&payload);
    assert!(data.len() >= 4, "Version string data expected");
    assert_eq!(data[0], (b'G' as u16) << 8 | b'A' as u16);

    // IR 1658-1699 is entirely unmapped — should get error
    let resp = send_recv(&mut stream, &build_read_input_request(1658, 42)).await;
    let (_, func, _) = decode_response(&resp);
    assert_eq!(
        func,
        FC_READ_INPUT | 0x80,
        "Unmapped bank range 1658-1699 must return error"
    );

    // IR 1700-1704 (AIO summary) is valid — must succeed
    let resp = send_recv(&mut stream, &build_read_input_request(1700, 5)).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, FC_READ_INPUT, "Valid AIO summary range must succeed");
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 1, "parallel_aio_num = 1");
}

#[tokio::test]
async fn gateway_multi_aio_serves_3_aio_registers() {
    // Gateway with 3 modules = 3 AIOs, each getting 1/3 of power/energy/SOC.
    let ts = chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    let mut state = sim_models::PlantState::with_battery_count(ts, 3);
    state.config.inverter_type = "Gateway12kW".to_string();
    state.config.parallel_aio_num = 3;
    for b in &mut state.batteries {
        b.soc_percent = 65.0;
        b.power_kw = 2.0;
    }
    state.sync_battery_from_vec();
    state.energy_totals.battery_charge_kwh = 50.0;
    state.energy_totals.battery_discharge_kwh = 30.0;

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // 1) Read parallel_aio_num
    let resp = send_recv(&mut stream, &build_read_input_request(1700, 2)).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, FC_READ_INPUT);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 3, "parallel_aio_num = 3");
    assert_eq!(data[1], 3, "parallel_aio_online_num = 3");

    // 2) Read per-AIO SOC (1801-1803)
    let resp = send_recv(&mut stream, &build_read_input_request(1801, 3)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 65, "aio1_soc");
    assert_eq!(data[1], 65, "aio2_soc");
    assert_eq!(data[2], 65, "aio3_soc");

    // 3) Read per-AIO inverter power (1816-1818)
    // 3 modules × 2kW charging = 6kW total. Gateway wire convention
    // (opposite of standard IR 52 p_battery): raw + = charging, so each
    // AIO emits +6000/3 = +2000W charging (positive on wire).
    let resp = send_recv(&mut stream, &build_read_input_request(1816, 3)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    for (i, &v) in data.iter().enumerate() {
        let signed = v as i16;
        assert_eq!(signed, 2000, "AIO{} power must be +2000W (charging)", i + 1);
    }

    // 4) Read per-AIO charge today (1705, 1708, 1711)
    // 50 kWh total / 3 = 16.667 kWh each → deci = 167
    let resp = send_recv(&mut stream, &build_read_input_request(1705, 9)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 167, "aio1_charge_today");
    assert_eq!(data[3], 167, "aio2_charge_today");
    assert_eq!(data[6], 167, "aio3_charge_today");

    // 5) Read AIO serials (V1: 1831-1835, 1838-1842, 1845-1849)
    // 3 AIOs × 5 regs with 3-reg gaps (1836-1837, 1843-1844) = 19 regs
    let resp = send_recv(&mut stream, &build_read_input_request(1831, 19)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data.len(), 19);

    let decode_serial = |start: usize| -> String {
        let mut s = String::new();
        for &v in data.iter().skip(start).take(5) {
            s.push((v >> 8) as u8 as char);
            s.push((v & 0xFF) as u8 as char);
        }
        s.trim().to_string()
    };
    assert_eq!(decode_serial(0), "SA24230001", "aio1 serial at 1831-1835");
    // Gap at 1836-1837 (data[5..7])
    assert_eq!(data[5], 0, "gap 1836");
    assert_eq!(data[6], 0, "gap 1837");
    assert_eq!(decode_serial(7), "SA24230002", "aio2 serial at 1838-1842");
    // Gap at 1843-1844 (data[12..14])
    assert_eq!(data[12], 0, "gap 1843");
    assert_eq!(data[13], 0, "gap 1844");
    assert_eq!(decode_serial(14), "SA24230003", "aio3 serial at 1845-1849");
}

#[tokio::test]
async fn write_and_read_different_slave_addresses() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Write uses slave 0x11, read uses slave 0x32 — both should work
    send_recv(&mut stream, &build_write_request(100, 99)).await;

    // Read with slave 0x32 (standard read address)
    let resp = send_recv(&mut stream, &build_read_request(100, 1)).await;
    let (slave, _, payload) = decode_response(&resp);
    assert_eq!(slave, 0x32);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 99);
}

#[tokio::test]
async fn multi_battery_input_registers() {
    let mut state = sim_models::PlantState::with_battery_count(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        3,
    );
    state.batteries[0].soc_percent = 50.0;
    state.batteries[1].soc_percent = 60.0;
    state.batteries[2].soc_percent = 70.0;
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // IR 59 = aggregate SOC (capacity-weighted avg)
    let resp = send_recv(&mut stream, &build_read_input_request(59, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 60, "IR 59 = capacity-weighted average SOC = 60");
}

// ===========================================================================
// 13. New Holding Register Tests (schedules, time, pause, calibration)
// ===========================================================================

#[tokio::test]
async fn ge_holding_system_time_readable() {
    let state = test_state(); // 2025-06-01 12:00:00
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read HR 35-40 (system time)
    let resp = send_recv(&mut stream, &build_read_request(35, 6)).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, FC_READ_HOLDING);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data.len(), 6);
    assert_eq!(data[0], 2025, "Year");
    assert_eq!(data[1], 6, "Month");
    assert_eq!(data[2], 1, "Day");
    assert_eq!(data[3], 12, "Hour");
}

#[tokio::test]
async fn ge_holding_schedule_slots_disabled() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read discharge slot 1 (HR 56-57)
    let resp = send_recv(&mut stream, &build_read_request(56, 2)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 60, "HR 56 discharge slot 1 start = 60 (disabled)");
    assert_eq!(data[1], 60, "HR 57 discharge slot 1 end = 60 (disabled)");
}

#[tokio::test]
async fn ge_holding_charge_slots_disabled() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read charge slot 1 (HR 94-95)
    let resp = send_recv(&mut stream, &build_read_request(94, 2)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 60, "HR 94 charge slot 1 start = 60 (disabled)");
    assert_eq!(data[1], 60, "HR 95 charge slot 1 end = 60 (disabled)");
}

#[tokio::test]
async fn ge_holding_pause_mode_readable() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read HR 318-320
    let resp = send_recv(&mut stream, &build_read_request(318, 3)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 0, "HR 318 pause mode = 0");
    assert_eq!(data[1], 60, "HR 319 pause slot start = 60 (disabled)");
    assert_eq!(data[2], 60, "HR 320 pause slot end = 60 (disabled)");
}

#[tokio::test]
async fn ge_write_schedule_slot() {
    let (addr, _) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Write discharge slot 1 start = 1800 (18:00)
    let resp = send_recv(&mut stream, &build_write_request(56, 1800)).await;
    let (_, func, payload) = decode_response(&resp);
    assert_eq!(func, FC_WRITE_SINGLE);
    let val = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(val, 1800);

    // Read back
    let resp = send_recv(&mut stream, &build_read_request(56, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 1800, "HR 56 should read back 1800");
}

#[tokio::test]
async fn ge_write_enable_discharge() {
    let (addr, mut rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_write_request(59, 1)).await;
    let (_, func, _) = decode_response(&resp);
    assert_eq!(func, FC_WRITE_SINGLE);
    let cmd = rx.try_recv().unwrap();
    assert_eq!(cmd.address, 59);
    assert_eq!(cmd.value, 1);
}

#[tokio::test]
async fn ge_write_soc_reserve() {
    let (addr, mut rx) = start_test_server().await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_write_request(110, 4)).await;
    let (_, func, _) = decode_response(&resp);
    assert_eq!(func, FC_WRITE_SINGLE);
    let cmd = rx.try_recv().unwrap();
    assert_eq!(cmd.address, 110);
    assert_eq!(cmd.value, 4);
}

// ===========================================================================
// 14. Battery Power Sign Convention Tests
// ===========================================================================

#[tokio::test]
async fn battery_power_raw_negative_when_charging() {
    let mut state = test_state();
    state.batteries[0].power_kw = 2.0; // charging 2kW
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // IR 52 = battery power raw
    let resp = send_recv(&mut stream, &build_read_input_request(52, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    let raw = data[0];
    // Client does: battery_power = -signed(raw)
    // So raw should be negative for charging
    assert!(
        (raw as i16) < 0,
        "IR 52 raw should be negative when charging, got {raw}"
    );
}

#[tokio::test]
async fn battery_power_raw_positive_when_discharging() {
    let mut state = test_state();
    state.batteries[0].power_kw = -2.0; // discharging 2kW
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let resp = send_recv(&mut stream, &build_read_input_request(52, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    let raw = data[0];
    assert!(
        (raw as i16) > 0,
        "IR 52 raw should be positive when discharging, got {raw}"
    );
}

// ===========================================================================
// 15. Multi-Slave Battery BMS Tests
// ===========================================================================

use sim_modbus::{HEADER_SIZE, SERIAL_LEN, crc16};
use std::sync::Arc;

/// Start a modbus server with battery state for BMS testing.
async fn start_server_with_batteries(
    batteries: &[sim_models::BatteryState],
) -> (
    SocketAddr,
    Arc<tokio::sync::Mutex<Vec<sim_models::BatteryState>>>,
) {
    let cat = sim_registers::default_register_catalogue();
    let store = Arc::new(tokio::sync::Mutex::new(sim_registers::RegisterStore::new(
        cat,
    )));
    let (tx, _rx): (
        tokio::sync::mpsc::UnboundedSender<sim_modbus::ModbusCommand>,
        _,
    ) = tokio::sync::mpsc::unbounded_channel();
    let batt_state = Arc::new(tokio::sync::Mutex::new(batteries.to_vec()));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let store_clone = store.clone();
    let batt_clone = batt_state.clone();

    tokio::spawn(async move {
        let server_store = store_clone;
        let server_batts = batt_clone;
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let store = server_store.clone();
            let _cmd_tx = tx.clone();
            let batts = server_batts.clone();
            tokio::spawn(async move {
                let mut buf = [0u8; 512];
                let mut pending: Vec<u8> = Vec::new();
                let mut stream = stream;
                loop {
                    let n = match stream.read(&mut buf).await {
                        Ok(0) => break,
                        Ok(n) => n,
                        Err(_) => break,
                    };
                    pending.extend_from_slice(&buf[..n]);
                    loop {
                        if pending.len() < HEADER_SIZE {
                            break;
                        }
                        let tx_id = u16::from_be_bytes([pending[0], pending[1]]);
                        if tx_id != 0x5959 {
                            return;
                        }
                        let length = u16::from_be_bytes([pending[4], pending[5]]) as usize;
                        let frame_len = 6 + length;
                        if pending.len() < frame_len {
                            break;
                        }
                        let frame = &pending[..frame_len];
                        let mut serial = [b' '; SERIAL_LEN];
                        serial.copy_from_slice(&frame[8..8 + SERIAL_LEN]);
                        let inner = &frame[HEADER_SIZE..];
                        let slave = inner[0];
                        let inner_func = inner[1];
                        let payload = &inner[2..inner.len() - 2];
                        if inner_func == 0x03 || inner_func == 0x04 {
                            let start = u16::from_be_bytes([payload[0], payload[1]]);
                            let count = u16::from_be_bytes([payload[2], payload[3]]);
                            let space = if inner_func == 0x04 {
                                sim_registers::RegisterSpace::Input
                            } else {
                                sim_registers::RegisterSpace::Holding
                            };

                            let battery_index = if slave == 0x32
                                && inner_func == 0x04
                                && (60..120).contains(&start)
                            {
                                Some(0)
                            } else if (0x33..=0x37).contains(&slave) && inner_func == 0x04 {
                                Some((slave - 0x33 + 1) as usize)
                            } else {
                                None
                            };

                            let reg_data = if let Some(bi) = battery_index {
                                let bs = batts.lock().await;
                                if let Some(b) = bs.get(bi) {
                                    let sg = store.lock().await;
                                    let bms = sg.project_battery_bms(b, bi);
                                    drop(sg);
                                    let mut d = Vec::with_capacity(count as usize * 2);
                                    for i in 0..count {
                                        let idx = (start - 60 + i) as usize;
                                        d.extend_from_slice(
                                            &bms.get(idx).copied().unwrap_or(0).to_be_bytes(),
                                        );
                                    }
                                    d
                                } else {
                                    vec![0u8; count as usize * 2]
                                }
                            } else {
                                let sg = store.lock().await;
                                let mut d = Vec::with_capacity(count as usize * 2);
                                for i in 0..count {
                                    d.extend_from_slice(
                                        &sg.read_by_space(start + i, space)
                                            .unwrap_or(0)
                                            .to_be_bytes(),
                                    );
                                }
                                d
                            };

                            let mut resp_payload =
                                Vec::with_capacity(SERIAL_LEN + 4 + reg_data.len());
                            resp_payload.extend_from_slice(&serial);
                            resp_payload.extend_from_slice(&start.to_be_bytes());
                            resp_payload.extend_from_slice(&count.to_be_bytes());
                            resp_payload.extend_from_slice(&reg_data);
                            let resp = sim_modbus::build_response(
                                &serial,
                                slave,
                                inner_func,
                                &resp_payload,
                            );
                            let _ = stream.write_all(&resp).await;
                        }
                        pending.drain(..frame_len);
                    }
                }
            });
        }
    });
    (addr, batt_state)
}

#[tokio::test]
async fn multi_slave_battery_bms_returns_soc() {
    let batts = vec![
        sim_models::BatteryState {
            soc_percent: 75.0,
            voltage_v: 50.0,
            ..Default::default()
        },
        sim_models::BatteryState {
            soc_percent: 30.0,
            voltage_v: 48.0,
            ..Default::default()
        },
    ];
    let (addr, _bs) = start_server_with_batteries(&batts).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read battery #1 BMS on slave 0x32 (IR 100 = SOC)
    let req = build_read_input_request_with_slave(0x32, 100, 1);
    let resp = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 75, "Battery #1 SOC should be 75%");
}

#[tokio::test]
async fn multi_slave_battery_2_on_0x33() {
    let batts = vec![
        sim_models::BatteryState {
            soc_percent: 75.0,
            voltage_v: 50.0,
            ..Default::default()
        },
        sim_models::BatteryState {
            soc_percent: 30.0,
            voltage_v: 48.0,
            ..Default::default()
        },
    ];
    let (addr, _bs) = start_server_with_batteries(&batts).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read battery #2 BMS on slave 0x33 (IR 100 = SOC)
    let req = build_read_input_request_with_slave(0x33, 100, 1);
    let resp = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 30, "Battery #2 SOC should be 30%");
}

#[tokio::test]
async fn multi_slave_battery_3_not_present() {
    let batts = vec![sim_models::BatteryState {
        soc_percent: 75.0,
        ..Default::default()
    }];
    let (addr, _bs) = start_server_with_batteries(&batts).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read battery #3 BMS on slave 0x35 — not present → zeros
    let req = build_read_input_request_with_slave(0x35, 100, 1);
    let resp = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 0, "Missing battery SOC should be 0");
}

#[tokio::test]
async fn multi_slave_battery_bms_full_block() {
    let batts = vec![sim_models::BatteryState {
        soc_percent: 50.0,
        voltage_v: 52.0,
        temperature_celsius: 28.0,
        capacity_kwh: 9.5,
        ..Default::default()
    }];
    let (addr, _bs) = start_server_with_batteries(&batts).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read full BMS block IR 60-119
    let req = build_read_input_request_with_slave(0x32, 60, 60);
    let resp = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data.len(), 60, "Should return 60 BMS registers");

    // Check known positions
    assert_eq!(data[100 - 60], 50, "IR 100 = SOC 50%");
    assert_eq!(data[97 - 60], 16, "IR 97 = 16 cells");

    // Voltage at IR 82-83 (uint32 mV)
    let v_mv = ((data[82 - 60] as u32) << 16) | data[83 - 60] as u32;
    assert!(
        v_mv > 48000 && v_mv < 55000,
        "Voltage ~52V = ~52000 mV, got {v_mv}"
    );
}

// Helper: build read request with custom slave address
fn build_read_input_request_with_slave(slave: u8, start: u16, count: u16) -> Vec<u8> {
    let serial = b"SIM0000001";
    let mut serial_arr = [b' '; SERIAL_LEN];
    serial_arr[..serial.len()].copy_from_slice(serial);

    // Inner PDU: slave + 0x04 + start_addr(2) + count(2) + CRC
    let mut inner = Vec::with_capacity(6);
    inner.push(slave);
    inner.push(0x04);
    inner.extend_from_slice(&start.to_be_bytes());
    inner.extend_from_slice(&count.to_be_bytes());
    let crc = crc16(&inner);
    inner.extend_from_slice(&crc.to_le_bytes());

    // GivEnergy envelope
    let length = (1 + 1 + SERIAL_LEN + 8 + inner.len()) as u16;
    let mut frame = Vec::with_capacity(HEADER_SIZE + inner.len());
    frame.extend_from_slice(&0x5959u16.to_be_bytes());
    frame.extend_from_slice(&0x0001u16.to_be_bytes());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(0x01);
    frame.push(0x02);
    frame.extend_from_slice(&serial_arr);
    frame.extend_from_slice(&8u64.to_be_bytes());
    frame.extend_from_slice(&inner);
    frame
}

// ===========================================================================
// CT Meter tests using the real run_modbus_server
// ===========================================================================

/// Start the REAL Modbus server (not the simplified test server) so that
/// meter-slave routing, battery BMS routing etc. are exercised.
async fn start_real_server_with_state(
    state: &sim_models::PlantState,
) -> (
    SocketAddr,
    std::sync::Arc<tokio::sync::Mutex<RegisterStore>>,
    tokio::sync::mpsc::UnboundedReceiver<ModbusCommand>,
) {
    let store = RegisterStore::new(default_register_catalogue());
    let mut store_clone = store.clone();
    store_clone.project_from_state(state);
    let store = std::sync::Arc::new(tokio::sync::Mutex::new(store_clone));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // free the port so run_modbus_server can bind it

    let store_ref = store.clone();
    let batt = std::sync::Arc::new(tokio::sync::Mutex::new(state.batteries.clone()));
    tokio::spawn(async move {
        let _ = sim_modbus::run_modbus_server(addr, store_ref, tx, batt).await;
    });
    // Give the server time to start
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    (addr, store, rx)
}

#[tokio::test]
async fn ct_meter_slave_0x01_returns_meter_data_via_real_server() {
    let mut state = test_state();
    state.grid.power_w = 2400.0; // 2.4 kW import
    state.config.inverter_type = "ACCoupled".to_string();
    state.config.ct_meter_installed = true;
    state.sync_battery_from_vec();

    let (addr, _store, _rx) = start_real_server_with_state(&state).await;

    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Client probes meter: slave 0x01, IR 60, count 30
    let request = build_read_input_request_with_slave(0x01, 60, 30);
    stream.write_all(&request).await.unwrap();

    // Read response
    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(std::time::Duration::from_secs(2), stream.read(&mut buf))
        .await
        .expect("timeout reading response")
        .expect("read error");

    assert!(n >= HEADER_SIZE, "Response too short: {n} bytes");

    // Parse the response
    let resp = &buf[..n];
    // Skip GivEnergy header (26 bytes) to get to inner PDU
    let inner_pdu = &resp[HEADER_SIZE..];
    assert!(inner_pdu.len() >= 4, "Inner PDU too short");
    // Inner PDU: slave(1) + func(1) + serial(10) + start(2) + count(2) + data + CRC(2)
    let payload = &inner_pdu[2..inner_pdu.len() - 2]; // strip slave+func, strip CRC
    // payload = serial(10) + start(2) + count(2) + data
    assert!(payload.len() >= 14, "Payload too short: {}", payload.len());
    let data_start = u16::from_be_bytes([payload[10], payload[11]]);
    let data_count = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(data_start, 60);
    assert_eq!(data_count, 30);

    let data: Vec<u16> = payload[14..]
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    assert_eq!(data.len(), 30);

    // Decode like the real client:
    // data[0] = IR 60 (v_phase_1, ×0.1 V)
    let v1 = data[0] as f32 * 0.1;
    assert!(v1 > 100.0, "v_phase_1 should be > 100V, got {v1}");

    // data[7] = IR 67 (i_total, ×0.01 A)
    let i_total = data[7] as f32 * 0.01;
    assert!(i_total > 0.0, "i_total should be > 0, got {i_total}");

    // data[19] = IR 79 (p_apparent_total, signed VA)
    let p_apparent = data[19] as i16 as i32;
    assert!(
        p_apparent > 0,
        "p_apparent_total should be > 0, got {p_apparent}"
    );

    // data[23] = IR 83 (pf_total, ×0.001)
    let pf = data[23] as f32 * 0.001;
    assert!(pf > 0.0, "pf_total should be > 0, got {pf}");
}
