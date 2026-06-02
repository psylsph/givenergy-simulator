//! Integration tests for battery limits, schedule registers, and BMS data.
//!
//! Tests cover:
//! - Battery charge/discharge limits projected from PlantState via register scaling
//! - Schedule slot register reads (disabled sentinel values)
//! - Simulator-internal schedule registers
//! - Schedule slot register writes and read-back
//! - Full block reads of input and holding registers
//! - Multi-battery BMS data via project_battery_bms

use sim_modbus::*;
use sim_registers::{RegisterStore, default_register_catalogue, RegisterSpace};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use std::net::SocketAddr;

// ===========================================================================
// Helpers (copied from givenergy_protocol.rs)
// ===========================================================================

const TEST_SERIAL: [u8; SERIAL_LEN] = [b'S', b'A', b'1', b'2', b'3', b'4', b' ', b' ', b' ', b' '];

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

fn decode_response(data: &[u8]) -> (u8, u8, Vec<u8>) {
    assert!(data.len() >= HEADER_SIZE + 4, "Response too short: {} bytes", data.len());
    let inner_pdu = &data[HEADER_SIZE..];
    let slave = inner_pdu[0];
    let func = inner_pdu[1];
    let payload = inner_pdu[2..inner_pdu.len() - 2].to_vec();
    (slave, func, payload)
}

fn parse_read_payload(payload: &[u8]) -> (u16, u16, Vec<u16>) {
    assert!(payload.len() >= 14, "Read payload too short: {}", payload.len());
    let start = u16::from_be_bytes([payload[10], payload[11]]);
    let count = u16::from_be_bytes([payload[12], payload[13]]);
    let data = payload[14..]
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    (start, count, data)
}

async fn start_server_with_state(state: &sim_models::PlantState) -> (SocketAddr, RegisterStore, tokio::sync::mpsc::UnboundedReceiver<ModbusCommand>) {
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
            let n = match stream.read(&mut buf).await { Ok(0) => break, Ok(n) => n, Err(_) => break };
            pending.extend_from_slice(&buf[..n]);
            loop {
                if pending.len() < HEADER_SIZE { break; }
                let length = u16::from_be_bytes([pending[4], pending[5]]) as usize;
                let frame_len = 6 + length;
                if pending.len() < frame_len { break; }
                let frame = &pending[..frame_len];
                let mut serial = [b' '; SERIAL_LEN];
                serial.copy_from_slice(&frame[8..8 + SERIAL_LEN]);
                let inner_pdu = &frame[HEADER_SIZE..];
                if inner_pdu.len() < 4 { pending.drain(..frame_len); continue; }
                let slave = inner_pdu[0];
                let inner_func = inner_pdu[1];
                let inner_payload = &inner_pdu[2..inner_pdu.len() - 2];
                match inner_func {
                    FC_READ_HOLDING | FC_READ_INPUT => {
                        if inner_payload.len() < 4 { pending.drain(..frame_len); continue; }
                        let start_addr = u16::from_be_bytes([inner_payload[0], inner_payload[1]]);
                        let count = u16::from_be_bytes([inner_payload[2], inner_payload[3]]);
                        let s = store_ref.lock().await;
                        let space = if inner_func == FC_READ_INPUT { RegisterSpace::Input } else { RegisterSpace::Holding };
                        let mut reg_data = Vec::with_capacity(count as usize * 2);
                        for i in 0..count { reg_data.extend_from_slice(&s.read_by_space(start_addr + i, space).unwrap_or(0).to_be_bytes()); }
                        drop(s);
                        let mut resp_payload = Vec::with_capacity(SERIAL_LEN + 4 + reg_data.len());
                        resp_payload.extend_from_slice(&serial);
                        resp_payload.extend_from_slice(&start_addr.to_be_bytes());
                        resp_payload.extend_from_slice(&count.to_be_bytes());
                        resp_payload.extend_from_slice(&reg_data);
                        let _ = stream.write_all(&build_response(&serial, slave, inner_func, &resp_payload)).await;
                    }
                    FC_WRITE_SINGLE => {
                        if inner_payload.len() < 4 { pending.drain(..frame_len); continue; }
                        let address = u16::from_be_bytes([inner_payload[0], inner_payload[1]]);
                        let value = u16::from_be_bytes([inner_payload[2], inner_payload[3]]);
                        let ok = { let mut s = store_ref.lock().await; s.write(address, value) };
                        if ok {
                            let mut p = Vec::with_capacity(SERIAL_LEN + 4);
                            p.extend_from_slice(&serial);
                            p.extend_from_slice(&address.to_be_bytes());
                            p.extend_from_slice(&value.to_be_bytes());
                            let _ = stream.write_all(&build_response(&serial, slave, inner_func, &p)).await;
                            let _ = tx.send(ModbusCommand { address, value });
                        } else {
                            let _ = stream.write_all(&build_error_response(&serial, slave, inner_func, EC_ILLEGAL_DATA_ADDRESS)).await;
                        }
                    }
                    _ => { let _ = stream.write_all(&build_error_response(&serial, slave, inner_func, EC_ILLEGAL_FUNCTION)).await; }
                }
                pending.drain(..frame_len);
            }
        }
    });

    let store_snapshot = store.lock().await.clone();
    (addr, store_snapshot, rx)
}

async fn send_recv(stream: &mut tokio::net::TcpStream, req: &[u8]) -> Vec<u8> {
    stream.write_all(req).await.unwrap();
    let mut buf = [0u8; 1024];
    let n = stream.read(&mut buf).await.unwrap();
    buf[..n].to_vec()
}

fn test_state() -> sim_models::PlantState {
    let mut state = sim_models::PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1).unwrap().and_hms_opt(12, 0, 0).unwrap()
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

/// Helper: read a single holding register and return its value.
async fn read_hr(stream: &mut tokio::net::TcpStream, addr: u16) -> u16 {
    let resp = send_recv(stream, &build_read_request(addr, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    data[0]
}

/// Helper: read a single input register and return its value.
#[allow(dead_code)]
async fn read_ir(stream: &mut tokio::net::TcpStream, addr: u16) -> u16 {
    let resp = send_recv(stream, &build_read_input_request(addr, 1)).await;
    let (_, _, payload) = decode_response(&resp);
    let (_, _, data) = parse_read_payload(&payload);
    data[0]
}

// ===========================================================================
// 1. Battery charge limit reflects inverter type
// ===========================================================================

#[tokio::test]
async fn ac_coupled_battery_charge_limit_capped_at_3kw() {
    let mut state = test_state();
    state.config.inverter_type = "ACCoupled".to_string();
    state.batteries[0].max_charge_kw = 3.0;
    state.batteries[0].capacity_kwh = 8.2;
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 208 = battery_max_charge_kw, scaling 0.1 → 3.0 kW = raw 30
    let val = read_hr(&mut stream, 208).await;
    assert_eq!(val, 30, "HR 208 (battery_max_charge_kw) should be 30 (= 3.0 kW / 0.1 scaling)");
}

#[tokio::test]
async fn gen3_hybrid_battery_limit_3_6kw() {
    let mut state = test_state();
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.batteries[0].max_charge_kw = 3.6;
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 208 = battery_max_charge_kw, scaling 0.1 → 3.6 kW = raw 36
    let val = read_hr(&mut stream, 208).await;
    assert_eq!(val, 36, "HR 208 should be 36 (= 3.6 kW / 0.1 scaling)");
}

#[tokio::test]
async fn aio_10kw_battery_limit_10kw() {
    let mut state = test_state();
    state.config.inverter_type = "AIO10kW".to_string();
    state.batteries[0].max_charge_kw = 10.0;
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 208 = battery_max_charge_kw, scaling 0.1 → 10.0 kW = raw 100
    let val = read_hr(&mut stream, 208).await;
    assert_eq!(val, 100, "HR 208 should be 100 (= 10.0 kW / 0.1 scaling)");
}

// ===========================================================================
// 2. Schedule slot register reads
// ===========================================================================

#[tokio::test]
async fn charge_slot_1_reads_default_schedule() {
    // Default schedule: charge slots are disabled sentinel (60)
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 94 = charge_slot_1_start → disabled sentinel = 60
    let val94 = read_hr(&mut stream, 94).await;
    assert_eq!(val94, 60, "HR 94 (charge_slot_1_start) should be 60 (disabled sentinel)");

    // HR 95 = charge_slot_1_end → disabled sentinel = 60
    let val95 = read_hr(&mut stream, 95).await;
    assert_eq!(val95, 60, "HR 95 (charge_slot_1_end) should be 60 (disabled sentinel)");
}

#[tokio::test]
async fn discharge_slot_1_disabled_by_default() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let val56 = read_hr(&mut stream, 56).await;
    assert_eq!(val56, 60, "HR 56 (discharge_slot_1_start) should be 60 (disabled)");

    let val57 = read_hr(&mut stream, 57).await;
    assert_eq!(val57, 60, "HR 57 (discharge_slot_1_end) should be 60 (disabled)");
}

#[tokio::test]
async fn charge_slot_2_disabled_by_default() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    let val31 = read_hr(&mut stream, 31).await;
    assert_eq!(val31, 60, "HR 31 (charge_slot_2_start) should be 60 (disabled)");

    let val32 = read_hr(&mut stream, 32).await;
    assert_eq!(val32, 60, "HR 32 (charge_slot_2_end) should be 60 (disabled)");
}

#[tokio::test]
async fn simulator_internal_schedule_registers() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // HR 700 = schedule_charge_start → 0 (default: midnight)
    let val700 = read_hr(&mut stream, 700).await;
    assert_eq!(val700, 0, "HR 700 (schedule_charge_start) should be 0");

    // HR 701 = schedule_charge_end → 0 (default)
    let val701 = read_hr(&mut stream, 701).await;
    assert_eq!(val701, 0, "HR 701 (schedule_charge_end) should be 0");
}

// ===========================================================================
// 3. Schedule slot register writes
// ===========================================================================

#[tokio::test]
async fn write_charge_slot_1() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Write HR 94 = 2200 (22:00)
    let write_resp = send_recv(&mut stream, &build_write_request(94, 2200)).await;
    let (_, func, payload) = decode_response(&write_resp);
    assert_eq!(func, FC_WRITE_SINGLE, "Write should succeed for HR 94 (ReadWrite)");
    let written_addr = u16::from_be_bytes([payload[10], payload[11]]);
    let written_val = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(written_addr, 94);
    assert_eq!(written_val, 2200);

    // Read back HR 94
    let val94 = read_hr(&mut stream, 94).await;
    assert_eq!(val94, 2200, "HR 94 should read back 2200 after write");

    // Write HR 95 = 600 (06:00)
    send_recv(&mut stream, &build_write_request(95, 600)).await;

    // Read back HR 95
    let val95 = read_hr(&mut stream, 95).await;
    assert_eq!(val95, 600, "HR 95 should read back 600 after write");
}

#[tokio::test]
async fn write_discharge_slot_1() {
    let state = test_state();
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Write HR 56 = 1700 (17:00)
    let write_resp = send_recv(&mut stream, &build_write_request(56, 1700)).await;
    let (_, func, _) = decode_response(&write_resp);
    assert_eq!(func, FC_WRITE_SINGLE, "Write should succeed for HR 56 (ReadWrite)");

    // Read back HR 56
    let val56 = read_hr(&mut stream, 56).await;
    assert_eq!(val56, 1700, "HR 56 should read back 1700 after write");
}

// ===========================================================================
// 4. Full block reads (like GivTCP)
// ===========================================================================

#[tokio::test]
async fn full_input_register_block_0_to_59() {
    let state = test_state(); // solar=4000W, battery SOC=75%, grid connected
    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read IR 0-59 in one request
    let resp = send_recv(&mut stream, &build_read_input_request(0, 60)).await;
    let (slave, func, payload) = decode_response(&resp);
    assert_eq!(slave, 0x32);
    assert_eq!(func, FC_READ_INPUT);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 0);
    assert_eq!(count, 60);
    assert_eq!(data.len(), 60);

    // IR 0 (status) = 1
    assert_eq!(data[0], 1, "IR 0 = status should be 1 (normal)");

    // IR 5 (grid voltage) = 2400 (240.0V × 10)
    assert_eq!(data[5], 2400, "IR 5 = grid voltage should be 2400 (= 240.0V × 10)");

    // IR 18 (PV1 power) > 0 (solar generating)
    assert!(data[18] > 0, "IR 18 = PV1 power should be > 0 (got {})", data[18]);

    // IR 59 (SOC) > 0
    assert!(data[59] > 0, "IR 59 = battery SOC should be > 0 (got {})", data[59]);
}

#[tokio::test]
async fn full_holding_register_block_0_to_119() {
    let mut state = test_state();
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.sync_battery_from_vec();

    let (addr, _, _) = start_server_with_state(&state).await;
    let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();

    // Read HR 0-119 in one request
    let resp = send_recv(&mut stream, &build_read_request(0, 120)).await;
    let (slave, func, payload) = decode_response(&resp);
    assert_eq!(slave, 0x32);
    assert_eq!(func, FC_READ_HOLDING);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 0);
    assert_eq!(count, 120);
    assert_eq!(data.len(), 120);

    // HR 0 (device_type) = 0x2001 for Gen3Hybrid
    assert_eq!(data[0], 0x2001, "HR 0 (device_type) should be 0x2001 for Gen3Hybrid");

    // HR 50 (active_power_rate) = 100
    assert_eq!(data[50], 100, "HR 50 (active_power_rate) should be 100");

    // HR 56 (discharge_slot_1_start) = 60 (disabled)
    assert_eq!(data[56], 60, "HR 56 (discharge_slot_1_start) should be 60 (disabled)");

    // HR 57 (discharge_slot_1_end) = 60 (disabled)
    assert_eq!(data[57], 60, "HR 57 (discharge_slot_1_end) should be 60 (disabled)");

    // HR 31 (charge_slot_2_start) = 60 (disabled)
    assert_eq!(data[31], 60, "HR 31 (charge_slot_2_start) should be 60 (disabled)");

    // HR 32 (charge_slot_2_end) = 60 (disabled)
    assert_eq!(data[32], 60, "HR 32 (charge_slot_2_end) should be 60 (disabled)");
}

// ===========================================================================
// 5. Multi-battery BMS
// ===========================================================================

#[tokio::test]
async fn multi_battery_bms_slave_32_and_33() {
    // Create state with 2 batteries
    let mut state = sim_models::PlantState::with_battery_count(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1).unwrap().and_hms_opt(12, 0, 0).unwrap(),
        2,
    );

    // Configure batteries with distinct values
    state.batteries[0].soc_percent = 75.0;
    state.batteries[0].voltage_v = 51.2;
    state.batteries[0].temperature_celsius = 25.0;
    state.batteries[0].capacity_kwh = 9.5;

    state.batteries[1].soc_percent = 30.0;
    state.batteries[1].voltage_v = 48.0;
    state.batteries[1].temperature_celsius = 22.0;
    state.batteries[1].capacity_kwh = 9.5;

    state.sync_battery_from_vec();

    let store = RegisterStore::new(default_register_catalogue());

    // Battery BMS on slave 0x32 (battery index 0)
    let bms0 = store.project_battery_bms(&state.batteries[0], 0);
    // Verify cell voltages (IR 60-75) are non-zero
    let cell_sum_0: u32 = bms0[0..16].iter().map(|&v| v as u32).sum();
    assert!(cell_sum_0 > 0, "Battery 0 cell voltages should be non-zero (sum = {})", cell_sum_0);
    // Verify SOC (IR 100 = index 40) = 75
    assert_eq!(bms0[40], 75, "Battery 0 SOC (IR 100) should be 75");

    // Battery BMS on slave 0x33 (battery index 1)
    let bms1 = store.project_battery_bms(&state.batteries[1], 1);
    let cell_sum_1: u32 = bms1[0..16].iter().map(|&v| v as u32).sum();
    assert!(cell_sum_1 > 0, "Battery 1 cell voltages should be non-zero (sum = {})", cell_sum_1);
    // Verify SOC (IR 100 = index 40) = 30
    assert_eq!(bms1[40], 30, "Battery 1 SOC (IR 100) should be 30");

    // Verify both return data (not all zeros)
    let all_zero_0 = bms0.iter().all(|&v| v == 0);
    assert!(!all_zero_0, "Battery 0 BMS data should not be all zeros");

    let all_zero_1 = bms1.iter().all(|&v| v == 0);
    assert!(!all_zero_1, "Battery 1 BMS data should not be all zeros");

    // Verify temperatures differ (battery 0 = 25°C, battery 1 = 22°C)
    let temp_0 = bms0[16]; // IR 76 = temperature (0.1°C)
    let temp_1 = bms1[16];
    assert_ne!(temp_0, temp_1, "Battery temperatures should differ: {} vs {}", temp_0, temp_1);
}
