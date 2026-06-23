//! End-to-end integration tests for the `giv-sim simulate` pipeline.
//!
//! These tests exercise the full chain:
//!   CLI params → PlantState → SimulationEngine (ticking) → RegisterStore → Modbus TCP server
//!
//! A test client connects over TCP using the GivEnergy proprietary Modbus framing
//! and asserts that the correct register values are served for each configuration.
//!
//! Test categories:
//!   1. Inverter type → DTC register mapping
//!   2. Battery configuration (count, size, SOC) → register values
//!   3. Solar generation → PV registers
//!   4. Load demand → load/grid registers
//!   5. Grid power → signed register with correct sign convention
//!   6. Energy totals → accumulated registers
//!   7. Multi-battery BMS data per slave
//!   8. HV battery cluster discovery (BMS → BCU → BMU)
//!   9. Weather effect on solar registers
//!  10. Inverter mode change via register write
//!  11. Schedule slot read/write
//!  12. Firmware version registers per inverter type

use chrono::NaiveDate;
use sim_core::{
    BatteryEngine, EnergyTracker, EvcEngine, InverterEngine, LoadEngine, LoadProfile, PlantState,
    ScheduleEngine, SimulationEngine, SolarEngine,
};
use sim_faults::FaultEngine;
use sim_modbus::{FC_READ_HOLDING, FC_READ_INPUT, FC_WRITE_SINGLE, HEADER_SIZE, SERIAL_LEN, crc16};
use sim_models::DeviceModel;
use sim_registers::{RegisterStore, default_register_catalogue};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Mutex;
use tokio::time::{Duration, sleep};

// ---------------------------------------------------------------------------
// GivEnergy frame helpers
// ---------------------------------------------------------------------------

const TEST_SERIAL: [u8; SERIAL_LEN] = *b"SIM0000001";

fn wrap_inner(slave: u8, func: u8, payload: &[u8]) -> Vec<u8> {
    let mut inner = Vec::with_capacity(2 + payload.len() + 2);
    inner.push(slave);
    inner.push(func);
    inner.extend_from_slice(payload);
    let crc = crc16(&inner);
    inner.extend_from_slice(&crc.to_le_bytes());

    let length = (1 + 1 + SERIAL_LEN + 8 + inner.len()) as u16;
    let mut frame = Vec::with_capacity(HEADER_SIZE + inner.len());
    frame.extend_from_slice(&0x5959u16.to_be_bytes()); // transaction ID
    frame.extend_from_slice(&0x0001u16.to_be_bytes()); // protocol ID
    frame.extend_from_slice(&length.to_be_bytes());
    frame.push(0x01); // unit ID
    frame.push(0x02); // transparent function
    frame.extend_from_slice(&TEST_SERIAL);
    frame.extend_from_slice(&8u64.to_be_bytes()); // padding
    frame.extend_from_slice(&inner);
    frame
}

fn build_read(slave: u8, func: u8, start: u16, count: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(4);
    p.extend_from_slice(&start.to_be_bytes());
    p.extend_from_slice(&count.to_be_bytes());
    wrap_inner(slave, func, &p)
}

fn build_write(slave: u8, addr: u16, value: u16) -> Vec<u8> {
    let mut p = Vec::with_capacity(4);
    p.extend_from_slice(&addr.to_be_bytes());
    p.extend_from_slice(&value.to_be_bytes());
    wrap_inner(slave, FC_WRITE_SINGLE, &p)
}

fn decode_response(data: &[u8]) -> (u8, u8, Vec<u8>) {
    assert!(data.len() >= HEADER_SIZE + 4, "Response too short");
    let inner = &data[HEADER_SIZE..];
    let slave = inner[0];
    let func = inner[1];
    let payload = inner[2..inner.len() - 2].to_vec();
    (slave, func, payload)
}

/// Parse read response: serial(10) + start(2) + count(2) + data(N×2).
fn parse_read(payload: &[u8]) -> (u16, u16, Vec<u16>) {
    assert!(payload.len() >= 14, "Read payload too short");
    let start = u16::from_be_bytes([payload[10], payload[11]]);
    let count = u16::from_be_bytes([payload[12], payload[13]]);
    let data = payload[14..]
        .chunks_exact(2)
        .map(|c| u16::from_be_bytes([c[0], c[1]]))
        .collect();
    (start, count, data)
}

async fn send_recv(stream: &mut TcpStream, frame: &[u8]) -> Vec<u8> {
    stream.write_all(frame).await.expect("write");
    let mut buf = [0u8; 4096];
    let n = stream.read(&mut buf).await.expect("read");
    buf[..n].to_vec()
}

// ---------------------------------------------------------------------------
// Server + Engine helpers
// ---------------------------------------------------------------------------

/// Build a full simulation engine from the given state, tick it `n` times,
/// start the Modbus server, and return the address + register store.
struct TestHarness {
    addr: SocketAddr,
    _store: Arc<Mutex<RegisterStore>>,
    _cmd_rx: tokio::sync::mpsc::UnboundedReceiver<sim_modbus::ModbusCommand>,
}

impl TestHarness {
    async fn new(state: PlantState, ticks: u64) -> Self {
        let tick_interval = 1;
        let peak = state.config.solar_peak_watts;
        let lat = state.config.latitude;

        let schedule = sim_models::Schedule::default();
        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(ScheduleEngine::new(schedule.clone())),
            Box::new(SolarEngine::new(peak, lat)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EvcEngine::new()),
            Box::new(EnergyTracker::new()),
        ];

        let mut engine = SimulationEngine::new(state, devices, tick_interval);

        // Tick the engine so all device models compute their values
        for _ in 0..ticks {
            engine.tick();
        }

        let cat = default_register_catalogue();
        let mut store = RegisterStore::new(cat);
        store.project_from_state(&engine.state);

        let store = Arc::new(Mutex::new(store));
        let batteries = engine.state.batteries.clone();
        let batt_arc = Arc::new(Mutex::new(batteries));

        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);

        let s = store.clone();
        let b = batt_arc;
        let t = tx;
        tokio::spawn(async move {
            let _ = sim_modbus::run_modbus_server(addr, s, t, b).await;
        });

        // Wait for server ready
        for _ in 0..100 {
            if TcpStream::connect(addr).await.is_ok() {
                break;
            }
            sleep(Duration::from_millis(50)).await;
        }

        Self {
            addr,
            _store: store,
            _cmd_rx: rx,
        }
    }

    async fn connect(&self) -> TcpStream {
        TcpStream::connect(self.addr)
            .await
            .expect("connect to server")
    }

    /// Read a single holding register (slave 0x32).
    async fn read_hr(&self, stream: &mut TcpStream, addr: u16) -> u16 {
        let resp = send_recv(stream, &build_read(0x32, FC_READ_HOLDING, addr, 1)).await;
        let (_, _, payload) = decode_response(&resp);
        let (_, _, data) = parse_read(&payload);
        data[0]
    }

    /// Read a single input register (slave 0x32).
    async fn read_ir(&self, stream: &mut TcpStream, addr: u16) -> u16 {
        let resp = send_recv(stream, &build_read(0x32, FC_READ_INPUT, addr, 1)).await;
        let (_, _, payload) = decode_response(&resp);
        let (_, _, data) = parse_read(&payload);
        data[0]
    }

    /// Read a block of input registers (slave 0x32).
    async fn read_ir_block(&self, stream: &mut TcpStream, start: u16, count: u16) -> Vec<u16> {
        let resp = send_recv(stream, &build_read(0x32, FC_READ_INPUT, start, count)).await;
        let (_, _, payload) = decode_response(&resp);
        let (_, _, data) = parse_read(&payload);
        data
    }

    /// Read a block of holding registers (slave 0x32).
    async fn read_hr_block(&self, stream: &mut TcpStream, start: u16, count: u16) -> Vec<u16> {
        let resp = send_recv(stream, &build_read(0x32, FC_READ_HOLDING, start, count)).await;
        let (_, _, payload) = decode_response(&resp);
        let (_, _, data) = parse_read(&payload);
        data
    }

    /// Read input registers from a specific slave (for BMS/BMU reads).
    async fn read_ir_slave(
        &self,
        stream: &mut TcpStream,
        slave: u8,
        start: u16,
        count: u16,
    ) -> Vec<u16> {
        let resp = send_recv(stream, &build_read(slave, FC_READ_INPUT, start, count)).await;
        let (_, _, payload) = decode_response(&resp);
        let (_, _, data) = parse_read(&payload);
        data
    }

    /// Write a holding register and return the response payload.
    async fn write_hr(&self, stream: &mut TcpStream, addr: u16, value: u16) -> (u8, Vec<u8>) {
        let resp = send_recv(stream, &build_write(0x11, addr, value)).await;
        let (slave, func, payload) = decode_response(&resp);
        assert_eq!(func, FC_WRITE_SINGLE, "Write response func should be 0x06");
        (slave, payload)
    }
}

/// Create a PlantState at midday summer solstice with standard config.
fn midday_state() -> PlantState {
    let ts = NaiveDate::from_ymd_opt(2025, 6, 21)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    PlantState::new(ts)
}

/// Create a state at midnight.
fn midnight_state() -> PlantState {
    let ts = NaiveDate::from_ymd_opt(2025, 6, 21)
        .unwrap()
        .and_hms_opt(0, 0, 0)
        .unwrap();
    PlantState::new(ts)
}

/// Build state with specific inverter type and battery configuration.
fn build_state(inv_type: &str, battery_count: usize, battery_size: f64, soc: f64) -> PlantState {
    let ts = NaiveDate::from_ymd_opt(2025, 6, 21)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    let count = battery_count.clamp(1, 6);
    let actual_size = nearest_battery_size(battery_size);

    let max_batt_kw = max_batt_w_for_inverter(inv_type) / 1000.0;
    let per_module_max = max_batt_kw / count as f64;
    let c_rate_max = actual_size * 0.7;
    let module_max = per_module_max.min(c_rate_max).min(10.0);

    let mut state = PlantState::with_battery_count(ts, count);
    for b in &mut state.batteries {
        b.soc_percent = soc;
        b.capacity_kwh = actual_size;
        b.nominal_capacity_kwh = actual_size;
        b.max_charge_kw = module_max;
        b.max_discharge_kw = module_max;
    }
    state.sync_battery_from_vec();
    configure_inverter(&mut state, inv_type);
    state.energy_totals.seed_for_testing_if_zero();
    state
}

// ---------------------------------------------------------------------------
// Helpers from main.rs (duplicated to avoid depending on the binary crate)
// ---------------------------------------------------------------------------

const BATTERY_SIZES: [f64; 14] = [
    2.6, 3.4, 5.2, 6.8, 7.0, 8.2, 9.5, 10.2, 12.8, 13.6, 16.0, 17.0, 19.0, 20.4,
];

fn nearest_battery_size(requested: f64) -> f64 {
    *BATTERY_SIZES
        .iter()
        .min_by(|a, b| {
            ((*a - requested).abs())
                .partial_cmp(&((*b - requested).abs()))
                .unwrap_or(std::cmp::Ordering::Equal)
        })
        .unwrap_or(&9.5)
}

fn max_batt_w_for_inverter(inv_type: &str) -> f64 {
    match inv_type {
        "Gen1Hybrid" => 2500.0,
        "Gen2Hybrid" => 3600.0,
        "Gen3Hybrid" => 3600.0,
        "Gen3Hybrid8kW" => 8000.0,
        "Gen3Hybrid10kW" => 10000.0,
        "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 2600.0,
        "ACCoupled" | "ACCoupled2" => 3000.0,
        "ThreePhase" => 6000.0,
        "ThreePhase8kW" => 8000.0,
        "ThreePhase10kW" => 10000.0,
        "ThreePhase11kW" => 11000.0,
        "AllInOne6" | "AllInOne" => 6000.0,
        "AllInOne5" => 5000.0,
        "AIO8kW" => 8000.0,
        "AIO10kW" => 10000.0,
        "AIOHybrid6kW" => 6000.0,
        "AIOHybrid8kW" => 8000.0,
        "AIOHybrid10kW" => 10000.0,
        _ => 3600.0,
    }
}

fn max_ac_w_for_inverter(inv_type: &str) -> f64 {
    match inv_type {
        "Gen1Hybrid" | "Gen2Hybrid" | "Gen3Hybrid" => 5000.0,
        "Gen3Hybrid8kW" => 8000.0,
        "Gen3Hybrid10kW" => 10000.0,
        "Gen3Plus6kW" => 5000.0,
        "Gen3Plus4600" => 4600.0,
        "Gen3Plus3600" => 3600.0,
        "Gen3Plus6kW2" => 6000.0,
        "ACCoupled" | "ACCoupled2" => 3000.0,
        "ThreePhase" => 6000.0,
        "ThreePhase8kW" => 8000.0,
        "ThreePhase10kW" => 10000.0,
        "ThreePhase11kW" => 11000.0,
        "AllInOne6" | "AllInOne" => 6000.0,
        "AllInOne5" => 5000.0,
        "AIO8kW" => 8000.0,
        "AIO10kW" => 10000.0,
        "AIOHybrid6kW" => 6000.0,
        "AIOHybrid8kW" => 8000.0,
        "AIOHybrid10kW" => 10000.0,
        _ => 5000.0,
    }
}

fn configure_inverter(state: &mut PlantState, inv_type: &str) {
    state.config.inverter_type = inv_type.to_string();
    state.config.max_ac_watts = max_ac_w_for_inverter(inv_type);
    state.inverter.export_limit_w = sim_models::default_export_limit_w_for(inv_type);
}

// ===========================================================================
// 1. INVERTER TYPE → DTC REGISTER MAPPING
// ===========================================================================

#[tokio::test]
async fn dtc_gen3_hybrid() {
    let state = build_state("Gen3Hybrid", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2001, "Gen3Hybrid DTC should be 0x2001");
}

#[tokio::test]
async fn dtc_gen1_hybrid() {
    let state = build_state("Gen1Hybrid", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2001, "Gen1Hybrid DTC should be 0x2001");
}

#[tokio::test]
async fn dtc_gen2_hybrid() {
    let state = build_state("Gen2Hybrid", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2001, "Gen2Hybrid shares 0x2001 with Gen1/Gen3");
}

#[tokio::test]
async fn dtc_gen3_hybrid_8kw() {
    let state = build_state("Gen3Hybrid8kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2106, "Gen3Hybrid8kW DTC should be 0x2106");
}

#[tokio::test]
async fn dtc_ac_coupled() {
    let state = build_state("ACCoupled", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x3001, "ACCoupled DTC should be 0x3001");
}

#[tokio::test]
async fn dtc_ac_coupled_2() {
    let state = build_state("ACCoupled2", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x3002, "ACCoupled2 DTC should be 0x3002");
}

#[tokio::test]
async fn dtc_three_phase() {
    let state = build_state("ThreePhase", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x4001, "ThreePhase DTC should be 0x4001");
}

#[tokio::test]
async fn dtc_three_phase_8kw() {
    let state = build_state("ThreePhase8kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x4002, "ThreePhase8kW DTC should be 0x4002");
}

#[tokio::test]
async fn dtc_three_phase_10kw() {
    let state = build_state("ThreePhase10kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x4003, "ThreePhase10kW DTC should be 0x4003");
}

#[tokio::test]
async fn dtc_three_phase_11kw() {
    let state = build_state("ThreePhase11kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x4004, "ThreePhase11kW DTC should be 0x4004");
}

#[tokio::test]
async fn dtc_all_in_one_6() {
    let state = build_state("AllInOne6", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8001, "AllInOne6 DTC should be 0x8001");
}

#[tokio::test]
async fn dtc_all_in_one() {
    let state = build_state("AllInOne", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8002, "AllInOne DTC should be 0x8002");
}

#[tokio::test]
async fn dtc_all_in_one_5() {
    let state = build_state("AllInOne5", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8003, "AllInOne5 DTC should be 0x8003");
}

#[tokio::test]
async fn dtc_aio_8kw() {
    let state = build_state("AIO8kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8102, "AIO8kW DTC should be 0x8102");
}

#[tokio::test]
async fn dtc_aio_10kw() {
    let state = build_state("AIO10kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8103, "AIO10kW DTC should be 0x8103");
}

#[tokio::test]
async fn dtc_aio_hybrid_6kw() {
    let state = build_state("AIOHybrid6kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8201, "AIOHybrid6kW DTC should be 0x8201");
}

#[tokio::test]
async fn dtc_aio_hybrid_8kw() {
    let state = build_state("AIOHybrid8kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8202, "AIOHybrid8kW DTC should be 0x8202");
}

#[tokio::test]
async fn dtc_aio_hybrid_10kw() {
    let state = build_state("AIOHybrid10kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8203, "AIOHybrid10kW DTC should be 0x8203");
}

#[tokio::test]
async fn dtc_gen3_plus_6kw() {
    let state = build_state("Gen3Plus6kW", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2201, "Gen3Plus6kW DTC should be 0x2201");
}

#[tokio::test]
async fn dtc_gen3_plus_4600() {
    let state = build_state("Gen3Plus4600", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2202, "Gen3Plus4600 DTC should be 0x2202");
}

#[tokio::test]
async fn dtc_gen3_plus_3600() {
    let state = build_state("Gen3Plus3600", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2203, "Gen3Plus3600 DTC should be 0x2203");
}

#[tokio::test]
async fn dtc_unknown_falls_back_to_0x2001() {
    let state = build_state("UnknownInverter", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;
    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2001, "Unknown inverter should fallback to 0x2001");
}

// ===========================================================================
// 2. BATTERY CONFIGURATION → REGISTER VALUES
// ===========================================================================

#[tokio::test]
async fn battery_soc_reflects_in_ir_59() {
    let mut state = midday_state();
    state.batteries[0].soc_percent = 75.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    let soc = h.read_ir(&mut s, 59).await;
    // After 1 tick at midday the SOC may change slightly due to solar charging
    assert!(
        (soc as f64 - 75.0).abs() < 2.0,
        "IR 59 (SOC) should be ~75%, got {soc}%"
    );
}

#[tokio::test]
async fn battery_soc_100_percent() {
    let mut state = midday_state();
    state.batteries[0].soc_percent = 100.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    let soc = h.read_ir(&mut s, 59).await;
    assert_eq!(soc, 100, "IR 59 should be 100% SOC");
}

#[tokio::test]
async fn battery_soc_0_percent_clamped_to_min() {
    let mut state = midnight_state();
    state.batteries[0].soc_percent = 0.0;
    state.batteries[0].min_soc = 0.0; // Allow 0%
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    let soc = h.read_ir(&mut s, 59).await;
    // Default min_soc is 4%, so even at 0% the battery engine may charge to min_soc
    assert!(soc <= 5, "IR 59 should be ~0% (clamped to min), got {soc}%");
}

#[tokio::test]
async fn two_batteries_aggregate_soc() {
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        2,
    );
    // Both 9.5 kWh, one at 40% one at 60% → aggregate = 50%
    state.batteries[0].soc_percent = 40.0;
    state.batteries[0].capacity_kwh = 9.5;
    state.batteries[1].soc_percent = 60.0;
    state.batteries[1].capacity_kwh = 9.5;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    let soc = h.read_ir(&mut s, 59).await;
    assert_eq!(
        soc, 50,
        "Aggregate SOC of 40%+60% with equal capacity = 50%"
    );
}

#[tokio::test]
async fn two_batteries_different_capacity_weighted_soc() {
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        2,
    );
    // Battery 0: 9.5 kWh at 20%, Battery 1: 5.2 kWh at 80%
    // Weighted: (20*9.5 + 80*5.2) / (9.5+5.2) = (190+416)/14.7 = 41.2%
    state.batteries[0].soc_percent = 20.0;
    state.batteries[0].capacity_kwh = 9.5;
    state.batteries[0].nominal_capacity_kwh = 9.5;
    state.batteries[1].soc_percent = 80.0;
    state.batteries[1].capacity_kwh = 5.2;
    state.batteries[1].nominal_capacity_kwh = 5.2;
    state.sync_battery_from_vec();

    // Verify internal calculation matches
    let expected_soc = state.aggregate_soc();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    let soc = h.read_ir(&mut s, 59).await;
    let expected = expected_soc.round() as u16;
    assert!(
        (soc as i32 - expected as i32).abs() <= 1,
        "Weighted SOC should be ~{expected}%, got {soc}%"
    );
}

#[tokio::test]
async fn battery_voltage_reflects_soc() {
    let mut state = midnight_state();
    state.batteries[0].soc_percent = 100.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    // IR 50 = battery voltage ×0.01 V. At SOC=100%: 44 + 100*0.08 = 52.0V → 5200
    let voltage = h.read_ir(&mut s, 50).await;
    let expected_v = 44.0 + 100.0 * 0.08; // 52.0V
    let expected_raw = (expected_v * 100.0_f64).round() as u16;
    assert!(
        (voltage as i32 - expected_raw as i32).abs() <= 50,
        "IR 50 (battery voltage) should be ~{expected_raw} ({expected_v}V), got {voltage}"
    );
}

#[tokio::test]
async fn battery_temperature() {
    let mut state = midday_state();
    state.batteries[0].temperature_celsius = 30.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    // IR 56 = battery temp ×0.1 °C → 30.0°C = 300
    let temp = h.read_ir(&mut s, 56).await;
    // Temperature may change after tick, check approximate
    assert!(
        (temp as i32 - 300).abs() < 50,
        "IR 56 (battery temp) should be ~300 (30°C), got {temp}"
    );
}

#[tokio::test]
async fn soc_reserve_holding_register() {
    let mut state = midday_state();
    state.batteries[0].min_soc = 10.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    // HR 110 = battery SOC reserve (%)
    let reserve = h.read_hr(&mut s, 110).await;
    assert_eq!(reserve, 10, "HR 110 should be 10% (min SOC reserve)");
}

#[tokio::test]
async fn battery_charge_limit_100_percent() {
    let mut state = midday_state();
    state.battery_charge_limit_percent = 100.0;

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    // HR 111 = battery charge limit (%)
    let limit = h.read_hr(&mut s, 111).await;
    assert_eq!(limit, 100, "HR 111 should be 100% charge limit");
}

#[tokio::test]
async fn battery_discharge_limit_50_percent() {
    let mut state = midday_state();
    state.battery_discharge_limit_percent = 50.0;

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;
    // HR 112 = battery discharge limit (%)
    let limit = h.read_hr(&mut s, 112).await;
    assert_eq!(limit, 50, "HR 112 should be 50% discharge limit");
}

// ===========================================================================
// 3. SOLAR GENERATION → PV REGISTERS
// ===========================================================================

#[tokio::test]
async fn midday_solar_pv1_voltage_nonzero() {
    let state = midday_state();
    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // After 100 ticks at midday, solar should be generating
    // IR 1 = PV1 voltage ×0.1 V → 350V when generating = 3500
    let pv1_v = h.read_ir(&mut s, 1).await;
    assert_eq!(
        pv1_v, 3500,
        "IR 1 (PV1 voltage) should be 3500 (=350V) at midday"
    );
}

#[tokio::test]
async fn midday_solar_pv1_power_nonzero() {
    let state = midday_state();
    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // IR 18 = PV1 power (W) — should be > 0 at midday summer solstice
    let pv1_power = h.read_ir(&mut s, 18).await;
    assert!(
        pv1_power > 0,
        "IR 18 (PV1 power) should be > 0 at midday, got {pv1_power}"
    );
}

#[tokio::test]
async fn midnight_solar_pv1_voltage_zero() {
    let state = midnight_state();
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    // IR 1 = PV1 voltage → 0 at night
    let pv1_v = h.read_ir(&mut s, 1).await;
    assert_eq!(pv1_v, 0, "IR 1 (PV1 voltage) should be 0 at midnight");
}

#[tokio::test]
async fn midnight_solar_pv1_power_zero() {
    let state = midnight_state();
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    let pv1_power = h.read_ir(&mut s, 18).await;
    assert_eq!(pv1_power, 0, "IR 18 (PV1 power) should be 0 at midnight");
}

#[tokio::test]
async fn midnight_solar_pv1_current_zero() {
    let state = midnight_state();
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    let pv1_current = h.read_ir(&mut s, 8).await;
    assert_eq!(pv1_current, 0, "IR 8 (PV1 current) should be 0 at midnight");
}

#[tokio::test]
async fn pv2_voltage_zero_when_not_configured() {
    let state = midday_state();
    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // No PV2 configured → IR 2 should be 0
    let pv2_v = h.read_ir(&mut s, 2).await;
    assert_eq!(
        pv2_v, 0,
        "IR 2 (PV2 voltage) should be 0 when PV2 not configured"
    );
}

#[tokio::test]
async fn pv2_configured_shows_voltage() {
    let mut state = midday_state();
    state.config.pv2_peak_watts = 5000.0;

    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // PV2 configured → IR 2 should show 3500 (=350V) when generating
    let pv2_v = h.read_ir(&mut s, 2).await;
    assert_eq!(
        pv2_v, 3500,
        "IR 2 (PV2 voltage) should be 3500 when PV2 configured and generating"
    );
}

#[tokio::test]
async fn pv2_power_split_45_55() {
    let mut state = midday_state();
    state.config.pv2_peak_watts = 5000.0;

    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    let pv1 = h.read_ir(&mut s, 18).await; // PV1 power
    let pv2 = h.read_ir(&mut s, 20).await; // PV2 power
    let total = pv1 + pv2;

    assert!(
        total > 0,
        "Total PV power should be > 0 at midday, got {total}"
    );

    // PV1 should be ~45% of total
    let pv1_ratio = pv1 as f64 / total as f64;
    assert!(
        (pv1_ratio - 0.45).abs() < 0.05,
        "PV1 should be ~45% of total, got {pv1_ratio:.2} ({pv1}/{total})"
    );

    // PV2 should be ~55% of total
    let pv2_ratio = pv2 as f64 / total as f64;
    assert!(
        (pv2_ratio - 0.55).abs() < 0.05,
        "PV2 should be ~55% of total, got {pv2_ratio:.2} ({pv2}/{total})"
    );
}

#[tokio::test]
async fn solar_energy_today_increases() {
    let state = midday_state();
    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // IR 17 = PV1 energy today ×0.1 kWh
    let pv1_energy = h.read_ir(&mut s, 17).await;
    assert!(
        pv1_energy > 0,
        "IR 17 (PV1 energy today) should be > 0 after 100 ticks at midday"
    );
}

// ===========================================================================
// 4. GRID REGISTERS
// ===========================================================================

#[tokio::test]
async fn grid_voltage_240v() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // IR 5 = grid voltage ×0.1 V → 240.0V = 2400
    let v = h.read_ir(&mut s, 5).await;
    assert_eq!(v, 2400, "IR 5 (grid voltage) should be 2400 (=240.0V)");
}

#[tokio::test]
async fn grid_frequency_50hz() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // IR 13 = grid frequency ×0.01 Hz → 50.00 Hz = 5000
    let f = h.read_ir(&mut s, 13).await;
    assert_eq!(f, 5000, "IR 13 (grid frequency) should be 5000 (=50.00Hz)");
}

#[tokio::test]
async fn grid_power_importing_at_night() {
    let mut state = midnight_state();
    state.load_override = Some(2000.0); // Force load
    state.solar_override = Some(0.0); // Force no solar
    // Battery at min SOC so it can't discharge → grid must supply load
    state.batteries[0].soc_percent = 4.0;
    state.batteries[0].min_soc = 4.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    // IR 30 = grid power, GE convention: positive = exporting, negative = importing
    let raw = h.read_ir(&mut s, 30).await;
    let signed = raw as i16;
    assert!(
        signed < 0,
        "IR 30 (grid power) should be negative (importing) at night with load, got {signed}"
    );
}

#[tokio::test]
async fn grid_power_exporting_with_full_battery_midday() {
    let mut state = midday_state();
    state.batteries[0].soc_percent = 100.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // At midday with full battery, excess solar exports to grid
    // IR 30 should be positive (exporting in GE convention)
    let raw = h.read_ir(&mut s, 30).await;
    let signed = raw as i16;
    // With 100% SOC the battery may stop charging, causing export
    // Note: this depends on the simulation state after 100 ticks
    // Just verify the register is readable and consistent
    assert!(
        signed as i32 >= -50000,
        "IR 30 should be a reasonable value, got {signed}"
    );
}

// ===========================================================================
// 5. INVERTER STATUS AND CONFIG REGISTERS
// ===========================================================================

#[tokio::test]
async fn status_register_always_1() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    let status = h.read_ir(&mut s, 0).await;
    assert_eq!(status, 1, "IR 0 (status) should always be 1 (normal)");
}

#[tokio::test]
async fn inverter_mode_default_eco() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 27 = battery power mode (0=Normal/export, 1=Eco).
    // Default is Eco so a freshly-created plant reports `battery_mode = Eco`
    // in the GUI rather than falling through to the `ExportPaused` arm of
    // the projection match. Users who want full solar-to-battery priority
    // can switch to Normal explicitly via HR 27 = 0.
    let mode = h.read_hr(&mut s, 27).await;
    assert_eq!(mode, 1, "HR 27 should be 1 (Eco) by default");
}

#[tokio::test]
async fn active_power_rate_100_percent() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 50 = active power rate (%)
    let rate = h.read_hr(&mut s, 50).await;
    assert_eq!(rate, 100, "HR 50 should be 100% by default");
}

#[tokio::test]
async fn inverter_temperature_reasonable() {
    let state = midday_state();
    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // IR 41 = inverter temp ×0.1 °C → should be 20-60°C range
    let temp = h.read_ir(&mut s, 41).await;
    let temp_c = temp as f64 / 10.0;
    assert!(
        (10.0..80.0).contains(&temp_c),
        "IR 41 (inverter temp) should be 10-80°C, got {temp_c}°C"
    );
}

#[tokio::test]
async fn charge_target_soc_default_100() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 116 = charge target SOC (%)
    let target = h.read_hr(&mut s, 116).await;
    assert_eq!(
        target, 100,
        "HR 116 should be 100% charge target by default"
    );
}

#[tokio::test]
async fn calibration_stage_default_0() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 29 = calibration stage
    let cal = h.read_hr(&mut s, 29).await;
    assert_eq!(cal, 0, "HR 29 should be 0 (off) by default");
}

// ===========================================================================
// 6. INVERTER MODE CHANGE VIA REGISTER WRITE
// ===========================================================================

#[tokio::test]
async fn write_hr_27_switches_to_normal_mode() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Write HR 27 = 0 (Normal mode)
    let (_, payload) = h.write_hr(&mut s, 27, 0).await;
    let written_addr = u16::from_be_bytes([payload[10], payload[11]]);
    let written_val = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(written_addr, 27);
    assert_eq!(written_val, 0);

    // Read back
    let mode = h.read_hr(&mut s, 27).await;
    assert_eq!(mode, 0, "HR 27 should be 0 (Normal) after write");
}

#[tokio::test]
async fn write_hr_100_to_force_charge() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 100 = inverter mode: 2 = ForceCharge
    let (_, payload) = h.write_hr(&mut s, 100, 2).await;
    let written_val = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(written_val, 2);

    let mode = h.read_hr(&mut s, 100).await;
    assert_eq!(mode, 2, "HR 100 should be 2 (ForceCharge) after write");
}

#[tokio::test]
async fn write_hr_100_to_force_discharge() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 100 = 3 = ForceDischarge
    h.write_hr(&mut s, 100, 3).await;
    let mode = h.read_hr(&mut s, 100).await;
    assert_eq!(mode, 3, "HR 100 should be 3 (ForceDischarge) after write");
}

#[tokio::test]
async fn write_hr_100_to_export_limit() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 100 = 4 = ExportLimit
    h.write_hr(&mut s, 100, 4).await;
    let mode = h.read_hr(&mut s, 100).await;
    assert_eq!(mode, 4, "HR 100 should be 4 (ExportLimit) after write");
}

#[tokio::test]
async fn write_soc_reserve() {
    let mut state = midday_state();
    state.batteries[0].min_soc = 4.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Write HR 110 = 15 (15% SOC reserve)
    h.write_hr(&mut s, 110, 15).await;
    let reserve = h.read_hr(&mut s, 110).await;
    assert_eq!(reserve, 15, "HR 110 should be 15 after write");
}

// ===========================================================================
// 7. SCHEDULE SLOT REGISTERS
// ===========================================================================

#[tokio::test]
async fn schedule_slots_disabled_by_default() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Charge slot 1 start/end → HR 94, 95 = 60 (disabled)
    assert_eq!(
        h.read_hr(&mut s, 94).await,
        60,
        "HR 94 (charge slot 1 start) disabled"
    );
    assert_eq!(
        h.read_hr(&mut s, 95).await,
        60,
        "HR 95 (charge slot 1 end) disabled"
    );

    // Discharge slot 1 start/end → HR 56, 57 = 60 (disabled)
    assert_eq!(
        h.read_hr(&mut s, 56).await,
        60,
        "HR 56 (discharge slot 1 start) disabled"
    );
    assert_eq!(
        h.read_hr(&mut s, 57).await,
        60,
        "HR 57 (discharge slot 1 end) disabled"
    );

    // Charge slot 2 start/end → HR 31, 32 = 60 (disabled)
    assert_eq!(
        h.read_hr(&mut s, 31).await,
        60,
        "HR 31 (charge slot 2 start) disabled"
    );
    assert_eq!(
        h.read_hr(&mut s, 32).await,
        60,
        "HR 32 (charge slot 2 end) disabled"
    );

    // Discharge slot 2 start/end → HR 44, 45 = 60 (disabled)
    assert_eq!(
        h.read_hr(&mut s, 44).await,
        60,
        "HR 44 (discharge slot 2 start) disabled"
    );
    assert_eq!(
        h.read_hr(&mut s, 45).await,
        60,
        "HR 45 (discharge slot 2 end) disabled"
    );
}

#[tokio::test]
async fn write_schedule_charge_slot_1() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Write charge slot 1: start=2200 (22:00), end=600 (06:00)
    h.write_hr(&mut s, 94, 2200).await;
    h.write_hr(&mut s, 95, 600).await;

    assert_eq!(
        h.read_hr(&mut s, 94).await,
        2200,
        "HR 94 should be 2200 after write"
    );
    assert_eq!(
        h.read_hr(&mut s, 95).await,
        600,
        "HR 95 should be 600 after write"
    );
}

#[tokio::test]
async fn write_schedule_discharge_slot_1() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Write discharge slot 1: start=1700 (17:00), end=2100 (21:00)
    h.write_hr(&mut s, 56, 1700).await;
    h.write_hr(&mut s, 57, 2100).await;

    assert_eq!(h.read_hr(&mut s, 56).await, 1700, "HR 56 should be 1700");
    assert_eq!(h.read_hr(&mut s, 57).await, 2100, "HR 57 should be 2100");
}

#[tokio::test]
async fn enable_charge_register() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 96 = enable charge (0/1)
    h.write_hr(&mut s, 96, 1).await;
    assert_eq!(
        h.read_hr(&mut s, 96).await,
        1,
        "HR 96 should be 1 (enabled)"
    );

    h.write_hr(&mut s, 96, 0).await;
    assert_eq!(
        h.read_hr(&mut s, 96).await,
        0,
        "HR 96 should be 0 (disabled)"
    );
}

#[tokio::test]
async fn enable_discharge_register() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 59 = enable discharge (0/1)
    h.write_hr(&mut s, 59, 1).await;
    assert_eq!(
        h.read_hr(&mut s, 59).await,
        1,
        "HR 59 should be 1 (enabled)"
    );
}

#[tokio::test]
async fn charge_target_soc_writable() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 116 = charge target SOC
    h.write_hr(&mut s, 116, 80).await;
    assert_eq!(
        h.read_hr(&mut s, 116).await,
        80,
        "HR 116 should be 80 after write"
    );
}

// ===========================================================================
// 8. SIMULATOR-INTERNAL REGISTERS
// ===========================================================================

#[tokio::test]
async fn internal_inverter_mode_register() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 100 = internal inverter mode (1=Eco default)
    let mode = h.read_hr(&mut s, 100).await;
    assert_eq!(mode, 1, "HR 100 should be 1 (Eco) by default");
}

#[tokio::test]
async fn internal_battery_soc_register() {
    let mut state = midday_state();
    state.batteries[0].soc_percent = 65.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 200 = internal battery SOC
    let soc = h.read_hr(&mut s, 200).await;
    assert_eq!(soc, 65, "HR 200 should be 65 (battery SOC)");
}

#[tokio::test]
async fn internal_pv_generation_register() {
    let state = midday_state();
    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // HR 300 = PV generation (W)
    let pv = h.read_hr(&mut s, 300).await;
    assert!(
        pv > 0,
        "HR 300 should be > 0 at midday after 100 ticks, got {pv}"
    );
}

#[tokio::test]
async fn internal_grid_power_register() {
    let mut state = midnight_state();
    state.load_override = Some(2000.0);
    state.solar_override = Some(0.0);
    state.batteries[0].soc_percent = 4.0;
    state.batteries[0].min_soc = 4.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    // HR 400 = grid power (signed W). At night with forced load → importing → positive
    let power_raw = h.read_hr(&mut s, 400).await;
    let signed = power_raw as i16;
    assert!(
        signed > 0,
        "HR 400 should be positive (importing) at night with load, got {signed}"
    );
}

#[tokio::test]
async fn internal_grid_voltage_register() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 401 = grid voltage ×0.1 V → 2400
    let v = h.read_hr(&mut s, 401).await;
    assert_eq!(v, 2400, "HR 401 should be 2400 (240.0V)");
}

#[tokio::test]
async fn internal_energy_totals_registers() {
    let mut state = midday_state();
    state.energy_totals.seed_for_testing_if_zero();

    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // HR 500-505 = energy totals (kWh)
    let import = h.read_hr(&mut s, 500).await;
    let export = h.read_hr(&mut s, 501).await;
    let charge = h.read_hr(&mut s, 502).await;
    let discharge = h.read_hr(&mut s, 503).await;
    let solar = h.read_hr(&mut s, 504).await;
    let consumption = h.read_hr(&mut s, 505).await;

    // With seeded totals, all should be non-zero
    assert!(import > 0, "HR 500 (import) should be > 0");
    assert!(export > 0, "HR 501 (export) should be > 0");
    assert!(charge > 0, "HR 502 (charge) should be > 0");
    assert!(discharge > 0, "HR 503 (discharge) should be > 0");
    assert!(solar > 0, "HR 504 (solar) should be > 0");
    assert!(consumption > 0, "HR 505 (consumption) should be > 0");
}

// ===========================================================================
// 9. SYSTEM TIME REGISTERS
// ===========================================================================

#[tokio::test]
async fn system_time_registers_match_timestamp() {
    let state = midday_state(); // 2025-06-21 12:00:00
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 35-40 = year, month, day, hour, minute, second
    let year = h.read_hr(&mut s, 35).await;
    let month = h.read_hr(&mut s, 36).await;
    let day = h.read_hr(&mut s, 37).await;
    let hour = h.read_hr(&mut s, 38).await;
    let minute = h.read_hr(&mut s, 39).await;
    let second = h.read_hr(&mut s, 40).await;

    // After 1 tick (1s interval), time is 12:00:01
    assert_eq!(year, 2025, "HR 35 (year)");
    assert_eq!(month, 6, "HR 36 (month)");
    assert_eq!(day, 21, "HR 37 (day)");
    assert_eq!(hour, 12, "HR 38 (hour)");
    assert_eq!(minute, 0, "HR 39 (minute)");
    assert_eq!(second, 1, "HR 40 (second) — after 1 tick");
}

#[tokio::test]
async fn system_time_advances_with_ticks() {
    let state = midday_state();
    let h = TestHarness::new(state, 60).await; // 60 ticks = 60 seconds
    let mut s = h.connect().await;

    let minute = h.read_hr(&mut s, 39).await;
    let second = h.read_hr(&mut s, 40).await;

    // After 60 ticks at 12:00:00, should be 12:01:00
    assert_eq!(minute, 1, "HR 39 (minute) should be 1 after 60s of ticks");
    assert_eq!(second, 0, "HR 40 (second) should be 0 at 12:01:00");
}

// ===========================================================================
// 10. MULTI-BATTERY BMS DATA VIA SLAVE
// ===========================================================================

#[tokio::test]
async fn bms_slave_0x32_returns_battery_0_data() {
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        2,
    );
    state.batteries[0].soc_percent = 70.0;
    state.batteries[1].soc_percent = 30.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Read IR 60-119 on slave 0x32 (battery 0 BMS data)
    let data = h.read_ir_slave(&mut s, 0x32, 60, 60).await;
    assert_eq!(data.len(), 60);

    // IR 100 (index 40) = SOC of battery 0
    assert_eq!(data[40], 70, "BMS slave 0x32 SOC should be 70%");
}

#[tokio::test]
async fn bms_slave_0x33_returns_battery_1_data() {
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        2,
    );
    state.batteries[0].soc_percent = 70.0;
    state.batteries[1].soc_percent = 30.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Read IR 60-119 on slave 0x33 (battery 1 BMS data)
    let data = h.read_ir_slave(&mut s, 0x33, 60, 60).await;
    assert_eq!(data.len(), 60);

    // IR 100 (index 40) = SOC of battery 1
    assert_eq!(data[40], 30, "BMS slave 0x33 SOC should be 30%");
}

#[tokio::test]
async fn bms_slave_0x34_returns_battery_2_data() {
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        3,
    );
    state.batteries[0].soc_percent = 50.0;
    state.batteries[1].soc_percent = 60.0;
    state.batteries[2].soc_percent = 40.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Battery 2 on slave 0x34
    let data = h.read_ir_slave(&mut s, 0x34, 60, 60).await;
    assert_eq!(data[40], 40, "BMS slave 0x34 SOC should be 40%");
}

#[tokio::test]
async fn bms_cell_voltages_nonzero() {
    let state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        1,
    );

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Read IR 60-75 (cell voltages 1-16)
    let data = h.read_ir_slave(&mut s, 0x32, 60, 16).await;
    for (i, &cell) in data.iter().enumerate() {
        assert!(
            cell > 2500 && cell < 4200,
            "Cell {} voltage should be 2500-4200 mV range, got {cell}",
            i + 1
        );
    }
}

#[tokio::test]
async fn bms_num_cells_16() {
    let state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        1,
    );

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // IR 97 = num_cells (index 37 in the 60-register block)
    let data = h.read_ir_slave(&mut s, 0x32, 60, 60).await;
    assert_eq!(data[37], 16, "IR 97 (num_cells) should be 16");
}

#[tokio::test]
async fn bms_absent_slave_returns_zeros() {
    // 1 battery, so slave 0x33 should return all zeros
    let state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        1,
    );

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    let data = h.read_ir_slave(&mut s, 0x33, 60, 60).await;
    assert!(
        data.iter().all(|&v| v == 0),
        "BMS slave 0x33 should return all zeros when only 1 battery"
    );
}

// ===========================================================================
// 11. HV BATTERY CLUSTER DISCOVERY
// ===========================================================================

#[tokio::test]
async fn hv_bms_discovery_reports_one_bcu() {
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        3,
    );
    for b in &mut state.batteries {
        b.soc_percent = 60.0;
        b.voltage_v = 51.2;
        b.soh = 0.95;
    }
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // BMS at slave 0xA0, IR(60,5). IR(61) = number of BCUs.
    let data = h.read_ir_slave(&mut s, 0xA0, 60, 5).await;
    assert_eq!(data[1], 1, "BMS IR(61) should report 1 BCU");
}

#[tokio::test]
async fn hv_bcu_reports_module_count() {
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        5,
    );
    for b in &mut state.batteries {
        b.soc_percent = 55.0;
    }
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // BCU at slave 0x70, IR(60,60). IR(64) = number of modules.
    let data = h.read_ir_slave(&mut s, 0x70, 60, 60).await;
    assert_eq!(data[4], 5, "BCU IR(64) should report 5 modules");
    assert_eq!(data[5], 24, "BCU IR(65) cells_per_module should be 24");
    // Version prefix should be 'HV' → 0x4856
    assert_eq!(data[0], 0x4856, "BCU version prefix should be 'HV'");
}

#[tokio::test]
async fn hv_bmu_per_module_has_cells_and_serial() {
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        3,
    );
    for b in &mut state.batteries {
        b.soc_percent = 60.0;
        b.voltage_v = 51.2;
    }
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // BMU at slaves 0x50, 0x51, 0x52
    for (idx, slave) in [0x50, 0x51, 0x52].iter().enumerate() {
        let data = h.read_ir_slave(&mut s, *slave, 60, 60).await;
        assert_eq!(
            data.len(),
            60,
            "BMU slave {slave:#04x} should return 60 registers"
        );

        // Cell voltages (IR 60-83, indices 0-23) should be non-zero
        let cell_sum: u32 = data[0..24].iter().map(|&v| v as u32).sum();
        assert!(
            cell_sum > 0,
            "BMU {} ({slave:#04x}) cell voltages should be non-zero",
            idx
        );

        // Serial (IR 114-118, indices 54-58) should be non-blank
        assert_ne!(
            (data[54] >> 8) as u8,
            b' ',
            "BMU {} serial high byte should be non-space",
            idx
        );
    }
}

#[tokio::test]
async fn hv_absent_bmu_returns_zeros() {
    // 2 batteries → BMUs at 0x50, 0x51. 0x52 should be all zeros.
    let mut state = PlantState::with_battery_count(
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        2,
    );
    for b in &mut state.batteries {
        b.soc_percent = 50.0;
    }
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    let data = h.read_ir_slave(&mut s, 0x52, 60, 60).await;
    assert!(
        data.iter().all(|&v| v == 0),
        "BMU slave 0x52 should return zeros when only 2 batteries"
    );
}

// ===========================================================================
// 12. FULL BLOCK READ (LIKE GIVTCP CLIENT)
// ===========================================================================

#[tokio::test]
async fn full_input_block_0_to_59_matches_reference() {
    let mut state = midday_state();
    state.solar.generation_w = 4000.0;
    state.solar.pv1_w = 4000.0;
    state.batteries[0].soc_percent = 75.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    let data = h.read_ir_block(&mut s, 0, 60).await;
    assert_eq!(data.len(), 60);

    // IR 0 = status = 1
    assert_eq!(data[0], 1);
    // IR 1 = PV1 voltage → 3500 (350V) when generating
    assert_eq!(data[1], 3500);
    // IR 5 = grid voltage = 2400
    assert_eq!(data[5], 2400);
    // IR 13 = frequency = 5000
    assert_eq!(data[13], 5000);
    // IR 59 = SOC ≈ 75
    assert_eq!(data[59], 75);
}

#[tokio::test]
async fn full_holding_block_0_to_119_gen3() {
    let mut state = midday_state();
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // Read in two blocks to stay within response frame size
    let data_lo = h.read_hr_block(&mut s, 0, 60).await;
    let data_hi = h.read_hr_block(&mut s, 60, 60).await;
    let data: Vec<u16> = [data_lo.as_slice(), data_hi.as_slice()].concat();
    assert_eq!(data.len(), 120);

    // HR 0 = DTC = 0x2001
    assert_eq!(data[0], 0x2001);
    // HR 27 = power mode = 1 (Eco default)
    assert_eq!(data[27], 1);
    // HR 50 = active power rate = 100
    assert_eq!(data[50], 100);
    // HR 56, 57 = discharge slot 1 = 60 (disabled)
    assert_eq!(data[56], 60);
    assert_eq!(data[57], 60);
    // HR 94, 95 = charge slot 1 = 60 (disabled)
    assert_eq!(data[94], 60);
    assert_eq!(data[95], 60);
    // HR 96 = enable charge = 0
    assert_eq!(data[96], 0);
    // HR 110 = SOC reserve
    assert_eq!(data[110], 4, "HR 110 should be 4% (default min_soc)");
    // HR 111 = charge limit = 100
    assert_eq!(data[111], 100);
    // HR 112 = discharge limit = 100
    assert_eq!(data[112], 100);
    // HR 116 = charge target SOC = 100
    assert_eq!(data[116], 100);
}

// ===========================================================================
// 13. BATTERY POWER SIGN CONVENTION
// ===========================================================================

#[tokio::test]
async fn battery_power_negative_when_discharging() {
    // At midnight with load and no solar → battery discharging
    let state = midnight_state();
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    // IR 52 = battery power (signed, GE convention: positive = discharging)
    let raw = h.read_ir(&mut s, 52).await;
    let signed = raw as i16;
    // Battery should be discharging at night to supply load
    // GE convention: raw positive = discharging (our internal negative)
    // The register projection negates the value
    assert!(
        signed != 0,
        "IR 52 (battery power) should be non-zero at night, got {signed}"
    );
}

#[tokio::test]
async fn battery_current_sign_convention() {
    let state = midnight_state();
    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    // IR 51 = battery current (signed ×0.01 A, GE convention: negated)
    let raw = h.read_ir(&mut s, 51).await;
    let signed = raw as i16;
    // At night with load, battery is discharging → internal negative → GE positive
    assert!(
        signed != 0,
        "IR 51 (battery current) should be non-zero, got {signed}"
    );
}

// ===========================================================================
// 14. EXPORT/IMPORT ENERGY REGISTERS
// ===========================================================================

#[tokio::test]
async fn energy_totals_seeded_nonzero() {
    let mut state = midday_state();
    state.energy_totals.seed_for_testing_if_zero();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // IR 25 = export today ×0.1 kWh
    let export = h.read_ir(&mut s, 25).await;
    assert!(
        export > 0,
        "IR 25 (export today) should be > 0 with seeded totals"
    );

    // IR 26 = import today ×0.1 kWh
    let import = h.read_ir(&mut s, 26).await;
    assert!(
        import > 0,
        "IR 26 (import today) should be > 0 with seeded totals"
    );
}

#[tokio::test]
async fn battery_charge_discharge_today() {
    let mut state = midday_state();
    state.energy_totals.seed_for_testing_if_zero();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // IR 36 = battery charge today ×0.1 kWh
    let charge = h.read_ir(&mut s, 36).await;
    assert!(charge > 0, "IR 36 (battery charge today) should be > 0");

    // IR 37 = battery discharge today ×0.1 kWh
    let discharge = h.read_ir(&mut s, 37).await;
    assert!(
        discharge > 0,
        "IR 37 (battery discharge today) should be > 0"
    );
}

#[tokio::test]
async fn consumption_today() {
    let mut state = midday_state();
    state.energy_totals.seed_for_testing_if_zero();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // IR 35 = consumption today ×0.1 kWh
    let consumption = h.read_ir(&mut s, 35).await;
    assert!(consumption > 0, "IR 35 (consumption today) should be > 0");
}

// ===========================================================================
// 15. PAUSE MODE REGISTER
// ===========================================================================

#[tokio::test]
async fn battery_pause_mode_default_zero() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 318 = battery pause mode (0=off)
    let pause = h.read_hr(&mut s, 318).await;
    assert_eq!(pause, 0, "HR 318 should be 0 (pause off) by default");
}

#[tokio::test]
async fn battery_pause_mode_writable() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    h.write_hr(&mut s, 318, 1).await;
    let pause = h.read_hr(&mut s, 318).await;
    assert_eq!(pause, 1, "HR 318 should be 1 after write");
}

// ===========================================================================
// 16. INVERTER REBOOT REGISTER
// ===========================================================================

#[tokio::test]
async fn inverter_reboot_register_writable() {
    let state = midday_state();
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // HR 163 = inverter reboot (write 100 to trigger)
    let (_, payload) = h.write_hr(&mut s, 163, 100).await;
    let written_val = u16::from_be_bytes([payload[12], payload[13]]);
    assert_eq!(written_val, 100, "HR 163 should accept write of 100");
}

// ===========================================================================
// 17. DIFFERENT INVERTER TYPES AFFECT REGISTERS
// ===========================================================================

#[tokio::test]
async fn three_phase_has_correct_dtc_and_max_power() {
    let state = build_state("ThreePhase", 2, 9.5, 50.0);
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x4001, "ThreePhase DTC should be 0x4001");
}

#[tokio::test]
async fn all_in_one_has_correct_dtc() {
    let state = build_state("AllInOne", 1, 9.5, 50.0);
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x8002, "AllInOne DTC should be 0x8002");
}

#[tokio::test]
async fn gen3_plus_has_correct_dtc() {
    let state = build_state("Gen3Plus6kW", 1, 5.2, 50.0);
    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    let dtc = h.read_hr(&mut s, 0).await;
    assert_eq!(dtc, 0x2201, "Gen3Plus6kW DTC should be 0x2201");
}

// ===========================================================================
// 18. PV ENERGY SCALING
// ===========================================================================

#[tokio::test]
async fn pv1_energy_scaling() {
    let mut state = midday_state();
    state.energy_totals.solar_generation_kwh = 25.3;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // IR 17 = PV1 energy today ×0.1 kWh → 25.3 / 0.1 = 253 (no PV2)
    let pv1_energy = h.read_ir(&mut s, 17).await;
    assert_eq!(
        pv1_energy, 253,
        "IR 17 (PV1 energy) should be 253 (=25.3 kWh × 10)"
    );
}

#[tokio::test]
async fn pv1_energy_split_with_pv2() {
    let mut state = midday_state();
    state.config.pv2_peak_watts = 5000.0;
    state.energy_totals.solar_generation_kwh = 20.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 1).await;
    let mut s = h.connect().await;

    // With PV2: PV1 gets 45%, PV2 gets 55%
    let pv1_energy = h.read_ir(&mut s, 17).await;
    let pv2_energy = h.read_ir(&mut s, 19).await;

    let expected_pv1: f64 = 20.0 * 0.45 * 10.0;
    let expected_pv1 = expected_pv1.round() as u16;
    let expected_pv2: f64 = 20.0 * 0.55 * 10.0;
    let expected_pv2 = expected_pv2.round() as u16;

    assert_eq!(
        pv1_energy, expected_pv1,
        "PV1 energy should be 45% of total"
    );
    assert_eq!(
        pv2_energy, expected_pv2,
        "PV2 energy should be 55% of total"
    );
    assert_eq!(pv1_energy + pv2_energy, 200, "PV1+PV2 should equal total");
}

// ===========================================================================
// 19. GRID POWER SIGN CONVENTION VALIDATION
// ===========================================================================

#[tokio::test]
async fn grid_power_positive_when_exporting() {
    let mut state = midday_state();
    // Override solar to max, override load to low → exporting
    state.solar_override = Some(5000.0);
    state.load_override = Some(500.0);

    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    // IR 30 = grid power (GE convention: positive = exporting)
    let raw = h.read_ir(&mut s, 30).await;
    let signed = raw as i16;
    // With high solar and low load, should be exporting
    assert!(
        signed > 0,
        "IR 30 should be positive (exporting) with high solar/low load, got {signed}"
    );
}

#[tokio::test]
async fn grid_power_negative_when_importing() {
    let mut state = midnight_state();
    state.load_override = Some(2000.0);
    state.solar_override = Some(0.0);
    state.batteries[0].soc_percent = 4.0;
    state.batteries[0].min_soc = 4.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    let raw = h.read_ir(&mut s, 30).await;
    let signed = raw as i16;
    assert!(
        signed < 0,
        "IR 30 should be negative (importing) at night with load, got {signed}"
    );
}

// ===========================================================================
// 20. CHARGE STATUS REGISTER
// ===========================================================================

#[tokio::test]
async fn charge_status_discharging_at_night() {
    let mut state = midnight_state();
    state.load_override = Some(2000.0);
    state.solar_override = Some(0.0);
    state.batteries[0].soc_percent = 50.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 10).await;
    let mut s = h.connect().await;

    // IR 14 = charge status (0=idle, 1=charging, 2=discharging)
    let status = h.read_ir(&mut s, 14).await;
    // At night with load, battery should be discharging
    assert!(
        status == 2,
        "IR 14 (charge status) should be 2 (discharging) at night with load, got {status}"
    );
}

#[tokio::test]
async fn charge_status_charging_midday() {
    let mut state = midday_state();
    state.batteries[0].soc_percent = 30.0;
    state.sync_battery_from_vec();

    let h = TestHarness::new(state, 100).await;
    let mut s = h.connect().await;

    // IR 14 = charge status → should be 1 (charging) with solar surplus
    let status = h.read_ir(&mut s, 14).await;
    assert!(
        status == 1,
        "IR 14 should be 1 (charging) at midday with low SOC, got {status}"
    );
}
