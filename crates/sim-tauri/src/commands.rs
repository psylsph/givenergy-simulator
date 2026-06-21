//! Tauri IPC commands.
//!
//! All #[tauri::command] functions must live in a separate module
//! to avoid a proc-macro namespace collision (E0255) in lib targets.

use crate::app_state::{AppState, PlantStateDto};
use sim_core::{
    BatteryEngine, Command, EnergyTracker, EvcEngine, InverterEngine, InverterMode, LoadEngine,
    LoadProfile, ScheduleEngine, SimulationEngine, SolarEngine, WeatherCondition,
};
use sim_models::{BatteryState, DeviceModel};
use sim_recording::RecordingFrame;
use tauri::{AppHandle, Emitter, Manager, State};

// ---------------------------------------------------------------------------
// Create Plant
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct BatteryModuleConfig {
    /// Capacity in kWh.
    pub capacity_kwh: f64,
    /// State of Health (0.0–1.0). Defaults to 1.0 (100%).
    #[serde(default = "default_soh")]
    pub soh: f64,
}

fn default_soh() -> f64 {
    1.0
}

/// Derive realistic throughput_kwh from SOH using the degradation model.
/// degradation_per_cycle = 0.0002 (0.02% SOH loss per cycle)
/// throughput_kwh = ((1.0 - SOH) / degradation_per_cycle) * nominal_capacity_kwh
fn throughput_from_soh(soh: f64, nominal_capacity_kwh: f64) -> f64 {
    const DEGRADATION_PER_CYCLE: f64 = 0.0002;
    let cycles = (1.0 - soh) / DEGRADATION_PER_CYCLE;
    cycles * nominal_capacity_kwh
}

#[derive(serde::Deserialize)]
pub struct CreatePlantParams {
    pub battery_count: Option<usize>,
    /// Per-module battery configurations (overrides battery_count).
    pub battery_modules: Option<Vec<BatteryModuleConfig>>,
    pub peak_watts: Option<f64>,
    pub pv2_peak_watts: Option<f64>,
    pub latitude: Option<f64>,
    pub load_profile: Option<String>,
    pub tick_interval: Option<u64>,
    pub inverter_type: Option<String>,
    pub ct_meter_installed: Option<bool>,
}

#[tauri::command]
pub async fn create_plant(
    app: AppHandle,
    state: State<'_, AppState>,
    params: CreatePlantParams,
) -> Result<PlantStateDto, String> {
    let peak_watts = params.peak_watts.unwrap_or(5000.0);
    let latitude = params.latitude.unwrap_or(51.5);
    let tick_interval = params.tick_interval.unwrap_or(1);

    let profile = match params.load_profile.as_deref().unwrap_or("family") {
        "minimal" => LoadProfile::Minimal,
        "ev" => LoadProfile::EV,
        "heatpump" => LoadProfile::HeatPump,
        _ => LoadProfile::Family,
    };

    let inv_type = params.inverter_type.as_deref().unwrap_or("Gen3Hybrid");

    // Max battery charge/discharge power per inverter type (watts)
    // Source: official GivEnergy datasheets
    let max_batt_w = match inv_type {
        // Gen 1 Hybrid 5.0: 2500W charge/discharge
        "Gen1Hybrid" => 2500.0,
        // Gen2 Hybrid 5.0: 3600W charge/discharge (same DC limit as Gen3)
        "Gen2Hybrid" => 3600.0,
        // Gen3 Hybrid 3.6/5.0: charge 3300W, discharge 3600W. Use 3600 as the DC battery limit.
        "Gen3Hybrid" => 3600.0,
        // Gen3 Hybrid 8.0: charge 8000W, discharge 8500W
        "Gen3Hybrid8kW" => 8000.0,
        // Gen3 Hybrid 10.0: charge 10000W, discharge 10500W
        "Gen3Hybrid10kW" => 10000.0,
        // Gen3 Plus variants
        "Gen3Plus6kW" => 2600.0,
        "Gen3Plus4600" => 2600.0,
        "Gen3Plus3600" => 2600.0,
        "Gen3Plus6kW2" => 2600.0,
        // AC Coupled / Mk2: 3000W charge/discharge
        "ACCoupled" | "ACCoupled2" => 3000.0,
        // All-in-One 6kW: 6000W continuous
        "AllInOne6" => 6000.0,
        // All-in-One (original 0x8002): 6kW continuous (7.2kW peak off-grid)
        "AllInOne" => 6000.0,
        // All-in-One 5kW variant
        "AllInOne5" => 5000.0,
        // AIO 8kW
        "AIO8kW" => 8000.0,
        // AIO 10kW
        "AIO10kW" => 10000.0,
        // AIO Hybrid variants
        "AIOHybrid6kW" => 6000.0,
        "AIOHybrid8kW" => 8000.0,
        "AIOHybrid10kW" => 10000.0,
        // 3-Phase 6kW: charge/discharge 6000W
        "ThreePhase" => 6000.0,
        "ThreePhase8kW" => 8000.0,
        "ThreePhase10kW" => 10000.0,
        "ThreePhase11kW" => 11000.0,
        // Gateway: aggregates an All-in-One (6kW continuous) behind it.
        "Gateway12kW" => 6000.0,
        _ => 3600.0,
    };
    let max_batt_kw = max_batt_w / 1000.0;

    let now = chrono::Local::now().naive_local();

    // Gateway: batteries live in the child AIO, not the gateway itself.
    // Default to a realistic 3 × GIV-BAT-3.4-HV stack (10.2 kWh) for the AIO.
    // Check Gateway FIRST so it always wins regardless of battery_modules param.
    let mut plant_state = if inv_type.starts_with("Gateway") {
        // Each module is 16S LFP @ 3.2V nominal = 51.2V.
        let hv_capacity: f64 = 3.4;
        let hv_voltage: f64 = 51.2;
        let count = 3usize;
        let per_module_max_kw = max_batt_kw / count as f64;
        let batts: Vec<BatteryState> = (0..count)
            .map(|_| {
                let c_rate_kw = (hv_capacity * 0.7).min(10.0);
                // Gateway HV stack: seed a 3-year-old pack so IR(6-7)
                // reads a realistic value on day one, in line with the
                // single-phase `PlantState::new` behaviour.
                let seeded_throughput = sim_core::seed_battery_throughput_for_age(
                    sim_core::BATTERY_DEFAULT_AGE_YEARS,
                    hv_capacity,
                );
                let seeded_soh =
                    sim_core::seed_battery_soh_for_age(sim_core::BATTERY_DEFAULT_AGE_YEARS);
                BatteryState {
                    capacity_kwh: hv_capacity,
                    nominal_capacity_kwh: hv_capacity,
                    voltage_v: hv_voltage,
                    soh: seeded_soh,
                    throughput_kwh: seeded_throughput,
                    max_charge_kw: c_rate_kw.min(per_module_max_kw),
                    max_discharge_kw: c_rate_kw.min(per_module_max_kw),
                    ..BatteryState::default()
                }
            })
            .collect();
        let mut state = sim_models::PlantState::new(now);
        state.batteries = batts;
        state.sync_battery_from_vec();
        state
    } else if let Some(modules) = params.battery_modules {
        let module_count = modules.len().clamp(1, 6);
        let per_module_max_kw = max_batt_kw / module_count as f64;
        let batts: Vec<BatteryState> =
            modules
                .into_iter()
                .take(6)
                .map(|m| {
                    let soh = m.soh.clamp(0.0, 1.0);
                    let capacity = m.capacity_kwh.max(1.0);
                    let effective_capacity = capacity * soh;
                    let c_rate_kw = (effective_capacity * 0.7).min(10.0);
                    // Derive throughput + SOH from a single consistent age.
                    // When the GUI sends the default `soh = 1.0` we treat it as
                    // "no user preference" and seed a 3-year-old pack so IR(6-7)
                    // reads a realistic value on day one. When the user has
                    // actively lowered SOH (e.g. via the slider), derive the
                    // equivalent age and seed from that so throughput, cycles,
                    // and SOH all agree.
                    let years = if soh < 1.0 {
                        // Reverse the degradation model: cycles = (1 - soh) /
                        // BATTERY_DEGRADATION_PER_CYCLE, years = cycles / CYCLES_PER_YEAR.
                        let cycles = (1.0 - soh) / sim_core::BATTERY_DEGRADATION_PER_CYCLE;
                        cycles / sim_core::BATTERY_CYCLES_PER_YEAR
                    } else {
                        sim_core::BATTERY_DEFAULT_AGE_YEARS
                    };
                    let seeded_throughput =
                        sim_core::seed_battery_throughput_for_age(years, capacity);
                    let seeded_soh = sim_core::seed_battery_soh_for_age(years);
                    tracing::info!(
                    "Creating battery module: SOH={} (from {}y), capacity={}, throughput={} kWh",
                    seeded_soh, years, capacity, seeded_throughput,
                );
                    BatteryState {
                        capacity_kwh: effective_capacity,
                        nominal_capacity_kwh: capacity,
                        soh: seeded_soh,
                        throughput_kwh: seeded_throughput,
                        max_charge_kw: c_rate_kw.min(per_module_max_kw),
                        max_discharge_kw: c_rate_kw.min(per_module_max_kw),
                        ..BatteryState::default()
                    }
                })
                .collect();
        let mut state = sim_models::PlantState::new(now);
        state.batteries = batts;
        state.sync_battery_from_vec();
        state
    } else {
        let battery_count = params.battery_count.unwrap_or(1).clamp(1, 6);
        sim_models::PlantState::with_battery_count(now, battery_count)
    };
    plant_state.config.solar_peak_watts = peak_watts;
    plant_state.config.latitude = latitude;
    plant_state.config.tick_interval_secs = tick_interval;
    plant_state.config.pv2_peak_watts = params.pv2_peak_watts.unwrap_or(0.0);
    plant_state.config.inverter_type = inv_type.to_string();
    plant_state.config.ct_meter_installed = params.ct_meter_installed.unwrap_or(true);
    // Gateway: the N battery modules form a single HV stack inside ONE AIO
    // (single-AIO topology — see docs/gateway-register-reference.md §11 and
    // AGENTS.md "Single-AIO topology: parallel_aio_num = 1"). The modules are
    // BMU cells of one stack, NOT separate AIOs, so parallel_aio_num is always
    // 1 and AIO2/AIO3 stay zero. (Advertising N AIOs made clients multiply the
    // per-AIO 6 kW battery limit by N, producing a phantom e.g. 18 kW limit
    // the sim could never reach.)
    if inv_type.starts_with("Gateway") {
        plant_state.config.parallel_aio_num = 1;
    }
    // Default DSP firmware per inverter type. Matches typical real-world values.
    plant_state.inverter.dsp_firmware_version = match inv_type {
        "Gen1Hybrid" => 110,
        "Gen2Hybrid" => 230,
        "Gen3Hybrid" => 449,
        "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 510,
        "ACCoupled" | "ACCoupled2" => 305,
        "ThreePhase" => 612,
        "ThreePhase8kW" | "ThreePhase10kW" => 612,
        "ThreePhase11kW" => 11043,
        "AllInOne6" | "AllInOne" | "AllInOne5" => 1010,
        "AIO8kW" | "AIO10kW" => 1010,
        "AIOHybrid6kW" | "AIOHybrid8kW" | "AIOHybrid10kW" => 1010,
        _ => 449,
    };
    plant_state.config.max_ac_watts = match plant_state.config.inverter_type.as_str() {
        "Gen3Hybrid8kW" => 8000.0,
        "Gen3Hybrid10kW" => 10000.0,
        "Gen3Plus6kW" => 5000.0,
        "Gen3Plus4600" => 4600.0,
        "Gen3Plus3600" => 3600.0,
        "Gen3Plus6kW2" => 6000.0,
        "AllInOne6" => 6000.0,
        "AIO8kW" => 8000.0,
        "AIO10kW" => 10000.0,
        "AIOHybrid6kW" => 6000.0,
        "AIOHybrid8kW" => 8000.0,
        "AIOHybrid10kW" => 10000.0,
        "ThreePhase" => 6000.0,
        "ThreePhase8kW" => 8000.0,
        "ThreePhase10kW" => 10000.0,
        "ThreePhase11kW" => 11000.0,
        "ACCoupled" | "ACCoupled2" => 3000.0,
        "AllInOne" => 6000.0,
        "AllInOne5" => 5000.0,
        "Gen1Hybrid" => 5000.0,
        "Gen2Hybrid" => 5000.0,
        "Gen3Hybrid" => 5000.0,
        // Gateway: aggregates an All-in-One (6kW AC) behind it.
        "Gateway12kW" => 6000.0,
        _ => 5000.0,
    };
    plant_state.inverter.export_limit_w = plant_state.config.max_ac_watts * 0.72;

    // Seed daily energy totals from 00:00 → now so the IR/HR "today" registers
    // (pv_energy_today, grid import/export today, etc.) read realistic values
    // immediately instead of climbing from zero over the first few minutes.
    // The `EnergyTracker` below is constructed with `with_last_reset_date(now.date())`
    // so the engine doesn't clobber the seed on its first tick.
    {
        let pv2_peak_w = plant_state.config.pv2_peak_watts;
        let weather_str = plant_state.weather.clone();
        let batteries = plant_state.batteries.clone();
        let max_ac_watts = plant_state.config.max_ac_watts;
        let charge_lim_pct = plant_state.battery_charge_limit_percent;
        let discharge_lim_pct = plant_state.battery_discharge_limit_percent;
        let seed_params = sim_core::EnergySeedParams {
            peak_w: peak_watts,
            pv2_peak_w,
            latitude,
            weather_str: &weather_str,
            batteries: &batteries,
            max_ac_watts,
            battery_charge_limit_percent: charge_lim_pct,
            battery_discharge_limit_percent: discharge_lim_pct,
        };
        plant_state.energy_totals = sim_core::seed_energy_totals_for_time_of_day(
            plant_state.timestamp,
            profile.clone(),
            &seed_params,
        );
    }

    // Reset schedule to default — a new plant shouldn't inherit old schedule settings
    {
        let mut sched = state.schedule.lock().await;
        *sched = Some(sim_core::Schedule::default());
    }
    // Ensure a default schedule exists
    {
        let mut sched = state.schedule.lock().await;
        if sched.is_none() {
            *sched = Some(sim_core::Schedule::default());
        }
    }
    let schedule_opt = state.schedule.lock().await.clone();
    // `last_reset_date = now.date()` so the engine doesn't zero the seeded
    // totals on its first tick (the first-tick record arm would clobber the
    // seed; the same-day no-op arm preserves it).
    let seed_date = plant_state.timestamp.date();
    let devices: Vec<Box<dyn DeviceModel>> = if let Some(ref sched) = schedule_opt {
        vec![
            Box::new(ScheduleEngine::new(sched.clone())),
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(profile)),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(sim_core::EnergyTracker::new().with_last_reset_date(seed_date)),
            Box::new(sim_core::EvcEngine::new()),
        ]
    } else {
        vec![
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(profile)),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(sim_core::EnergyTracker::new().with_last_reset_date(seed_date)),
            Box::new(sim_core::EvcEngine::new()),
        ]
    };

    let engine = SimulationEngine::new(plant_state, devices, tick_interval);

    // Populate register store immediately so Modbus clients see
    // non-zero values before the first tick.
    {
        let mut rs = state.register_store.lock().await;
        rs.project_from_state(&engine.state);
    }

    let plant_state = {
        let mut eng = state.engine.lock().await;
        *eng = Some(engine);
        let mut s = eng.as_ref().map(|e| e.state.clone()).unwrap();
        // Ensure scheduled_charge is false at startup — no tick has run yet.
        // This prevents stale persisted state from leaking into the first snapshot.
        s.scheduled_charge = false;
        s.scheduled_discharge = false;
        s
    };

    let dto = PlantStateDto::with_schedule(&plant_state, schedule_opt.as_ref());
    let _ = app.emit("state_changed", &dto);

    // Auto-save plant + schedule to disk
    let persisted = crate::app_state::PersistedState {
        plant: plant_state,
        schedule: schedule_opt,
    };
    let data_dir = app
        .path()
        .app_data_dir()
        .map_err(|e| format!("Cannot get app data dir: {e}"))?;
    std::fs::create_dir_all(&data_dir).map_err(|e| format!("Cannot create data dir: {e}"))?;
    let path = data_dir.join("plant_state.json");
    let json =
        serde_json::to_string_pretty(&persisted).map_err(|e| format!("Serialize error: {e}"))?;
    std::fs::write(&path, json).map_err(|e| format!("Write error: {e}"))?;
    tracing::info!("Auto-saved plant to {}", path.display());

    Ok(dto)
}

// ---------------------------------------------------------------------------
// Load Scenario
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct LoadScenarioParams {
    pub path: String,
}

#[derive(serde::Serialize)]
pub struct ScenarioEventInfo {
    pub time: String,
    pub has_solar: bool,
    pub has_load: bool,
    pub has_fault: bool,
    pub has_expect: bool,
    pub mode: Option<String>,
    pub weather: Option<String>,
}

#[derive(serde::Serialize)]
pub struct ScenarioInfo {
    pub name: String,
    pub days: u32,
    pub events: Vec<ScenarioEventInfo>,
}

#[tauri::command]
pub async fn load_scenario(
    _state: State<'_, AppState>,
    params: LoadScenarioParams,
) -> Result<ScenarioInfo, String> {
    let yaml = std::fs::read_to_string(&params.path).map_err(|e| e.to_string())?;
    let scenario = sim_scenarios::parse_named_scenario(&yaml).map_err(|e| e.to_string())?;

    let events: Vec<ScenarioEventInfo> = scenario
        .events
        .iter()
        .map(|(time, evt)| ScenarioEventInfo {
            time: time.to_string(),
            has_solar: evt.solar.is_some(),
            has_load: evt.load.is_some(),
            has_fault: evt.fault.is_some(),
            has_expect: evt.expect.is_some(),
            mode: evt.mode.clone(),
            weather: evt.weather.clone(),
        })
        .collect();

    Ok(ScenarioInfo {
        name: scenario.name,
        days: scenario.days,
        events,
    })
}

// ---------------------------------------------------------------------------
// Modbus command translation
// ---------------------------------------------------------------------------

/// Convert a Modbus register write into a simulation Command.
/// Handles both GE-native (HR 27, 59, 96, 110, etc.) and internal addresses.
fn modbus_address_to_command(address: u16, value: u16) -> Option<Command> {
    match address {
        // HR 20: enable charge target
        20 => Some(Command::SetEnableChargeTarget(value != 0)),
        // HR 27: Battery power mode (0=export, 1=eco)
        27 => {
            let mode = match value {
                1 => InverterMode::Eco,
                _ => InverterMode::Normal,
            };
            Some(Command::SetInverterMode(mode))
        }
        // HR 50: Active power rate (%) → export limit = rate% of max
        50 => Some(Command::SetActivePowerRate(value as f64)),
        // HR 96
        // HR 110: Battery SOC reserve (%)
        110 => Some(Command::SetMinSoc(value as f64)),
        // HR 111/112: Battery charge/discharge limits (%)
        111 => Some(Command::SetBatteryChargeLimit(value as f64)),
        112 => Some(Command::SetBatteryDischargeLimit(value as f64)),
        313 | 1110 => Some(Command::SetBatteryChargeLimit(value as f64)),
        314 | 1108 => Some(Command::SetBatteryDischargeLimit(value as f64)),
        29 => {
            if value == 0 {
                Some(Command::CancelCalibration)
            } else {
                Some(Command::StartCalibration { module: None })
            }
        }
        166 => Some(Command::SetEnableRtc(value != 0)),
        163 => {
            if value == 100 {
                Some(Command::InverterReboot)
            } else {
                None
            }
        }
        199 => Some(Command::SetEnableInverterParallelMode(value != 0)),
        311 => Some(Command::SetExportPriority(value)),
        317 => Some(Command::SetEnableEps(value != 0)),
        2040 => Some(Command::SetEmsEnable(value != 0)),
        318 => Some(Command::SetBatteryPause {
            mode: value,
            start: 60,
            end: 60,
        }),
        // HR 1122: Three-phase force discharge enable
        1122 => Some(Command::SetInverterMode(if value != 0 {
            InverterMode::ForceDischarge
        } else {
            InverterMode::Eco
        })),
        // HR 1123: Three-phase force charge enable
        1123 => Some(Command::SetInverterMode(if value != 0 {
            InverterMode::ForceCharge
        } else {
            InverterMode::Eco
        })),
        // HR 100: Inverter mode (internal)
        100 => {
            let mode = match value {
                1 => InverterMode::Eco,
                2 => InverterMode::ForceCharge,
                3 => InverterMode::ForceDischarge,
                4 => InverterMode::ExportLimit,
                _ => InverterMode::Normal,
            };
            Some(Command::SetInverterMode(mode))
        }
        // HR 210: Battery min SOC (internal)
        210 => Some(Command::SetMinSoc(value as f64)),
        // HR 211: Battery max SOC (internal)
        211 => Some(Command::SetMaxSoc(value as f64)),
        // All other registers (including schedule HR 31-32, 44-45, 56-57, 59, 94-95, 96, 116)
        // are handled by the drain loop's schedule accumulator.
        _ => None,
    }
}

/// Convert HHMM register value (e.g. 530 = 05:30) to decimal hours.
/// Returns None if the value is the disabled sentinel (60) or invalid.
fn hhmm_to_hours(val: u16) -> Option<f64> {
    if val == 60 {
        return None; // disabled
    }
    let hours = val / 100;
    let mins = val % 100;
    if mins > 59 || hours > 23 {
        return None; // invalid
    }
    Some(hours as f64 + mins as f64 / 60.0)
}

/// Check if a register address is a schedule-related holding register.
fn is_schedule_register(addr: u16) -> bool {
    matches!(
        addr,
        31..=32 | 44..=45 | 56..=57 | 59 | 94..=96 | 116
            | 242..=245 | 272 | 275
            | 246..=269 | 276..=299
            | 1109 | 1111..=1116 | 1118..=1121
            | 2062..=2071
            | 2044..=2061
    )
}

// ---------------------------------------------------------------------------
// Start / Pause Simulation
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct StartSimulationParams {
    pub speed: Option<f64>,
    pub scenario_path: Option<String>,
}

#[tauri::command]
pub async fn start_simulation(
    app: AppHandle,
    state: State<'_, AppState>,
    params: StartSimulationParams,
) -> Result<(), String> {
    if state.engine.lock().await.is_none() {
        return Err("No plant exists — create or load a plant first".into());
    }

    {
        let mut running = state.running.lock().await;
        if *running {
            return Err("Simulation already running".into());
        }
        *running = true;
    }

    let scenario = if let Some(path) = &params.scenario_path {
        let yaml = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
        Some(sim_scenarios::parse_named_scenario(&yaml).map_err(|e| e.to_string())?)
    } else {
        None
    };

    let speed = params.speed.unwrap_or(10.0);
    let tick_delay = std::time::Duration::from_millis((1000.0 / speed) as u64);

    // At real-time speed (speed <= 1.0) lock the sim clock to the host wall
    // clock so the served/displayed inverter time matches the computer's time
    // (no drift). At higher speeds the user wants fast-forward, so keep the
    // original fixed-step advancement.
    if speed <= 1.0 {
        if let Some(e) = state.engine.lock().await.as_mut() {
            e.anchor_to_wall_clock(None);
        }
    }

    let engine = state.engine.clone();
    let register_store = state.register_store.clone();
    let recording = state.recording.clone();
    let running_flag = state.running.clone();
    let app_handle = app.clone();

    let modbus_cmds = state.modbus_cmds.clone();
    let battery_snapshot = state.battery_snapshot.clone();
    let pending_time_regs = state.pending_time_regs.clone();
    let evc_arc = state.evc_state.clone();
    let schedule_arc = state.schedule.clone();
    let save_dir = app.path().app_data_dir().ok();

    tauri::async_runtime::spawn(async move {
        if let Some(scen) = scenario {
            let events = scen.events;
            let mut event_idx = 0;

            loop {
                if !*running_flag.lock().await {
                    break;
                }

                let tick_result = {
                    let mut eng = engine.lock().await;
                    if let Some(ref mut e) = *eng {
                        if let Some((time, _event)) = events.get(event_idx) {
                            let current_date = e.state.timestamp.date();
                            let target = current_date.and_time(*time);
                            if e.state.timestamp >= target {
                                let (_, event) = &events[event_idx];
                                if let Some(solar_w) = event.solar {
                                    e.state.solar.generation_w = solar_w;
                                }
                                if let Some(load_w) = event.load {
                                    e.state.load.demand_w = load_w;
                                }
                                if let Some(fault) = &event.fault {
                                    e.enqueue(Command::InjectFault(fault.clone()));
                                }
                                if let Some(fault) = &event.clear_fault {
                                    e.enqueue(Command::ClearFault(fault.clone()));
                                }
                                if let Some(mode_str) = &event.mode {
                                    let mode = match mode_str.as_str() {
                                        "Normal" => InverterMode::Normal,
                                        "Eco" => InverterMode::Eco,
                                        "ForceCharge" => InverterMode::ForceCharge,
                                        "ForceDischarge" => InverterMode::ForceDischarge,
                                        "ExportLimit" => InverterMode::ExportLimit,
                                        _ => InverterMode::Normal,
                                    };
                                    e.enqueue(Command::SetInverterMode(mode));
                                }
                                if let Some(limit) = event.export_limit {
                                    e.enqueue(Command::SetExportLimit(limit));
                                }
                                if let Some(weather_str) = &event.weather {
                                    let w = match weather_str.to_lowercase().as_str() {
                                        "partlycloudy" | "partly-cloudy" | "partly_cloudy" => {
                                            WeatherCondition::PartlyCloudy
                                        }
                                        "overcast" => WeatherCondition::Overcast,
                                        "storm" => WeatherCondition::Storm,
                                        _ => WeatherCondition::Clear,
                                    };
                                    e.enqueue(Command::SetWeather(w));
                                }
                                event_idx += 1;
                                if let Some(expect) = &event.expect {
                                    match sim_scenarios::check_assertions(expect, &e.state) {
                                        Ok(()) => tracing::info!("[{time}] ✓ assertions passed"),
                                        Err(failures) => tracing::error!("[{time}] ✗ {failures:?}"),
                                    }
                                }
                                if event_idx >= events.len() {
                                    let _ = app_handle.emit("scenario_completed", ());
                                    break;
                                }
                            }
                        }

                        // Drain and apply Modbus write commands before tick
                        // Phase 1: collect under sync MutexGuards (no .await allowed)
                        let mut sched_dirty = false;
                        let mut sched_updates: std::collections::HashMap<u16, u16> =
                            std::collections::HashMap::new();
                        {
                            if let Ok(mut cmds) = modbus_cmds.lock() {
                                if let Ok(mut time_buf) = pending_time_regs.lock() {
                                    for cmd in cmds.drain(..) {
                                        match cmd.address {
                                            35 => time_buf[0] = Some(cmd.value),
                                            36 => time_buf[1] = Some(cmd.value),
                                            37 => time_buf[2] = Some(cmd.value),
                                            38 => time_buf[3] = Some(cmd.value),
                                            39 => time_buf[4] = Some(cmd.value),
                                            40 => time_buf[5] = Some(cmd.value),
                                            _ => {}
                                        }
                                        if is_schedule_register(cmd.address) {
                                            sched_updates.insert(cmd.address, cmd.value);
                                            sched_dirty = true;
                                        }
                                        // Also collect pause slot registers
                                        if matches!(cmd.address, 318..=320) {
                                            sched_updates.insert(cmd.address, cmd.value);
                                        }
                                        if let Some(sim_cmd) =
                                            modbus_address_to_command(cmd.address, cmd.value)
                                        {
                                            e.enqueue(sim_cmd);
                                        }
                                    }
                                    // Apply time registers
                                    if time_buf.iter().all(|r| r.is_some()) {
                                        let y = time_buf[0].unwrap() as i32;
                                        let m = time_buf[1].unwrap() as u32;
                                        let d = time_buf[2].unwrap() as u32;
                                        let h = time_buf[3].unwrap() as u32;
                                        let min = time_buf[4].unwrap() as u32;
                                        let s = time_buf[5].unwrap() as u32;
                                        if let Some(dt) = chrono::NaiveDate::from_ymd_opt(y, m, d)
                                            .and_then(|date| date.and_hms_opt(h, min, s))
                                        {
                                            e.enqueue(Command::SetSimulationTime(dt));
                                        }
                                        *time_buf = [None; 6];
                                    }
                                }
                            }
                        }
                        // Handle pause slot updates (HR 318-320) after MutexGuards dropped
                        if let Some(&mode) = sched_updates.get(&318) {
                            e.enqueue(Command::SetBatteryPause {
                                mode,
                                start: sched_updates
                                    .get(&319)
                                    .copied()
                                    .unwrap_or(e.state.battery_pause_slot_start),
                                end: sched_updates
                                    .get(&320)
                                    .copied()
                                    .unwrap_or(e.state.battery_pause_slot_end),
                            });
                        } else if sched_updates.contains_key(&319)
                            || sched_updates.contains_key(&320)
                        {
                            e.enqueue(Command::SetBatteryPause {
                                mode: e.state.battery_pause_mode,
                                start: sched_updates
                                    .get(&319)
                                    .copied()
                                    .unwrap_or(e.state.battery_pause_slot_start),
                                end: sched_updates
                                    .get(&320)
                                    .copied()
                                    .unwrap_or(e.state.battery_pause_slot_end),
                            });
                        }
                        // Phase 2: apply schedule updates (MutexGuards dropped, safe to .await)
                        if sched_dirty {
                            let mut sched = schedule_arc.lock().await.clone().unwrap_or_default();
                            // Charge slot 1 (HR 94-95) — primary
                            if let Some(&v) = sched_updates.get(&94) {
                                sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&95) {
                                sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Charge slot 2 (HR 31-32, GivTCP Gen3 aliases HR 243-244)
                            if let Some(&v) =
                                sched_updates.get(&31).or_else(|| sched_updates.get(&243))
                            {
                                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) =
                                sched_updates.get(&32).or_else(|| sched_updates.get(&244))
                            {
                                sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Discharge slot 1 (HR 56-57) — primary
                            if let Some(&v) = sched_updates.get(&56) {
                                sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&57) {
                                sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Discharge slot 2 (HR 44-45)
                            if let Some(&v) = sched_updates.get(&44) {
                                sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&45) {
                                sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Charge target SOC (HR 116)
                            if let Some(&v) = sched_updates.get(&116) {
                                sched.charge_target_soc = v as f64;
                            }
                            // Charge target SOC slot 1 per-slot (HR 242)
                            if let Some(&v) = sched_updates.get(&242) {
                                sched.charge_target_soc = v as f64;
                            }
                            // Charge target SOC slot 2 per-slot (HR 245)
                            if let Some(&v) = sched_updates.get(&245) {
                                sched.charge_target_soc_2 = v as f64;
                            }
                            // Discharge target SOC slot 1 per-slot (HR 272)
                            if let Some(&v) = sched_updates.get(&272) {
                                sched.discharge_target_soc = v as f64;
                            }
                            // Discharge target SOC slot 2 per-slot (HR 275)
                            if let Some(&v) = sched_updates.get(&275) {
                                sched.discharge_target_soc_2 = v as f64;
                            }
                            // Charge slot 3-10 (HR 246-268, alternating start/end)
                            if let Some(&v) = sched_updates.get(&246) {
                                sched.charge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&247) {
                                sched.charge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&248) {
                                sched.charge_target_soc_3 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&249) {
                                sched.charge_start_4 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&250) {
                                sched.charge_end_4 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&251) {
                                sched.charge_target_soc_4 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&252) {
                                sched.charge_start_5 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&253) {
                                sched.charge_end_5 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&254) {
                                sched.charge_target_soc_5 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&255) {
                                sched.charge_start_6 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&256) {
                                sched.charge_end_6 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&257) {
                                sched.charge_target_soc_6 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&258) {
                                sched.charge_start_7 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&259) {
                                sched.charge_end_7 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&260) {
                                sched.charge_target_soc_7 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&261) {
                                sched.charge_start_8 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&262) {
                                sched.charge_end_8 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&263) {
                                sched.charge_target_soc_8 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&264) {
                                sched.charge_start_9 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&265) {
                                sched.charge_end_9 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&266) {
                                sched.charge_target_soc_9 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&267) {
                                sched.charge_start_10 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&268) {
                                sched.charge_end_10 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&269) {
                                sched.charge_target_soc_10 = v as f64;
                            }
                            // Discharge slot 3-10 (HR 276-298, alternating start/end)
                            if let Some(&v) = sched_updates.get(&276) {
                                sched.discharge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&277) {
                                sched.discharge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&278) {
                                sched.discharge_target_soc_3 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&279) {
                                sched.discharge_start_4 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&280) {
                                sched.discharge_end_4 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&281) {
                                sched.discharge_target_soc_4 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&282) {
                                sched.discharge_start_5 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&283) {
                                sched.discharge_end_5 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&284) {
                                sched.discharge_target_soc_5 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&285) {
                                sched.discharge_start_6 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&286) {
                                sched.discharge_end_6 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&287) {
                                sched.discharge_target_soc_6 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&288) {
                                sched.discharge_start_7 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&289) {
                                sched.discharge_end_7 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&290) {
                                sched.discharge_target_soc_7 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&291) {
                                sched.discharge_start_8 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&292) {
                                sched.discharge_end_8 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&293) {
                                sched.discharge_target_soc_8 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&294) {
                                sched.discharge_start_9 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&295) {
                                sched.discharge_end_9 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&296) {
                                sched.discharge_target_soc_9 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&297) {
                                sched.discharge_start_10 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&298) {
                                sched.discharge_end_10 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&299) {
                                sched.discharge_target_soc_10 = v as f64;
                            }
                            // EMS discharge slots 1-3 (HR 2044-2052)
                            if let Some(&v) = sched_updates.get(&2044) {
                                sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2045) {
                                sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2046) {
                                sched.discharge_target_soc = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&2047) {
                                sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2048) {
                                sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2049) {
                                sched.discharge_target_soc_2 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&2050) {
                                sched.discharge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2051) {
                                sched.discharge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2052) {
                                sched.discharge_target_soc_3 = v as f64;
                            }
                            // EMS charge slots 1-3 (HR 2053-2061)
                            if let Some(&v) = sched_updates.get(&2053) {
                                sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2054) {
                                sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2055) {
                                sched.charge_target_soc = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&2056) {
                                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2057) {
                                sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2058) {
                                sched.charge_target_soc_2 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&2059) {
                                sched.charge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2060) {
                                sched.charge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2061) {
                                sched.charge_target_soc_3 = v as f64;
                            }

                            // Enable charge (HR 96) — 0 = disable slot 1, 1 = always-on
                            if let Some(&v) = sched_updates.get(&96) {
                                if v == 0 {
                                    sched.charge_start = 0.0;
                                    sched.charge_end = 0.0;
                                    sched.enable_charge = false;
                                } else {
                                    sched.enable_charge = true;
                                }
                            }
                            // Enable discharge (HR 59) — 0 = disable, 1 = always-on
                            if let Some(&v) = sched_updates.get(&59) {
                                if v == 0 {
                                    sched.discharge_start = 0.0;
                                    sched.discharge_end = 0.0;
                                    sched.enable_discharge = false;
                                } else {
                                    sched.enable_discharge = true;
                                }
                            }
                            // TPH charge target SOC (HR 1111) — same as HR 116
                            if let Some(&v) = sched_updates.get(&1111) {
                                sched.charge_target_soc = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&1112) {
                                sched.enable_charge = v != 0;
                            }
                            if let Some(&v) = sched_updates.get(&1113) {
                                sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1114) {
                                sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1115) {
                                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1116) {
                                sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1118) {
                                sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1119) {
                                sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1120) {
                                sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1121) {
                                sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Charge target SOC (HR 116)
                            *schedule_arc.lock().await = Some(sched.clone());
                            e.enqueue(Command::SetSchedule(Box::new(sched)));
                        }

                        // Sync EVC state from Modbus writes before tick
                        {
                            let evc_guard = evc_arc.lock().await;
                            e.state.evc.charge_control = evc_guard.charge_control;
                            e.state.evc.charge_current_limit = evc_guard.charge_current_limit;
                            e.state.evc.plug_and_go = evc_guard.plug_and_go;
                            e.state.evc.enabled = evc_guard.enabled;
                            e.state.evc.connection_status = evc_guard.connection_status;
                        }
                        e.tick();
                        {
                            let mut rs = register_store.lock().await;
                            rs.project_from_state(&e.state);
                            let sched_ref = schedule_arc.lock().await.clone();
                            if let Some(ref s) = sched_ref {
                                rs.project_schedule_for(s, &e.state.config.inverter_type);
                            }
                        }
                        // Update battery snapshot for Modbus BMS reads
                        {
                            let mut bs = battery_snapshot.lock().await;
                            *bs = e.state.batteries.clone();
                        }
                        // Sync EVC state for Modbus reads/writes
                        {
                            let mut evc = evc_arc.lock().await;
                            *evc = e.state.evc.clone();
                        }
                        let frame = RecordingFrame {
                            timestamp: e.state.timestamp,
                            plant_state: e.state.clone(),
                            register_snapshot: register_store.lock().await.snapshot(),
                        };
                        recording.lock().await.push(frame);
                        let sched_ref = schedule_arc.lock().await.clone();
                        Some(PlantStateDto::with_schedule(&e.state, sched_ref.as_ref()))
                    } else {
                        None
                    }
                };

                if let Some(dto) = &tick_result {
                    let _ = app_handle.emit("state_changed", dto);
                }
                // Auto-save every tick
                {
                    let plant = engine.lock().await.as_ref().map(|e| e.state.clone());
                    if let (Some(plant), Some(ref dir)) = (plant, &save_dir) {
                        let sched = schedule_arc.lock().await.clone();
                        let persisted = crate::app_state::PersistedState {
                            plant,
                            schedule: sched,
                        };
                        let path = dir.join("plant_state.json");
                        if let Ok(json) = serde_json::to_string_pretty(&persisted) {
                            let _ = std::fs::write(&path, json);
                        }
                    }
                }
                tokio::time::sleep(tick_delay).await;
            }
        } else {
            loop {
                if !*running_flag.lock().await {
                    break;
                }
                let tick_result = {
                    let mut eng = engine.lock().await;
                    if let Some(ref mut e) = *eng {
                        // Drain and apply Modbus write commands before tick
                        // Phase 1: collect under sync MutexGuards (no .await allowed)
                        let mut sched_dirty = false;
                        let mut sched_updates: std::collections::HashMap<u16, u16> =
                            std::collections::HashMap::new();
                        {
                            if let Ok(mut cmds) = modbus_cmds.lock() {
                                if let Ok(mut time_buf) = pending_time_regs.lock() {
                                    for cmd in cmds.drain(..) {
                                        match cmd.address {
                                            35 => time_buf[0] = Some(cmd.value),
                                            36 => time_buf[1] = Some(cmd.value),
                                            37 => time_buf[2] = Some(cmd.value),
                                            38 => time_buf[3] = Some(cmd.value),
                                            39 => time_buf[4] = Some(cmd.value),
                                            40 => time_buf[5] = Some(cmd.value),
                                            _ => {}
                                        }
                                        if is_schedule_register(cmd.address) {
                                            sched_updates.insert(cmd.address, cmd.value);
                                            sched_dirty = true;
                                        }
                                        // Also collect pause slot registers
                                        if matches!(cmd.address, 318..=320) {
                                            sched_updates.insert(cmd.address, cmd.value);
                                        }
                                        if let Some(sim_cmd) =
                                            modbus_address_to_command(cmd.address, cmd.value)
                                        {
                                            e.enqueue(sim_cmd);
                                        }
                                    }
                                    // Apply time registers
                                    if time_buf.iter().all(|r| r.is_some()) {
                                        let y = time_buf[0].unwrap() as i32;
                                        let m = time_buf[1].unwrap() as u32;
                                        let d = time_buf[2].unwrap() as u32;
                                        let h = time_buf[3].unwrap() as u32;
                                        let min = time_buf[4].unwrap() as u32;
                                        let s = time_buf[5].unwrap() as u32;
                                        if let Some(dt) = chrono::NaiveDate::from_ymd_opt(y, m, d)
                                            .and_then(|date| date.and_hms_opt(h, min, s))
                                        {
                                            e.enqueue(Command::SetSimulationTime(dt));
                                        }
                                        *time_buf = [None; 6];
                                    }
                                }
                            }
                        }
                        // Handle pause slot updates (HR 318-320) after MutexGuards dropped
                        if let Some(&mode) = sched_updates.get(&318) {
                            e.enqueue(Command::SetBatteryPause {
                                mode,
                                start: sched_updates
                                    .get(&319)
                                    .copied()
                                    .unwrap_or(e.state.battery_pause_slot_start),
                                end: sched_updates
                                    .get(&320)
                                    .copied()
                                    .unwrap_or(e.state.battery_pause_slot_end),
                            });
                        } else if sched_updates.contains_key(&319)
                            || sched_updates.contains_key(&320)
                        {
                            e.enqueue(Command::SetBatteryPause {
                                mode: e.state.battery_pause_mode,
                                start: sched_updates
                                    .get(&319)
                                    .copied()
                                    .unwrap_or(e.state.battery_pause_slot_start),
                                end: sched_updates
                                    .get(&320)
                                    .copied()
                                    .unwrap_or(e.state.battery_pause_slot_end),
                            });
                        }
                        // Phase 2: apply schedule updates (MutexGuards dropped, safe to .await)
                        if sched_dirty {
                            let mut sched = schedule_arc.lock().await.clone().unwrap_or_default();
                            // Charge slot 1 (HR 94-95) — primary
                            if let Some(&v) = sched_updates.get(&94) {
                                sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&95) {
                                sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Charge slot 2 (HR 31-32, GivTCP Gen3 aliases HR 243-244)
                            if let Some(&v) =
                                sched_updates.get(&31).or_else(|| sched_updates.get(&243))
                            {
                                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) =
                                sched_updates.get(&32).or_else(|| sched_updates.get(&244))
                            {
                                sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Discharge slot 1 (HR 56-57) — primary
                            if let Some(&v) = sched_updates.get(&56) {
                                sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&57) {
                                sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Discharge slot 2 (HR 44-45)
                            if let Some(&v) = sched_updates.get(&44) {
                                sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&45) {
                                sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Charge target SOC (HR 116)
                            if let Some(&v) = sched_updates.get(&116) {
                                sched.charge_target_soc = v as f64;
                            }
                            // Charge target SOC slot 1 per-slot (HR 242)
                            if let Some(&v) = sched_updates.get(&242) {
                                sched.charge_target_soc = v as f64;
                            }
                            // Charge target SOC slot 2 per-slot (HR 245)
                            if let Some(&v) = sched_updates.get(&245) {
                                sched.charge_target_soc_2 = v as f64;
                            }
                            // Discharge target SOC slot 1 per-slot (HR 272)
                            if let Some(&v) = sched_updates.get(&272) {
                                sched.discharge_target_soc = v as f64;
                            }
                            // Discharge target SOC slot 2 per-slot (HR 275)
                            if let Some(&v) = sched_updates.get(&275) {
                                sched.discharge_target_soc_2 = v as f64;
                            }
                            // Charge slot 3-10 (HR 246-268, alternating start/end)
                            if let Some(&v) = sched_updates.get(&246) {
                                sched.charge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&247) {
                                sched.charge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&248) {
                                sched.charge_target_soc_3 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&249) {
                                sched.charge_start_4 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&250) {
                                sched.charge_end_4 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&251) {
                                sched.charge_target_soc_4 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&252) {
                                sched.charge_start_5 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&253) {
                                sched.charge_end_5 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&254) {
                                sched.charge_target_soc_5 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&255) {
                                sched.charge_start_6 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&256) {
                                sched.charge_end_6 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&257) {
                                sched.charge_target_soc_6 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&258) {
                                sched.charge_start_7 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&259) {
                                sched.charge_end_7 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&260) {
                                sched.charge_target_soc_7 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&261) {
                                sched.charge_start_8 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&262) {
                                sched.charge_end_8 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&263) {
                                sched.charge_target_soc_8 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&264) {
                                sched.charge_start_9 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&265) {
                                sched.charge_end_9 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&266) {
                                sched.charge_target_soc_9 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&267) {
                                sched.charge_start_10 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&268) {
                                sched.charge_end_10 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&269) {
                                sched.charge_target_soc_10 = v as f64;
                            }
                            // Discharge slot 3-10 (HR 276-298, alternating start/end)
                            if let Some(&v) = sched_updates.get(&276) {
                                sched.discharge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&277) {
                                sched.discharge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&278) {
                                sched.discharge_target_soc_3 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&279) {
                                sched.discharge_start_4 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&280) {
                                sched.discharge_end_4 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&281) {
                                sched.discharge_target_soc_4 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&282) {
                                sched.discharge_start_5 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&283) {
                                sched.discharge_end_5 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&284) {
                                sched.discharge_target_soc_5 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&285) {
                                sched.discharge_start_6 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&286) {
                                sched.discharge_end_6 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&287) {
                                sched.discharge_target_soc_6 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&288) {
                                sched.discharge_start_7 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&289) {
                                sched.discharge_end_7 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&290) {
                                sched.discharge_target_soc_7 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&291) {
                                sched.discharge_start_8 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&292) {
                                sched.discharge_end_8 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&293) {
                                sched.discharge_target_soc_8 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&294) {
                                sched.discharge_start_9 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&295) {
                                sched.discharge_end_9 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&296) {
                                sched.discharge_target_soc_9 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&297) {
                                sched.discharge_start_10 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&298) {
                                sched.discharge_end_10 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&299) {
                                sched.discharge_target_soc_10 = v as f64;
                            }
                            // EMS discharge slots 1-3 (HR 2044-2052)
                            if let Some(&v) = sched_updates.get(&2044) {
                                sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2045) {
                                sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2046) {
                                sched.discharge_target_soc = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&2047) {
                                sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2048) {
                                sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2049) {
                                sched.discharge_target_soc_2 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&2050) {
                                sched.discharge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2051) {
                                sched.discharge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2052) {
                                sched.discharge_target_soc_3 = v as f64;
                            }
                            // EMS charge slots 1-3 (HR 2053-2061)
                            if let Some(&v) = sched_updates.get(&2053) {
                                sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2054) {
                                sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2055) {
                                sched.charge_target_soc = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&2056) {
                                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2057) {
                                sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2058) {
                                sched.charge_target_soc_2 = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&2059) {
                                sched.charge_start_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2060) {
                                sched.charge_end_3 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&2061) {
                                sched.charge_target_soc_3 = v as f64;
                            }

                            // Enable charge (HR 96) — 0 = disable slot 1, 1 = always-on
                            if let Some(&v) = sched_updates.get(&96) {
                                if v == 0 {
                                    sched.charge_start = 0.0;
                                    sched.charge_end = 0.0;
                                    sched.enable_charge = false;
                                } else {
                                    sched.enable_charge = true;
                                }
                            }
                            // Enable discharge (HR 59) — 0 = disable, 1 = always-on
                            if let Some(&v) = sched_updates.get(&59) {
                                if v == 0 {
                                    sched.discharge_start = 0.0;
                                    sched.discharge_end = 0.0;
                                    sched.enable_discharge = false;
                                } else {
                                    sched.enable_discharge = true;
                                }
                            }
                            // TPH charge target SOC (HR 1111) — same as HR 116
                            if let Some(&v) = sched_updates.get(&1111) {
                                sched.charge_target_soc = v as f64;
                            }
                            if let Some(&v) = sched_updates.get(&1112) {
                                sched.enable_charge = v != 0;
                            }
                            if let Some(&v) = sched_updates.get(&1113) {
                                sched.charge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1114) {
                                sched.charge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1115) {
                                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1116) {
                                sched.charge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1118) {
                                sched.discharge_start = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1119) {
                                sched.discharge_end = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1120) {
                                sched.discharge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&1121) {
                                sched.discharge_end_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            // Charge target SOC (HR 116)
                            *schedule_arc.lock().await = Some(sched.clone());
                            e.enqueue(Command::SetSchedule(Box::new(sched)));
                        }

                        // Sync EVC state from Modbus writes before tick
                        {
                            let evc_guard = evc_arc.lock().await;
                            e.state.evc.charge_control = evc_guard.charge_control;
                            e.state.evc.charge_current_limit = evc_guard.charge_current_limit;
                            e.state.evc.plug_and_go = evc_guard.plug_and_go;
                            e.state.evc.enabled = evc_guard.enabled;
                            e.state.evc.connection_status = evc_guard.connection_status;
                        }
                        e.tick();
                        {
                            let mut rs = register_store.lock().await;
                            rs.project_from_state(&e.state);
                            let sched_ref = schedule_arc.lock().await.clone();
                            if let Some(ref s) = sched_ref {
                                rs.project_schedule_for(s, &e.state.config.inverter_type);
                            }
                        }
                        // Update battery snapshot for Modbus BMS reads
                        {
                            let mut bs = battery_snapshot.lock().await;
                            *bs = e.state.batteries.clone();
                        }
                        // Sync EVC state for Modbus reads/writes
                        {
                            let mut evc = evc_arc.lock().await;
                            *evc = e.state.evc.clone();
                        }
                        let frame = RecordingFrame {
                            timestamp: e.state.timestamp,
                            plant_state: e.state.clone(),
                            register_snapshot: register_store.lock().await.snapshot(),
                        };
                        recording.lock().await.push(frame);
                        let sched_ref = schedule_arc.lock().await.clone();
                        Some(PlantStateDto::with_schedule(&e.state, sched_ref.as_ref()))
                    } else {
                        None
                    }
                };
                if let Some(dto) = &tick_result {
                    let _ = app_handle.emit("state_changed", dto);
                }
                // Auto-save every tick
                {
                    let plant = engine.lock().await.as_ref().map(|e| e.state.clone());
                    if let (Some(plant), Some(ref dir)) = (plant, &save_dir) {
                        let sched = schedule_arc.lock().await.clone();
                        let persisted = crate::app_state::PersistedState {
                            plant,
                            schedule: sched,
                        };
                        let path = dir.join("plant_state.json");
                        if let Ok(json) = serde_json::to_string_pretty(&persisted) {
                            let _ = std::fs::write(&path, json);
                        }
                    }
                }
                tokio::time::sleep(tick_delay).await;
            }
        }

        *running_flag.lock().await = false;
    });

    Ok(())
}

#[tauri::command]
pub async fn pause_simulation(state: State<'_, AppState>) -> Result<(), String> {
    *state.running.lock().await = false;
    Ok(())
}

// ---------------------------------------------------------------------------
// Faults
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct InjectFaultParams {
    pub fault_id: String,
}

#[tauri::command]
pub async fn inject_fault(
    app: AppHandle,
    state: State<'_, AppState>,
    params: InjectFaultParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::InjectFault(params.fault_id.clone()));
        let _ = app.emit("fault_triggered", &params.fault_id);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct ClearFaultParams {
    pub fault_id: String,
}

#[tauri::command]
pub async fn clear_fault(
    state: State<'_, AppState>,
    params: ClearFaultParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::ClearFault(params.fault_id));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Mode / Weather
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct SetModeParams {
    pub mode: String,
}

#[tauri::command]
pub async fn set_mode(state: State<'_, AppState>, params: SetModeParams) -> Result<(), String> {
    let mode = match params.mode.as_str() {
        "Normal" => InverterMode::Normal,
        "Eco" => InverterMode::Eco,
        "ForceCharge" => InverterMode::ForceCharge,
        "ForceDischarge" => InverterMode::ForceDischarge,
        "ExportLimit" => InverterMode::ExportLimit,
        _ => return Err(format!("Unknown mode: {}", params.mode)),
    };
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::SetInverterMode(mode));
    }
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct SetWeatherParams {
    pub weather: String,
}

#[tauri::command]
pub async fn set_weather(
    state: State<'_, AppState>,
    params: SetWeatherParams,
) -> Result<(), String> {
    let w = match params.weather.as_str() {
        "Clear" => WeatherCondition::Clear,
        "PartlyCloudy" => WeatherCondition::PartlyCloudy,
        "Overcast" => WeatherCondition::Overcast,
        "Storm" => WeatherCondition::Storm,
        _ => return Err(format!("Unknown weather: {}", params.weather)),
    };
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::SetWeather(w));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Manual Overrides
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct SetOverrideParams {
    pub watts: Option<f64>,
}

#[tauri::command]
pub async fn set_solar_override(
    app: AppHandle,
    state: State<'_, AppState>,
    params: SetOverrideParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.state.solar_override = params.watts;
        let dto = PlantStateDto::from(&e.state);
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

#[tauri::command]
pub async fn set_load_override(
    app: AppHandle,
    state: State<'_, AppState>,
    params: SetOverrideParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.state.load_override = params.watts;
        let dto = PlantStateDto::from(&e.state);
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct SetCtMeterParams {
    pub enabled: bool,
}

#[tauri::command]
pub async fn set_ct_meter(
    app: AppHandle,
    state: State<'_, AppState>,
    params: SetCtMeterParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.state.config.ct_meter_installed = params.enabled;
        // Re-project registers so CT meter data (IR 60-89) appears immediately,
        // without waiting for the next simulation tick.
        let sched_ref = state.schedule.lock().await.clone();
        {
            let mut rs = state.register_store.lock().await;
            rs.project_from_state(&e.state);
            if let Some(ref sched) = sched_ref {
                rs.project_schedule_for(sched, &e.state.config.inverter_type);
            }
        }
        {
            let mut bs = state.battery_snapshot.lock().await;
            *bs = e.state.batteries.clone();
        }
        let dto = PlantStateDto::from(&e.state);
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct SetBatterySocParams {
    pub module: usize,
    pub soc: f64,
}

#[derive(serde::Deserialize)]
pub struct SetBatterySohParams {
    pub module: usize,
    pub soh: f64,
}

#[tauri::command]
pub async fn set_battery_soc(
    app: AppHandle,
    state: State<'_, AppState>,
    params: SetBatterySocParams,
) -> Result<PlantStateDto, String> {
    let mut eng = state.engine.lock().await;
    let e = eng
        .as_mut()
        .ok_or_else(|| "No plant exists — create or load a plant first".to_string())?;

    // Apply SOC directly WITHOUT running a full tick.
    // Running tick() here would let BatteryEngine/InverterEngine override
    // the user's chosen SOC (e.g. force-charge pushes it back to 100%).
    let b = e
        .state
        .batteries
        .get_mut(params.module)
        .ok_or_else(|| format!("Battery module {} does not exist", params.module + 1))?;
    b.soc_percent = params.soc.clamp(0.0, 100.0);
    // Avoid stale pre-edit power/current making the UI look like it ignored the edit.
    b.power_kw = 0.0;
    b.current_a = 0.0;
    // Hold the manual SOC for ~200 ticks (~20s at speed=10, ~2s at speed=100)
    // so the BatteryEngine doesn't immediately start drifting it away.
    e.state.manual_soc_hold_ticks = 200;
    e.state.sync_battery_from_vec();

    let sched_ref = state.schedule.lock().await.clone();
    {
        let mut rs = state.register_store.lock().await;
        rs.project_from_state(&e.state);
        if let Some(ref sched) = sched_ref {
            rs.project_schedule_for(sched, &e.state.config.inverter_type);
        }
    }
    {
        let mut bs = state.battery_snapshot.lock().await;
        *bs = e.state.batteries.clone();
    }

    // Persist immediately so a reload does not restore the old SOC.
    if let Ok(dir) = app.path().app_data_dir() {
        let persisted = crate::app_state::PersistedState {
            plant: e.state.clone(),
            schedule: sched_ref.clone(),
        };
        let path = dir.join("plant_state.json");
        if let Ok(json) = serde_json::to_string_pretty(&persisted) {
            let _ = std::fs::create_dir_all(&dir);
            let _ = std::fs::write(&path, json);
        }
    }

    let dto = PlantStateDto::with_schedule(&e.state, sched_ref.as_ref());
    let _ = app.emit("state_changed", &dto);
    Ok(dto)
}

#[tauri::command]
pub async fn set_battery_soh(
    app: AppHandle,
    state: State<'_, AppState>,
    params: SetBatterySohParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        // Apply directly without running a full tick (same rationale as set_battery_soc).
        let count = e.state.batteries.len().max(1);
        if let Some(b) = e.state.batteries.get_mut(params.module) {
            let soh = params.soh.clamp(0.0, 1.0);
            b.soh = soh;
            b.capacity_kwh = b.nominal_capacity_kwh * b.soh;
            // Derive realistic throughput from SOH using the degradation model
            b.throughput_kwh = throughput_from_soh(soh, b.nominal_capacity_kwh);
            tracing::info!(
                "Set battery SOH: module={}, SOH={}, throughput={} kWh",
                params.module,
                soh,
                b.throughput_kwh
            );
            let c_rate_kw = (b.capacity_kwh * 0.7).min(10.0);
            let inv_max_kw = e.state.config.max_ac_watts / 1000.0;
            let per_module_kw = inv_max_kw / count as f64;
            let limit = c_rate_kw.min(per_module_kw);
            b.max_charge_kw = limit;
            b.max_discharge_kw = limit;
        }
        // Ensure aggregate limits are sane
        let _ = count;
        // Update register store so IR 6-7 reflect the new throughput immediately
        {
            let mut rs = state.register_store.lock().await;
            rs.project_from_state(&e.state);
        }
        let sched_ref = state.schedule.lock().await.clone();
        let dto = PlantStateDto::with_schedule(&e.state, sched_ref.as_ref());
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Calibration / Speed control
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct StartCalibrationParams {
    pub module: Option<usize>,
}

#[tauri::command]
pub async fn start_calibration(
    app: AppHandle,
    state: State<'_, AppState>,
    params: StartCalibrationParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::StartCalibration {
            module: params.module,
        });
        e.tick();
        let sched_ref = state.schedule.lock().await.clone();
        let dto = PlantStateDto::with_schedule(&e.state, sched_ref.as_ref());
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

#[tauri::command]
pub async fn cancel_calibration(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::CancelCalibration);
        e.tick();
        let sched_ref = state.schedule.lock().await.clone();
        let dto = PlantStateDto::with_schedule(&e.state, sched_ref.as_ref());
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct SetTickIntervalParams {
    pub interval_secs: u64,
}

#[tauri::command]
pub async fn set_tick_interval(
    app: AppHandle,
    state: State<'_, AppState>,
    params: SetTickIntervalParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::SetTickInterval(params.interval_secs.max(1)));
        e.tick();
        let sched_ref = state.schedule.lock().await.clone();
        let dto = PlantStateDto::with_schedule(&e.state, sched_ref.as_ref());
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
pub struct ExportRecordingParams {
    pub path: String,
    pub format: String,
}

#[tauri::command]
pub async fn export_recording(
    app: AppHandle,
    state: State<'_, AppState>,
    params: ExportRecordingParams,
) -> Result<String, String> {
    let recording = state.recording.lock().await;
    let path = std::path::Path::new(&params.path);

    match params.format.as_str() {
        "csv" => {
            let mut f = std::fs::File::create(path).map_err(|e| e.to_string())?;
            sim_recording::write_csv(&mut f, &recording).map_err(|e| e.to_string())?;
        }
        "jsonl" => {
            sim_storage::save_recording(path, &recording).map_err(|e| e.to_string())?;
        }
        "json" => {
            let json = serde_json::to_string_pretty(&recording as &Vec<RecordingFrame>)
                .map_err(|e| e.to_string())?;
            std::fs::write(path, json).map_err(|e| e.to_string())?;
        }
        _ => return Err(format!("Unknown format: {}", params.format)),
    }

    let _ = app.emit("recording_saved", params.path.clone());
    Ok(params.path)
}

// ---------------------------------------------------------------------------
// Get Current State
// ---------------------------------------------------------------------------
// Get Current State
// ---------------------------------------------------------------------------

#[tauri::command]
pub async fn get_current_state(state: State<'_, AppState>) -> Result<PlantStateDto, String> {
    let eng = state.engine.lock().await;
    let sched_ref = state.schedule.lock().await.clone();
    match eng.as_ref() {
        Some(e) => Ok(PlantStateDto::with_schedule(&e.state, sched_ref.as_ref())),
        None => Err("No plant created yet".into()),
    }
}

// ---------------------------------------------------------------------------
// Save / Load Plant
// ---------------------------------------------------------------------------

/// Save the current plant state + schedule to a JSON file in the app data directory.
#[tauri::command]
pub async fn save_plant(app: AppHandle, state: State<'_, AppState>) -> Result<(), String> {
    let eng = state.engine.lock().await;
    let plant_state = eng
        .as_ref()
        .map(|e| e.state.clone())
        .ok_or_else(|| "No plant created yet".to_string())?;
    drop(eng);
    let schedule = state.schedule.lock().await.clone();

    let persisted = crate::app_state::PersistedState {
        plant: plant_state,
        schedule,
    };
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    std::fs::create_dir_all(&data_dir).map_err(|e| e.to_string())?;
    let path = data_dir.join("plant_state.json");
    let json = serde_json::to_string_pretty(&persisted).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    tracing::info!("Plant state saved to {}", path.display());
    Ok(())
}

/// Export the current plant config (plant state + schedule + overrides) to a JSON file.
/// The exported file can be loaded by `giv-sim serve <path> --modbus <addr>`.
#[tauri::command]
pub async fn export_config(
    _app: AppHandle,
    state: State<'_, AppState>,
    path: String,
) -> Result<(), String> {
    let eng = state.engine.lock().await;
    let plant_state = eng
        .as_ref()
        .map(|e| e.state.clone())
        .ok_or_else(|| "No plant created yet".to_string())?;
    drop(eng);
    let schedule = state.schedule.lock().await.clone();

    let persisted = crate::app_state::PersistedState {
        plant: plant_state,
        schedule,
    };
    let json = serde_json::to_string_pretty(&persisted).map_err(|e| e.to_string())?;
    std::fs::write(&path, json).map_err(|e| e.to_string())?;
    tracing::info!("Plant config exported to {}", path);
    Ok(())
}

/// Check if a saved plant state exists.
#[tauri::command]
pub async fn has_saved_plant(app: AppHandle) -> Result<bool, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let path = data_dir.join("plant_state.json");
    Ok(path.exists())
}

/// Load a saved plant state + schedule and recreate the simulation engine.
#[tauri::command]
pub async fn load_plant(
    app: AppHandle,
    tauri_state: State<'_, AppState>,
) -> Result<PlantStateDto, String> {
    let data_dir = app.path().app_data_dir().map_err(|e| e.to_string())?;
    let path = data_dir.join("plant_state.json");
    let json = std::fs::read_to_string(&path).map_err(|e| e.to_string())?;

    // Try new PersistedState format first, fall back to plain PlantState
    let (plant_state, schedule_opt): (sim_models::PlantState, Option<sim_core::Schedule>) =
        if let Ok(ps) = serde_json::from_str::<crate::app_state::PersistedState>(&json) {
            (ps.plant, ps.schedule)
        } else {
            let ps =
                serde_json::from_str::<sim_models::PlantState>(&json).map_err(|e| e.to_string())?;
            (ps, None)
        };

    // Restore schedule into AppState
    {
        let mut sched = tauri_state.schedule.lock().await;
        *sched = schedule_opt.clone();
    }

    let peak_watts = plant_state.config.solar_peak_watts;
    let latitude = plant_state.config.latitude;
    let tick_interval = plant_state.config.tick_interval_secs;

    let devices: Vec<Box<dyn DeviceModel>> = if let Some(ref sched) = schedule_opt {
        vec![
            Box::new(ScheduleEngine::new(sched.clone())),
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
            Box::new(EvcEngine::new()),
        ]
    } else {
        vec![
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
            Box::new(EvcEngine::new()),
        ]
    };

    let mut engine = SimulationEngine::new(plant_state, devices, tick_interval);
    // Loaded plants run live: lock the clock to the host wall clock so the
    // served/displayed inverter time matches the computer's time (no drift).
    engine.anchor_to_wall_clock(None);

    // Populate register store so Modbus clients see non-zero values immediately.
    {
        let mut rs = tauri_state.register_store.lock().await;
        rs.project_from_state(&engine.state);
    }

    let dto = {
        let mut eng = tauri_state.engine.lock().await;
        *eng = Some(engine);
        eng.as_ref()
            .map(|e| PlantStateDto::with_schedule(&e.state, schedule_opt.as_ref()))
            .unwrap()
    };

    let _ = app.emit("state_changed", &dto);
    Ok(dto)
}

// ---------------------------------------------------------------------------
// EVC (Electric Vehicle Charger) commands
// ---------------------------------------------------------------------------

/// Toggle the EVC on/off. When off, the charger draws no power regardless
/// of cable state or charge_control.
#[tauri::command]
pub async fn set_evc_enabled(state: State<'_, AppState>, enabled: bool) -> Result<(), String> {
    {
        let mut evc = state.evc_state.lock().await;
        evc.enabled = enabled;
    }
    // Also propagate to the engine's PlantState so the next tick reflects it
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.state.evc.enabled = enabled;
    }
    Ok(())
}

/// Set charge_control register (0=Ready, 1=Start, 2=Stop).
#[tauri::command]
pub async fn set_evc_charge_control(state: State<'_, AppState>, mode: u16) -> Result<(), String> {
    let mode = mode.min(2);
    {
        let mut evc = state.evc_state.lock().await;
        evc.charge_control = mode;
    }
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.state.evc.charge_control = mode;
    }
    Ok(())
}

/// Set the charge current limit (deci-Amps, ×10). Clamped to 60–320 (6.0–32.0 A).
#[tauri::command]
pub async fn set_evc_charge_current(
    state: State<'_, AppState>,
    deci_amps: u16,
) -> Result<(), String> {
    let deci_amps = deci_amps.clamp(60, 320);
    {
        let mut evc = state.evc_state.lock().await;
        evc.charge_current_limit = deci_amps;
    }
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.state.evc.charge_current_limit = deci_amps;
    }
    Ok(())
}

/// Simulate plugging / unplugging the charging cable.
/// Sets connection_status (0=Not Connected, 1=Connected) and drives the
/// charging state machine via EvcEngine.
#[tauri::command]
pub async fn set_evc_cable_status(state: State<'_, AppState>, status: u16) -> Result<(), String> {
    let status = status.min(1);
    {
        let mut evc = state.evc_state.lock().await;
        evc.connection_status = status;
        if status == 0 {
            // Cable unplugged → back to Idle, reset power
            evc.charging_state = 1;
            evc.active_power_w = 0.0;
            evc.active_power_l1 = 0.0;
            evc.active_power_l2 = 0.0;
            evc.active_power_l3 = 0.0;
            evc.current_l1 = 0.0;
            evc.current_l2 = 0.0;
            evc.current_l3 = 0.0;
            evc.session_energy_kwh = 0.0;
            evc.session_duration_secs = 0;
        }
    }
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.state.evc.connection_status = status;
    }
    Ok(())
}

/// Return current EVC state for the frontend.
#[tauri::command]
pub async fn get_evc_state(state: State<'_, AppState>) -> Result<sim_models::EvcState, String> {
    Ok(state.evc_state.lock().await.clone())
}

/// Set the EVC Modbus TCP port. Takes effect on next restart.
#[tauri::command]
pub async fn set_evc_port(state: State<'_, AppState>, port: u16) -> Result<(), String> {
    if port == 0 {
        return Err("Port must be >= 1".to_string());
    }
    {
        let mut p = state.evc_port.lock().map_err(|e| e.to_string())?;
        *p = port;
    }
    Ok(())
}

/// Get the current EVC Modbus TCP port.
#[tauri::command]
pub async fn get_evc_port(state: State<'_, AppState>) -> Result<u16, String> {
    let port = state.evc_port.lock().map_err(|e| e.to_string())?;
    Ok(*port)
}

// ---------------------------------------------------------------------------
// Firmware commands
// ---------------------------------------------------------------------------

/// Override the DSP firmware version (HR 19). Useful for testing how a client
/// behaves with different firmware versions.
#[tauri::command]
pub async fn set_dsp_firmware(state: State<'_, AppState>, version: u16) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.state.inverter.dsp_firmware_version = version;
    }
    Ok(())
}

/// Override the ARM firmware version (HR 21). Changing the century
/// (fw / 100) lets you simulate Gen1/Gen2/Gen3 identification against
/// the shared 0x2001 family DTC.
#[tauri::command]
pub async fn set_arm_firmware(state: State<'_, AppState>, version: u16) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.state.inverter.arm_firmware_version = version;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_throughput_from_soh_calculation() {
        // Test SOH 0.781 (approx 3 years old, 1 cycle/day)
        let soh = 0.781;
        let capacity = 9.5;
        let throughput = throughput_from_soh(soh, capacity);

        // Expected: cycles = (1.0 - 0.781) / 0.0002 = 1095 cycles
        // throughput = 1095 * 9.5 = 10402.5 kWh
        assert!(
            (throughput - 10402.5).abs() < 1.0,
            "Expected ~10402.5, got {}",
            throughput
        );
        println!("SOH {}: throughput = {} kWh", soh, throughput);

        // Test new battery (SOH 1.0 = 0 throughput)
        let throughput_new = throughput_from_soh(1.0, 9.5);
        assert_eq!(throughput_new, 0.0, "New battery should have 0 throughput");

        // Test 50% SOH (end of life)
        let throughput_eol = throughput_from_soh(0.5, 9.5);
        let expected_eol = (1.0 - 0.5) / 0.0002 * 9.5; // 2500 cycles * 9.5 = 23750
        assert!((throughput_eol - expected_eol).abs() < 1.0);
    }
}
