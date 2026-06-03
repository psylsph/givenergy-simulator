//! Tauri IPC commands.
//!
//! All #[tauri::command] functions must live in a separate module
//! to avoid a proc-macro namespace collision (E0255) in lib targets.

use crate::app_state::{AppState, PlantStateDto};
use sim_core::{
    BatteryEngine, Command, EnergyTracker, InverterEngine, InverterMode, LoadEngine, LoadProfile,
    ScheduleEngine, SimulationEngine, SolarEngine, WeatherCondition,
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
        // Gen3 Hybrid 3.6/5.0: charge 3300W, discharge 3600W. Use 3600 as the DC battery limit.
        "Gen3Hybrid" => 3600.0,
        // Gen3 Hybrid 8.0: charge 8000W, discharge 8500W
        "Gen3Hybrid8kW" => 8000.0,
        // Gen3 Hybrid 10.0: charge 10000W, discharge 10500W
        "Gen3Hybrid10kW" => 10000.0,
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
        // 3-Phase 6kW: charge/discharge 6000W
        "ThreePhase" => 6000.0,
        _ => 3600.0,
    };
    let max_batt_kw = max_batt_w / 1000.0;

    let now = chrono::Local::now().naive_local();

    // Build battery modules from per-module config, or fall back to battery_count
    let mut plant_state = if let Some(modules) = params.battery_modules {
        let batts: Vec<BatteryState> = modules
            .into_iter()
            .take(3)
            .map(|m| {
                let soh = m.soh.clamp(0.0, 1.0);
                let capacity = m.capacity_kwh.max(1.0);
                let effective_capacity = capacity * soh;
                let c_rate_kw = (effective_capacity * 0.3).min(10.0);
                BatteryState {
                    capacity_kwh: effective_capacity,
                    nominal_capacity_kwh: capacity,
                    soh,
                    max_charge_kw: c_rate_kw.min(max_batt_kw),
                    max_discharge_kw: c_rate_kw.min(max_batt_kw),
                    ..BatteryState::default()
                }
            })
            .collect();
        let mut state = sim_models::PlantState::new(now);
        state.batteries = batts;
        state.sync_battery_from_vec();
        state
    } else {
        let battery_count = params.battery_count.unwrap_or(1).clamp(1, 3);
        sim_models::PlantState::with_battery_count(now, battery_count)
    };
    plant_state.config.solar_peak_watts = peak_watts;
    plant_state.config.latitude = latitude;
    plant_state.config.tick_interval_secs = tick_interval;
    plant_state.config.pv2_peak_watts = params.pv2_peak_watts.unwrap_or(0.0);
    plant_state.config.inverter_type = inv_type.to_string();
    plant_state.config.max_ac_watts = match plant_state.config.inverter_type.as_str() {
        // Gen3 Hybrid 8.0: 8000W nominal AC output
        "Gen3Hybrid8kW" => 8000.0,
        // Gen3 Hybrid 10.0: 10000W nominal AC output
        "Gen3Hybrid10kW" => 10000.0,
        "AllInOne6" => 6000.0,
        "AIO8kW" => 8000.0,
        "AIO10kW" => 10000.0,
        "ThreePhase" => 6000.0,
        "ACCoupled" | "ACCoupled2" => 3000.0,
        // AllInOne (original 0x8002): 6kW continuous
        "AllInOne" => 6000.0,
        "AllInOne5" => 5000.0,
        // Gen 1 Hybrid 5.0: 5000W nominal AC output
        "Gen1Hybrid" => 5000.0,
        // Gen3Hybrid 3.6/5.0 default to 5000W
        _ => 5000.0,
    };
    plant_state.inverter.export_limit_w = plant_state.config.max_ac_watts * 0.72;

    // Ensure a default schedule exists (charge slot 00:00-05:30)
    {
        let mut sched = state.schedule.lock().await;
        if sched.is_none() {
            *sched = Some(sim_core::Schedule::default());
        }
    }
    let schedule_opt = state.schedule.lock().await.clone();
    let devices: Vec<Box<dyn DeviceModel>> = if let Some(ref sched) = schedule_opt {
        vec![
            Box::new(ScheduleEngine::new(sched.clone())),
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(profile)),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
        ]
    } else {
        vec![
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(profile)),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
        ]
    };

    let engine = SimulationEngine::new(plant_state, devices, tick_interval);

    let plant_state = {
        let mut eng = state.engine.lock().await;
        *eng = Some(engine);
        eng.as_ref().map(|e| e.state.clone()).unwrap()
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
        // HR 27: Battery power mode (0=export, 1=eco)
        27 => {
            let mode = match value {
                1 => InverterMode::Eco,
                _ => InverterMode::Normal,
            };
            Some(Command::SetInverterMode(mode))
        }
        // HR 50: Active power rate (%) → export limit = rate% of max
        50 => None, // handled in register store
        // HR 96: Enable charge — handled via schedule update below
        // HR 110: Battery SOC reserve (%)
        110 => Some(Command::SetMinSoc(value as f64)),
        // HR 111: Battery charge limit (%) — register-only
        111 => None,
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
            | 242 | 245 | 272 | 275
            | 1109 | 1111 | 1113..=1116 | 1118..=1121
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

    let engine = state.engine.clone();
    let register_store = state.register_store.clone();
    let recording = state.recording.clone();
    let running_flag = state.running.clone();
    let app_handle = app.clone();

    let modbus_cmds = state.modbus_cmds.clone();
    let battery_snapshot = state.battery_snapshot.clone();
    let pending_time_regs = state.pending_time_regs.clone();
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
                            // Charge slot 2 (HR 31-32)
                            if let Some(&v) = sched_updates.get(&31) {
                                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&32) {
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
                            *schedule_arc.lock().await = Some(sched);
                        }

                        e.tick();
                        {
                            let mut rs = register_store.lock().await;
                            rs.project_from_state(&e.state);
                            let sched_ref = schedule_arc.lock().await.clone();
                            if let Some(ref s) = sched_ref {
                                rs.project_schedule(s);
                            }
                        }
                        // Update battery snapshot for Modbus BMS reads
                        {
                            let mut bs = battery_snapshot.lock().await;
                            *bs = e.state.batteries.clone();
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
                            // Charge slot 2 (HR 31-32)
                            if let Some(&v) = sched_updates.get(&31) {
                                sched.charge_start_2 = hhmm_to_hours(v).unwrap_or(0.0);
                            }
                            if let Some(&v) = sched_updates.get(&32) {
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
                            *schedule_arc.lock().await = Some(sched);
                        }

                        e.tick();
                        {
                            let mut rs = register_store.lock().await;
                            rs.project_from_state(&e.state);
                            let sched_ref = schedule_arc.lock().await.clone();
                            if let Some(ref s) = sched_ref {
                                rs.project_schedule(s);
                            }
                        }
                        // Update battery snapshot for Modbus BMS reads
                        {
                            let mut bs = battery_snapshot.lock().await;
                            *bs = e.state.batteries.clone();
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
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::SetBatterySoc {
            module: params.module,
            soc: params.soc,
        });
        // Apply immediately and emit
        e.tick();
        let sched_ref = state.schedule.lock().await.clone();
        let dto = PlantStateDto::with_schedule(&e.state, sched_ref.as_ref());
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

#[tauri::command]
pub async fn set_battery_soh(
    app: AppHandle,
    state: State<'_, AppState>,
    params: SetBatterySohParams,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.enqueue(Command::SetBatterySoH {
            module: params.module,
            soh: params.soh,
        });
        e.tick();
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
        ]
    } else {
        vec![
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
        ]
    };

    let engine = SimulationEngine::new(plant_state, devices, tick_interval);
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
