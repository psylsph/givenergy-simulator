//! Integration tests: real GivEnergy Modbus TCP server ↔ TCP client round-trips.
//!
//! Spins up `run_modbus_server` on a random port, connects via TcpStream,
//! and verifies the proprietary frame protocol end-to-end.

use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, mpsc};
use tokio::time::{Duration, sleep};

use sim_modbus::{
    FC_READ_HOLDING, FC_READ_INPUT, FC_WRITE_SINGLE, HEADER_SIZE, ModbusCommand, SERIAL_LEN, crc16,
};
use sim_models::{BatteryState, PlantState};
use sim_registers::{RegisterStore, default_register_catalogue};

// ---------------------------------------------------------------------------
// Frame helpers (same logic as givenergy_protocol.rs)
// ---------------------------------------------------------------------------

fn serial_arr() -> [u8; SERIAL_LEN] {
    let mut s = [b' '; SERIAL_LEN];
    s.copy_from_slice(b"SIM0000001");
    s
}

fn build_read_request(
    serial: &[u8; SERIAL_LEN],
    slave: u8,
    func: u8,
    start: u16,
    count: u16,
) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&start.to_be_bytes());
    payload.extend_from_slice(&count.to_be_bytes());
    wrap_inner(serial, slave, func, &payload)
}

fn build_write_request(serial: &[u8; SERIAL_LEN], slave: u8, start: u16, value: u16) -> Vec<u8> {
    let mut payload = Vec::with_capacity(4);
    payload.extend_from_slice(&start.to_be_bytes());
    payload.extend_from_slice(&value.to_be_bytes());
    wrap_inner(serial, slave, FC_WRITE_SINGLE, &payload)
}

fn wrap_inner(serial: &[u8; SERIAL_LEN], slave: u8, func: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(2 + payload.len() + 2);
    inner.push(slave);
    inner.push(func);
    inner.extend_from_slice(payload);
    let crc = crc16(&inner);
    inner.extend_from_slice(&crc.to_le_bytes());

    let length = (1 + 1 + SERIAL_LEN + 8 + inner.len()) as u16;
    let mut frame = Vec::with_capacity(HEADER_SIZE + inner.len());
    frame.extend_from_slice(&0x5959u16.to_be_bytes());
    frame.extend_from_slice(&0x0001u16.to_be_bytes());
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(0x01);
    frame.push(0x02);
    frame.extend_from_slice(serial);
    frame.extend_from_slice(&8u64.to_be_bytes());
    frame.extend_from_slice(&inner);
    frame
}

/// Decode a response frame into (slave, func, payload_bytes).
/// Payload has CRC stripped.
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
    let _serial = &payload[..10];
    let start = u16::from_be_bytes([payload[10], payload[11]]);
    let count = u16::from_be_bytes([payload[12], payload[13]]);
    let data = payload[14..]
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    (start, count, data)
}

// ---------------------------------------------------------------------------
// Server helpers
// ---------------------------------------------------------------------------

/// Start the real `run_modbus_server` with a pre-populated state.
async fn start_server(
    state: &PlantState,
) -> (
    SocketAddr,
    Arc<Mutex<RegisterStore>>,
    mpsc::UnboundedReceiver<ModbusCommand>,
) {
    let mut store = RegisterStore::new(default_register_catalogue());
    store.project_from_state(state);
    let store = Arc::new(Mutex::new(store));

    let batteries: Vec<BatteryState> = state.batteries.clone();
    let batt_arc = Arc::new(Mutex::new(batteries));

    let (tx, rx) = mpsc::unbounded_channel();

    // Bind to port 0 to get a random free port
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener); // free the port so run_modbus_server can bind it

    let s = store.clone();
    let b = batt_arc;
    let t = tx.clone();
    tokio::spawn(async move {
        let _ = sim_modbus::run_modbus_server(addr, s, t, b).await;
    });

    // Wait for server to become ready
    for _ in 0..100 {
        if TcpStream::connect(addr).await.is_ok() {
            break;
        }
        sleep(Duration::from_millis(50)).await;
    }

    (addr, store, rx)
}

async fn send_recv(stream: &mut TcpStream, frame: &[u8]) -> Vec<u8> {
    stream.write_all(frame).await.expect("write frame");
    let mut buf = [0u8; 2048];
    let n = stream.read(&mut buf).await.expect("read response");
    buf[..n].to_vec()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[tokio::test]
async fn read_input_registers_end_to_end() {
    let state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    let (addr, _store, _rx) = start_server(&state).await;

    let serial = serial_arr();
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // Read IR 0..1 (status, PV1 voltage)
    let req = build_read_request(&serial, 0x32, FC_READ_INPUT, 0, 2);
    let raw = send_recv(&mut stream, &req).await;
    let (slave, func, payload) = decode_response(&raw);
    assert_eq!(slave, 0x32);
    assert_eq!(func, FC_READ_INPUT);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 0);
    assert_eq!(count, 2);
    assert_eq!(data[0], 1, "IR 0 = status (always 1)");

    // IR 59 (SOC) - default plant has 1 battery at SOC from default
    let req = build_read_request(&serial, 0x32, FC_READ_INPUT, 59, 1);
    let raw = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&raw);
    let (_, _, data) = parse_read_payload(&payload);
    // Default PlantState has SOC from default battery init
    assert!(data[0] <= 100, "SOC should be 0..100, got {}", data[0]);
}

#[tokio::test]
async fn read_holding_device_type() {
    let state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    let (addr, _store, _rx) = start_server(&state).await;

    let serial = serial_arr();
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // HR 0 = device type (0x2001 = Gen3 Hybrid)
    let req = build_read_request(&serial, 0x32, FC_READ_HOLDING, 0, 1);
    let raw = send_recv(&mut stream, &req).await;
    let (slave, func, payload) = decode_response(&raw);
    assert_eq!(slave, 0x32);
    assert_eq!(func, FC_READ_HOLDING);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 0x2001, "HR 0 = device type 0x2001");
}

#[tokio::test]
async fn read_write_holding_inverter_mode() {
    let state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    let (addr, _store, _rx) = start_server(&state).await;

    let serial = serial_arr();
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // HR 100 = inverter_mode (1=Eco by default)
    let req = build_read_request(&serial, 0x32, FC_READ_HOLDING, 100, 1);
    let raw = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&raw);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 1, "HR 100 starts at Eco (1)");

    // Write HR 100 = 2 (ForceCharge)
    let req = build_write_request(&serial, 0x32, 100, 2);
    let raw = send_recv(&mut stream, &req).await;
    let (slave, func, payload) = decode_response(&raw);
    assert_eq!(slave, 0x32);
    assert_eq!(func, FC_WRITE_SINGLE);
    // Write response payload: serial(10) + address(2) + value(2)
    assert_eq!(payload.len(), 14);
    let written_addr = u16::from_be_bytes([payload[10], payload[11]]);
    let written_val = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(written_addr, 100);
    assert_eq!(written_val, 2);

    // Re-read HR 100 — should now be 2
    let req = build_read_request(&serial, 0x32, FC_READ_HOLDING, 100, 1);
    let raw = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&raw);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 2, "HR 100 persisted ForceCharge (2)");

    // Write back to Normal (0)
    let req = build_write_request(&serial, 0x32, 100, 0);
    let raw = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&raw);
    let written_val = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(written_val, 0);
}

#[tokio::test]
async fn read_battery_soc_via_input_register() {
    let mut state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    state.batteries[0].soc_percent = 75.0;
    state.sync_battery_from_vec();

    let (addr, _store, _rx) = start_server(&state).await;
    let serial = serial_arr();
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // IR 59 = battery SOC
    let req = build_read_request(&serial, 0x32, FC_READ_INPUT, 59, 1);
    let raw = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&raw);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data[0], 75, "IR 59 SOC = 75%");
}

#[tokio::test]
async fn write_command_sent_to_channel() {
    let state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    let (addr, _store, mut rx) = start_server(&state).await;
    let serial = serial_arr();
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // Write HR 163 = 100 (inverter reboot command)
    let req = build_write_request(&serial, 0x32, 163, 100);
    let raw = send_recv(&mut stream, &req).await;
    let (_, func, _) = decode_response(&raw);
    assert_eq!(func, FC_WRITE_SINGLE);

    // The command should be sent to the channel
    let cmd = tokio::time::timeout(Duration::from_secs(2), rx.recv()).await;
    assert!(cmd.is_ok(), "should receive ModbusCommand");
    let cmd = cmd.unwrap().unwrap();
    assert_eq!(cmd.address, 163);
    assert_eq!(cmd.value, 100);
}

#[tokio::test]
async fn multi_register_read_crosses_register_types() {
    let state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    let (addr, _store, _rx) = start_server(&state).await;
    let serial = serial_arr();
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // Read a window of simulator-internal registers: HR 200..204 (battery metrics)
    let req = build_read_request(&serial, 0x32, FC_READ_HOLDING, 200, 5);
    let raw = send_recv(&mut stream, &req).await;
    let (_, func, payload) = decode_response(&raw);
    assert_eq!(func, FC_READ_HOLDING);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 200);
    assert_eq!(count, 5);
    assert_eq!(data.len(), 5);
    // HR 200 = battery SOC, HR 202 = battery temperature
    // Exact values depend on defaults, just verify they're within range
    assert!(data[0] <= 100, "SOC 0..100, got {}", data[0]);
}

#[tokio::test]
async fn read_battery_bms_on_slave_0x32() {
    let mut state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    state.batteries[0].soc_percent = 50.0;
    state.sync_battery_from_vec();

    let (addr, _store, _rx) = start_server(&state).await;
    let serial = serial_arr();
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // Read IR 60..119 (battery BMS data, 60 registers)
    let req = build_read_request(&serial, 0x32, FC_READ_INPUT, 60, 60);
    let raw = send_recv(&mut stream, &req).await;
    let (_, func, payload) = decode_response(&raw);
    assert_eq!(func, FC_READ_INPUT);
    let (start, count, data) = parse_read_payload(&payload);
    assert_eq!(start, 60);
    assert_eq!(count, 60);
    assert_eq!(data.len(), 60);

    // IR 100 = SOC
    assert_eq!(data[100 - 60], 50, "BMS SOC = 50%");
    // IR 97 = num_cells = 16
    assert_eq!(data[97 - 60], 16, "16 cells");
}

#[tokio::test]
async fn hv_battery_cluster_discovery_serves_bms_bcu_and_bmus() {
    // Reproduces the user's case: a GIV-3HY-11 (ThreePhase, 0x4004) with a
    // GIV-BAT-17.0-HV stack (5 x GIV-BAT-3.4-HV). HV inverters are discovered
    // via the BCU/BMU cluster protocol, NOT the LV 0x32 path, so a client issues
    // 1 BMS read (0xA0) + 1 BCU read (0x70) + 5 BMU reads (0x50-0x54) = 6 reads.
    let mut state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    // Build a 5-module HV stack.
    state.batteries = (0..5)
        .map(|_| sim_models::BatteryState {
            soc_percent: 60.0,
            voltage_v: 51.2,
            soh: 0.95,
            ..Default::default()
        })
        .collect();
    state.sync_battery_from_vec();

    let (addr, _store, _rx) = start_server(&state).await;
    let serial = serial_arr();
    let mut stream = TcpStream::connect(addr).await.expect("connect");

    // Step 1: BMS discovery at 0xA0, IR(60,5). IR(61) = number of BCUs.
    let req = build_read_request(&serial, 0xA0, FC_READ_INPUT, 60, 5);
    let raw = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&raw);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data.len(), 5);
    assert_eq!(data[1], 1, "IR(61) at 0xA0 must report 1 BCU");

    // Step 2: BCU cluster read at 0x70, IR(60,60). Must decode a valid version
    // and report 5 modules at IR(64).
    let req = build_read_request(&serial, 0x70, FC_READ_INPUT, 60, 60);
    let raw = send_recv(&mut stream, &req).await;
    let (_, _, payload) = decode_response(&raw);
    let (_, _, data) = parse_read_payload(&payload);
    assert_eq!(data.len(), 60);
    // IR 60-63 version prefix non-blank ('H','V')
    assert_eq!(data[0], 0x4856, "BCU version prefix must be 'HV'");
    assert_eq!(data[4], 5, "BCU IR(64) must report 5 modules");
    assert_eq!(data[5], 24, "BCU IR(65) cells_per_module = 24");

    // Step 3: BMU per-module reads at 0x50..0x54, IR(60,60). Each must populate
    // 24 cell voltages (IR 60-83) and a non-blank serial (IR 114-118).
    for slave in 0x50..=0x54 {
        let req = build_read_request(&serial, slave, FC_READ_INPUT, 60, 60);
        let raw = send_recv(&mut stream, &req).await;
        let (_, _, payload) = decode_response(&raw);
        let (_, _, data) = parse_read_payload(&payload);
        assert_eq!(data.len(), 60);
        // IR 60 first cell voltage non-zero
        assert_ne!(data[0], 0, "slave {slave:#04x} cell 0 voltage");
        // IR 114 serial high byte non-blank
        assert_ne!(
            (data[54] >> 8) as u8,
            b' ',
            "slave {slave:#04x} serial must be non-blank"
        );
    }
}
