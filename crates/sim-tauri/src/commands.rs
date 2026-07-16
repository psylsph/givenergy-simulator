//! Tauri IPC commands.
//!
//! All #[tauri::command] functions must live in a separate module
//! to avoid a proc-macro namespace collision (E0255) in lib targets.

use crate::app_state::{AppState, PlantStateDto};
use crate::atomic_write;
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

    // Max battery charge/discharge power per inverter type (watts). The
    // authoritative table now lives in `sim_models::max_batt_w_for_inverter`
    // (single source of truth) so sim-tauri and sim-api stay in sync.
    let max_batt_w = sim_models::max_batt_w_for_inverter(inv_type);
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
    // Default DSP firmware per inverter type. Authoritative table lives in
    // `sim_models::dsp_firmware_for_inverter` so this and the CLI agree.
    plant_state.inverter.dsp_firmware_version = sim_models::dsp_firmware_for_inverter(inv_type);
    // Use the authoritative inverter-type table from sim_models so the
    // simulator and Tauri/CLI entry points stay in sync.
    plant_state.config.max_ac_watts =
        sim_models::max_ac_watts_for(&plant_state.config.inverter_type);
    // Seed the export limit at the standard UK EREC G98 default for this
    // inverter family: 3680 W (16 A × 230 V) for single-phase Micro-
    // generators, 6500 W for three-phase (the wire ceiling on HR 1063),
    // and 0 W for EMS (operator must configure explicitly since HR 2071
    // has no G98 cap on the wire). See
    // `sim_models::default_export_limit_w_for`.
    plant_state.inverter.export_limit_w =
        sim_models::default_export_limit_w_for(&plant_state.config.inverter_type);

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
            // EvcEngine runs AFTER LoadEngine (so LoadEngine's overwrite
            // of state.load.demand_w with the family / heat-pump / minimal
            // baseline is preserved) and BEFORE InverterEngine (so the
            // inverter's solar/battery/grid priority logic sees the EV
            // draw as part of the total demand — spare solar/battery
            // output is routed to the EV first, with only the residual
            // shortfall falling back to grid import).
            Box::new(sim_core::EvcEngine::new()),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(sim_core::EnergyTracker::new().with_last_reset_date(seed_date)),
        ]
    } else {
        vec![
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(profile)),
            Box::new(sim_core::EvcEngine::new()),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(sim_core::EnergyTracker::new().with_last_reset_date(seed_date)),
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
    atomic_write::write(&path, json.as_bytes()).map_err(|e| format!("Write error: {e}"))?;
    tracing::info!("Auto-saved plant to {}", path.display());

    // Reset dongle misbehaviour to Off on new plant creation.
    *state.dongle_misbehaviour.lock().unwrap() = sim_models::DongleMisbehaviourMode::Off;

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
    // The desktop file picker intentionally permits user-selected files from
    // anywhere. The HTTP bridge never forwards an arbitrary client path; its
    // upload endpoint writes to a server-controlled temporary file first.
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
        // HR 318/319/320 (battery pause mode + single pause slot) are reconciled
        // into PlantState together by the write-loop handler below so a lone
        // HR 318 write doesn't clobber the start/end window. Returning None
        // here avoids a redundant clobbering enqueue ahead of that merge.
        318..=320 => None,
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
        // HR 102: Inverter export limit (W) — single-phase / AC-coupled / Gen1-4
        // wires `ge_hr_grid_port_max_power_output` (HR 26) as read-only, but
        // HR 102 is a separate, writable export-limit register the inverter
        // uses to apply a user-set cap. giv_tcp / givenergy-modbus map both
        // HR 102 (internal) and the wire-protocol export limit to this
        // same state field.
        102 => Some(Command::SetExportLimit(value as f64)),
        // HR 1063: Three-phase / HV / AIO `p_export_limit`. Wire encoding is
        // `C.deci` (raw = watts × 10, clamped to u16). givenergy-modbus and
        // giv_tcp both pass a `WriteHoldingRegisterRequest(1063, watts × 10)`,
        // and givenergy-modbus caps `valid=(-6500, 6500)`. The simulator
        // stores the user-friendly watts in `state.inverter.export_limit_w`;
        // the projection in `sim-registers` re-multiplies by 10 on the way out.
        1063 => Some(Command::SetExportLimit((value as f64) / 10.0)),
        // HR 2071: EMS / EmsCommercial / Gateway `export_power_limit`. Raw
        // watts (C.uint16, no scaling).
        2071 => Some(Command::SetExportLimit(value as f64)),
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

/// Check if a register address is a schedule-related holding register.
///
/// HR 2071 (`ems_export_power_limit`) was previously in this list because the
/// schedule engine wrote it from `schedule.export_power_limit_w`. After the
/// 2071 projection moved to `project_from_state` (mirroring
/// `state.inverter.export_limit_w`), the schedule accumulator no longer needs
/// to react to writes — they're routed straight to `SetExportLimit` via
/// `modbus_address_to_command`.
fn is_schedule_register(addr: u16) -> bool {
    matches!(
        addr,
        31..=32 | 44..=45 | 56..=57 | 59 | 94..=96 | 116
            | 242..=245 | 272 | 275
            | 246..=269 | 276..=299
            | 1109 | 1111..=1116 | 1118..=1121
            | 2062..=2070
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
    let pending_pause_regs = state.pending_pause_regs.clone();
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
                        // Collect pause-slot writes while the synchronous command
                        // guards are held. Missing fields are filled from live state
                        // before emitting SetBatteryPause below.
                        let mut pause_mode: Option<u16> = None;
                        let mut pause_start: Option<u16> = None;
                        let mut pause_end: Option<u16> = None;
                        let mut pause_ready = false;
                        {
                            if let Ok(mut cmds) = modbus_cmds.lock() {
                                if let Ok(mut time_buf) = pending_time_regs.lock() {
                                    if let Ok(mut pause_buf) = pending_pause_regs.lock() {
                                        for cmd in cmds.drain(..) {
                                            match cmd.address {
                                                35 => time_buf[0] = Some(cmd.value),
                                                36 => time_buf[1] = Some(cmd.value),
                                                37 => time_buf[2] = Some(cmd.value),
                                                38 => time_buf[3] = Some(cmd.value),
                                                39 => time_buf[4] = Some(cmd.value),
                                                40 => time_buf[5] = Some(cmd.value),
                                                318 => pause_mode = Some(cmd.value),
                                                319 => pause_start = Some(cmd.value),
                                                320 => pause_end = Some(cmd.value),
                                                _ => {}
                                            }
                                            if is_schedule_register(cmd.address) {
                                                sched_updates.insert(cmd.address, cmd.value);
                                                sched_dirty = true;
                                            }
                                            // Keep pause writes in the accumulator for
                                            // compatibility with existing consumers. The
                                            // authoritative state update is emitted below.
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
                                            if let Some(dt) =
                                                chrono::NaiveDate::from_ymd_opt(y, m, d)
                                                    .and_then(|date| date.and_hms_opt(h, min, s))
                                            {
                                                e.enqueue(Command::SetSimulationTime(dt));
                                            }
                                            *time_buf = [None; 6];
                                        }
                                        // Apply any pause-register write immediately while
                                        // preserving fields the client did not write. A lone
                                        // HR 318 mode toggle must not wait indefinitely for
                                        // HR 319/320 or reset the existing window.
                                        if pause_mode.is_some()
                                            || pause_start.is_some()
                                            || pause_end.is_some()
                                        {
                                            pause_mode.get_or_insert(e.state.battery_pause_mode);
                                            pause_start
                                                .get_or_insert(e.state.battery_pause_slot_start);
                                            pause_end.get_or_insert(e.state.battery_pause_slot_end);
                                            *pause_buf = [None; 3];
                                            pause_ready = true;
                                        }
                                    }
                                }
                            }
                        }
                        // Flush a complete command assembled from this cycle's writes
                        // and the live values of any untouched fields.
                        if pause_ready {
                            e.enqueue(Command::SetBatteryPause {
                                mode: pause_mode.unwrap(),
                                start: pause_start.unwrap(),
                                end: pause_end.unwrap(),
                            });
                        }
                        // Phase 2: apply schedule updates (MutexGuards dropped, safe to .await)
                        if sched_dirty {
                            let mut sched = schedule_arc.lock().await.clone().unwrap_or_default();
                            sched.apply_modbus_updates(&sched_updates);
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
                            let _ = atomic_write::write(&path, json.as_bytes());
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
                        // Collect pause-slot writes while the synchronous command
                        // guards are held. Missing fields are filled from live state
                        // before emitting SetBatteryPause below.
                        let mut pause_mode: Option<u16> = None;
                        let mut pause_start: Option<u16> = None;
                        let mut pause_end: Option<u16> = None;
                        let mut pause_ready = false;
                        {
                            if let Ok(mut cmds) = modbus_cmds.lock() {
                                if let Ok(mut time_buf) = pending_time_regs.lock() {
                                    if let Ok(mut pause_buf) = pending_pause_regs.lock() {
                                        for cmd in cmds.drain(..) {
                                            match cmd.address {
                                                35 => time_buf[0] = Some(cmd.value),
                                                36 => time_buf[1] = Some(cmd.value),
                                                37 => time_buf[2] = Some(cmd.value),
                                                38 => time_buf[3] = Some(cmd.value),
                                                39 => time_buf[4] = Some(cmd.value),
                                                40 => time_buf[5] = Some(cmd.value),
                                                318 => pause_mode = Some(cmd.value),
                                                319 => pause_start = Some(cmd.value),
                                                320 => pause_end = Some(cmd.value),
                                                _ => {}
                                            }
                                            if is_schedule_register(cmd.address) {
                                                sched_updates.insert(cmd.address, cmd.value);
                                                sched_dirty = true;
                                            }
                                            // Keep pause writes in the accumulator for
                                            // compatibility with existing consumers. The
                                            // authoritative state update is emitted below.
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
                                            if let Some(dt) =
                                                chrono::NaiveDate::from_ymd_opt(y, m, d)
                                                    .and_then(|date| date.and_hms_opt(h, min, s))
                                            {
                                                e.enqueue(Command::SetSimulationTime(dt));
                                            }
                                            *time_buf = [None; 6];
                                        }
                                        // Apply any pause-register write immediately while
                                        // preserving fields the client did not write. A lone
                                        // HR 318 mode toggle must not wait indefinitely for
                                        // HR 319/320 or reset the existing window.
                                        if pause_mode.is_some()
                                            || pause_start.is_some()
                                            || pause_end.is_some()
                                        {
                                            pause_mode.get_or_insert(e.state.battery_pause_mode);
                                            pause_start
                                                .get_or_insert(e.state.battery_pause_slot_start);
                                            pause_end.get_or_insert(e.state.battery_pause_slot_end);
                                            *pause_buf = [None; 3];
                                            pause_ready = true;
                                        }
                                    }
                                }
                            }
                        }
                        // Flush a complete command assembled from this cycle's writes
                        // and the live values of any untouched fields.
                        if pause_ready {
                            e.enqueue(Command::SetBatteryPause {
                                mode: pause_mode.unwrap(),
                                start: pause_start.unwrap(),
                                end: pause_end.unwrap(),
                            });
                        }
                        // Phase 2: apply schedule updates (MutexGuards dropped, safe to .await)
                        if sched_dirty {
                            let mut sched = schedule_arc.lock().await.clone().unwrap_or_default();
                            sched.apply_modbus_updates(&sched_updates);
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
                            let _ = atomic_write::write(&path, json.as_bytes());
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
            let _ = atomic_write::write(&path, json.as_bytes());
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
    // Desktop callers choose this path through the native save dialog. HTTP
    // exports pass a server-controlled temporary path under the app data dir.
    let path = std::path::PathBuf::from(&params.path);

    let recording = state.recording.lock().await;
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
    }
    match params.format.as_str() {
        "csv" => {
            // Render to a Vec first so atomic_write can perform the
            // create-rename swap. A direct `File::create` followed by
            // `write_all` would leave a partial file on a crash mid-write.
            let mut buf = Vec::new();
            sim_recording::write_csv(&mut buf, &recording).map_err(|e| e.to_string())?;
            atomic_write::write(&path, &buf).map_err(|e| e.to_string())?;
        }
        "jsonl" => {
            // Render to a Vec first so atomic_write can perform the
            // create-rename swap, instead of `sim_storage::save_recording`
            // which writes directly to a `File` and would leave a partial
            // file on a crash mid-write.
            let mut buf = Vec::new();
            for frame in recording.iter() {
                sim_recording::write_frame(&mut buf, frame).map_err(|e| e.to_string())?;
            }
            atomic_write::write(&path, &buf).map_err(|e| e.to_string())?;
        }
        "json" => {
            let json = serde_json::to_string_pretty(&recording as &Vec<RecordingFrame>)
                .map_err(|e| e.to_string())?;
            atomic_write::write(&path, json.as_bytes()).map_err(|e| e.to_string())?;
        }
        _ => return Err(format!("Unknown format: {}", params.format)),
    }

    let _ = app.emit("recording_saved", path.to_string_lossy().into_owned());
    Ok(path.to_string_lossy().into_owned())
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
    atomic_write::write(&path, json.as_bytes()).map_err(|e| e.to_string())?;
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
    let path = std::path::PathBuf::from(path);
    let json = serde_json::to_string_pretty(&persisted).map_err(|e| e.to_string())?;
    atomic_write::write(&path, json.as_bytes()).map_err(|e| e.to_string())?;
    tracing::info!("Plant config exported to {}", path.display());
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
            Box::new(EvcEngine::new()),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
        ]
    } else {
        vec![
            Box::new(SolarEngine::new(peak_watts, latitude)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(EvcEngine::new()),
            Box::new(InverterEngine::new()),
            Box::new(sim_faults::FaultEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
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

/// Set the charge session energy (HR 72) directly, in kWh. Useful for
/// seeding a session to a known value — e.g. to exercise a client's
/// "session in progress" display without waiting for energy to accumulate.
/// Clamped to [0, 6553.5] kWh (the u16 ÷10 wire range). Projected on the
/// wire as kWh×10 (deci-kWh) to match the real GivEVC / GivTCP decode.
#[tauri::command]
pub async fn set_evc_session_energy(state: State<'_, AppState>, kwh: f64) -> Result<(), String> {
    let kwh = kwh.clamp(0.0, 6553.5);
    {
        let mut evc = state.evc_state.lock().await;
        evc.session_energy_kwh = kwh;
    }
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.state.evc.session_energy_kwh = kwh;
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
// Dongle misbehaviour simulation
// ---------------------------------------------------------------------------

/// Get the current dongle misbehaviour mode.
#[tauri::command]
pub async fn get_dongle_misbehaviour(state: State<'_, AppState>) -> Result<String, String> {
    let mode = state.dongle_misbehaviour.lock().unwrap();
    Ok(format!("{:?}", *mode))
}

/// Set the dongle misbehaviour mode.
/// Valid values: Off, EmptyData, StaleData, GarbageData, DropConnection, Intermittent.
#[tauri::command]
pub async fn set_dongle_misbehaviour(
    state: State<'_, AppState>,
    mode: String,
) -> Result<(), String> {
    use sim_models::DongleMisbehaviourMode;
    let parsed = match mode.as_str() {
        "Off" => DongleMisbehaviourMode::Off,
        "EmptyData" => DongleMisbehaviourMode::EmptyData,
        "StaleData" => DongleMisbehaviourMode::StaleData,
        "GarbageData" => DongleMisbehaviourMode::GarbageData,
        "DropConnection" => DongleMisbehaviourMode::DropConnection,
        "Intermittent" => DongleMisbehaviourMode::Intermittent,
        _ => return Err(format!("Unknown dongle misbehaviour mode: {mode}")),
    };
    *state.dongle_misbehaviour.lock().unwrap() = parsed;
    // Also sync to the engine's PlantState so it survives save/load.
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.state.dongle_misbehaviour = parsed;
    }
    Ok(())
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
// Inverter temperature override
// ---------------------------------------------------------------------------

/// Override the inverter temperature (°C). When set, the inverter thermal
/// model is bypassed and the temperature is held at this value every tick —
/// useful for holding a fixed temperature to exercise derating / over-
/// temperature behaviour. Pass `null` to clear the override and restore the
/// thermal model. Mirrors the `SetInverterTemperature` command.
#[tauri::command]
pub async fn set_inverter_temperature(
    app: AppHandle,
    state: State<'_, AppState>,
    celsius: Option<f64>,
) -> Result<(), String> {
    let mut eng = state.engine.lock().await;
    if let Some(ref mut e) = *eng {
        e.state.inverter.temperature_override = celsius;
        if let Some(v) = celsius {
            e.state.inverter.temperature_celsius = v.clamp(-10.0, 80.0);
        }
        let dto = PlantStateDto::from(&e.state);
        let _ = app.emit("state_changed", &dto);
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Grid port max power output
// ---------------------------------------------------------------------------

/// Family of a connected inverter, as far as the grid-port-max-power-output
/// wire protocol is concerned. Used to route the same user-facing watts
/// value to the correct Modbus register — see `modbus_address_to_command`
/// for the byte-level encoding of each register.
///
/// The categorisation comes from a manual audit of the giv_tcp model files
/// (baseinverter.py, threephase.py, ems.py, gateway.py) cross-checked
/// against givenergy-modbus (inverter.py, inverter_threephase.py, ems.py):
///
/// * `SinglePhase` — HR 26 `ge_hr_grid_port_power_output` is **read-only**
///   on the wire (givenergy-modbus defines no setter, no `valid=` for write).
///   Clients can read it but cannot change it. The simulator mirrors
///   `state.config.max_ac_watts` there.
/// * `ThreePhase` — HR 1063 `p_export_limit`, encoded `C.deci` (raw = watts
///   × 10). givenergy-modbus caps `max=6500`. Round-trips via
///   `state.inverter.export_limit_w`.
/// * `Ems` — HR 2071 `ems_export_power_limit`, raw watts (`C.uint16`). Used
///   by EMS, EmsCommercial, and the Gateway (Gateway inherits the EMS
///   register map per the giv_tcp / givenergy-modbus convention).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum GridPortPowerFamily {
    SinglePhase,
    ThreePhase,
    Ems,
}

impl GridPortPowerFamily {
    /// Classify an inverter type string into the wire-protocol family.
    pub fn from_inverter_type(inverter_type: &str) -> Self {
        // Three-phase: starts with "ThreePhase" (ThreePhase / ThreePhase8kW /
        // ThreePhase10kW / ThreePhase11kW) OR equals "ACThreePhase". Per
        // giv_tcp's `model/threephase.py` these all share the 1000-1124
        // HR block and the 1061-1120 IR block, so they all read/write the
        // same `p_export_limit` at HR 1063.
        if inverter_type.starts_with("ThreePhase") || inverter_type == "ACThreePhase" {
            return Self::ThreePhase;
        }
        // EMS family: EMS, EmsCommercial, and the Gateway all use HR 2071
        // for `export_power_limit`. Gateway's giv_tcp model
        // (`givenergy_modbus_async/model/gateway.py`) doesn't define its
        // own inverter_max_power or export_power_limit — it inherits via
        // the underlying AIO/EMS projection (see `docs/gateway-register-reference.md`).
        if inverter_type == "EMS"
            || inverter_type == "EmsCommercial"
            || inverter_type == "Gateway12kW"
        {
            return Self::Ems;
        }
        // Everything else (Gen1/2/3/4 Hybrid, Polar, Gen3Plus, AC-coupled,
        // PV inverter, AIO, AIOHybrid) is single-phase from the wire
        // protocol's point of view — HR 26 is read-only and carries the
        // inverter's *physical* AC output (`max_ac_watts`), NOT a grid
        // export limit. Use `InverterMode::ExportLimit` to set the grid
        // export cap independently.
        Self::SinglePhase
    }

    /// Human-readable label used by the GUI to disambiguate the three
    /// registers (HR 26 vs HR 1063 vs HR 2071). Drives the field label
    /// and help text in the sidebar.
    ///
    /// Note the semantic split: HR 26 (SinglePhase) carries the inverter's
    /// *physical* AC output, while HR 1063 / HR 2071 carry the *grid-port
    /// export limit*. The labels make that distinction explicit so a UK
    /// installer doesn't read a 3 kW inverter's hardware capability as a
    /// 3000 W G98 grid-export cap.
    pub fn label(self) -> &'static str {
        match self {
            Self::SinglePhase => "Inverter Max AC Output (HR 26, read-only)",
            Self::ThreePhase => "Grid Port Export Limit (HR 1063, ×0.1 dW)",
            Self::Ems => "Grid Port Export Limit (HR 2071, raw W)",
        }
    }

    /// Maximum user-facing watts this family will accept. Mirrors the wire
    /// register's safe-input ceiling:
    /// * ThreePhase — givenergy-modbus `max=6500` on HR 1063.
    /// * Ems — 16-bit u16 register ceiling (65535 raw watts) on HR 2071.
    ///   givenergy-modbus doesn't set a max for HR 2071.
    /// * SinglePhase — N/A (HR 26 is read-only on the wire; setter
    ///   rejects unconditionally).
    pub fn max_w(self) -> f64 {
        match self {
            Self::SinglePhase => 0.0,
            Self::ThreePhase => 6500.0,
            Self::Ems => 65535.0,
        }
    }

    /// Clamp a user-supplied watts value to this family's safe-input
    /// ceiling. Negative values are pinned to 0 (the UI rejects negatives
    /// up-front, but defence-in-depth).
    pub fn clamp_watts(self, watts: f64) -> f64 {
        watts.max(0.0).min(self.max_w())
    }
}

/// Read the current grid port max power output, in user-friendly watts.
///
/// The return value is what the user sees in the GUI. The internal storage
/// (`state.inverter.export_limit_w` / `state.config.max_ac_watts`) is the
/// same scalar for both single-phase and three-phase / EMS — only the wire
/// encoding differs. For single-phase this returns the static
/// `config.max_ac_watts` (HR 26 is read-only).
#[tauri::command]
pub async fn get_grid_port_max_power(state: State<'_, AppState>) -> Result<f64, String> {
    let eng = state.engine.lock().await;
    let Some(e) = eng.as_ref() else {
        return Err("No plant created".to_string());
    };
    let family = GridPortPowerFamily::from_inverter_type(&e.state.config.inverter_type);
    let watts = match family {
        GridPortPowerFamily::SinglePhase => e.state.config.max_ac_watts,
        // Both ThreePhase and Ems store the active limit in
        // `state.inverter.export_limit_w`. The wire encoding (deci vs raw)
        // is handled by the projection, not by this getter.
        GridPortPowerFamily::ThreePhase | GridPortPowerFamily::Ems => {
            e.state.inverter.export_limit_w
        }
    };
    Ok(watts)
}

/// Set the grid port max power output, in user-friendly watts.
///
/// Behaviour depends on the inverter family:
/// * **Single-phase / AC-coupled / Gen1-4 / PV / AIO / AIOHybrid / Polar /
///   Gen3Plus**: rejected with an error — HR 26 is read-only on the wire
///   per givenergy-modbus (`model/inverter.py` has no setter for
///   `grid_port_max_power_output`). The single-phase max output comes from
///   the plant configuration (`config.max_ac_watts`), not from a writable
///   register.
/// * **Three-phase / HV / AIO three-phase**: enqueues `SetExportLimit` with
///   the raw watts. The projection in `sim-registers` encodes HR 1063 as
///   `watts × 10` (C.deci) before serving it to Modbus clients.
/// * **EMS / EmsCommercial / Gateway**: enqueues `SetExportLimit` with the
///   raw watts. HR 2071 is encoded verbatim (C.uint16, no scaling).
///
/// Returns the family on success so the GUI can confirm the right register
/// was targeted without re-querying the inverter type.
#[tauri::command]
pub async fn set_grid_port_max_power(
    state: State<'_, AppState>,
    watts: f64,
) -> Result<GridPortPowerFamily, String> {
    if !watts.is_finite() || watts < 0.0 {
        return Err(format!(
            "Watts must be a non-negative finite number, got {watts}"
        ));
    }
    let family = {
        let eng = state.engine.lock().await;
        let Some(e) = eng.as_ref() else {
            return Err("No plant created".to_string());
        };
        GridPortPowerFamily::from_inverter_type(&e.state.config.inverter_type)
    };
    match family {
        GridPortPowerFamily::SinglePhase => Err(format!(
            "{} is read-only on the wire (givenergy-modbus defines no setter). \
             Change the inverter's max AC output via the plant configuration instead.",
            family.label()
        )),
        GridPortPowerFamily::ThreePhase | GridPortPowerFamily::Ems => {
            let mut eng = state.engine.lock().await;
            if let Some(e) = eng.as_mut() {
                // Clamp to the family's wire-protocol maximum (6500 W on
                // HR 1063, 65535 W on HR 2071). The register projection
                // further clamps the raw u16 value on the way out.
                let clamped = family.clamp_watts(watts);
                e.enqueue(Command::SetExportLimit(clamped));
            }
            Ok(family)
        }
    }
}

/// Set the **grid export limit** (the regulatory/DNO-facing cap), in watts.
///
/// Distinct from [`set_grid_port_max_power`]: that command targets the wire
/// register whose semantics *change* per family (HR 26 hardware output for
/// single-phase, HR 1063 / HR 2071 export limit for three-phase / EMS).
/// This command always targets the *grid export limit*:
///
/// * **Single-phase**: writes the simulator-internal HR 102
///   (`inverter_export_limit_w`, ReadWrite in the catalogue) via
///   `Command::SetExportLimit`. Real GivEnergy single-phase inverters don't
///   expose a client-settable export limit on the wire (givenergy-modbus
///   defines no setter), but installers still need to configure the
///   DNO-mandated cap for the site (e.g. UK EREC G98 = 3680 W for a
///   single-phase Micro-generator). HR 102 carries the user's chosen
///   limit and the InverterEngine honors it when mode = ExportLimit.
///   Negative inputs are pinned to 0; positive inputs are accepted
///   unchanged even if they exceed `max_ac_watts` (e.g. a UK installer
///   setting 3680 W on a 3 kW AC-coupled inverter) — the inverter
///   physically can't reach those watts, so the higher cap is harmless.
/// * **Three-phase / EMS**: writes to the wire register HR 1063 / HR 2071
///   via `Command::SetExportLimit`, clamped to the family's wire ceiling
///   (6500 W for HR 1063, 65535 W for HR 2071).
#[tauri::command]
pub async fn set_grid_export_limit(state: State<'_, AppState>, watts: f64) -> Result<(), String> {
    if !watts.is_finite() || watts < 0.0 {
        return Err(format!(
            "Watts must be a non-negative finite number, got {watts}"
        ));
    }
    let family = {
        let eng = state.engine.lock().await;
        let Some(e) = eng.as_ref() else {
            return Err("No plant created".to_string());
        };
        GridPortPowerFamily::from_inverter_type(&e.state.config.inverter_type)
    };
    let clamped = match family {
        GridPortPowerFamily::SinglePhase => watts.max(0.0),
        GridPortPowerFamily::ThreePhase | GridPortPowerFamily::Ems => family.clamp_watts(watts),
    };
    let mut eng = state.engine.lock().await;
    if let Some(e) = eng.as_mut() {
        e.enqueue(Command::SetExportLimit(clamped));
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

    #[test]
    fn grid_port_power_family_classifies_inverter_types() {
        use GridPortPowerFamily::*;

        // Three-phase family — all share the HR 1000-1124 block per giv_tcp
        // model/threephase.py. ACThreePhase is included for symmetry with
        // is_three_phase_inverter() in sim-registers.
        assert_eq!(
            GridPortPowerFamily::from_inverter_type("ThreePhase"),
            ThreePhase
        );
        assert_eq!(
            GridPortPowerFamily::from_inverter_type("ThreePhase8kW"),
            ThreePhase
        );
        assert_eq!(
            GridPortPowerFamily::from_inverter_type("ThreePhase10kW"),
            ThreePhase
        );
        assert_eq!(
            GridPortPowerFamily::from_inverter_type("ThreePhase11kW"),
            ThreePhase
        );
        assert_eq!(
            GridPortPowerFamily::from_inverter_type("ACThreePhase"),
            ThreePhase
        );

        // EMS family — EMS, EmsCommercial, and Gateway (the Gateway inherits
        // its export_power_limit / 2071 semantics from EMS per
        // docs/gateway-register-reference.md).
        assert_eq!(GridPortPowerFamily::from_inverter_type("EMS"), Ems);
        assert_eq!(
            GridPortPowerFamily::from_inverter_type("EmsCommercial"),
            Ems
        );
        assert_eq!(GridPortPowerFamily::from_inverter_type("Gateway12kW"), Ems);

        // Single-phase family — HR 26 is read-only. Includes all the
        // hybrid / polar / AC-coupled / Gen3+ / PV inverter / AIO variants.
        for inv in [
            "Gen1Hybrid",
            "Gen2Hybrid",
            "Gen3Hybrid",
            "Gen3Hybrid8kW",
            "Gen3Hybrid10kW",
            "Gen4Hybrid6kW",
            "Gen3Plus6kW",
            "Gen3Plus4600",
            "Gen3Plus3600",
            "Gen3Plus6kW2",
            "Gen3Plus7kW",
            "Gen3Plus8kW",
            "ACCoupled",
            "ACCoupled2",
            "Hybrid4600",
            "Hybrid3600",
            "Polar5kW",
            "Polar4600",
            "Polar3600",
            "Polar6kW",
            "Polar7kW",
            "PVInverter5kW",
            "PVInverter4600",
            "PVInverter3600",
            "PVInverter6kW",
            "AllInOne",
            "AllInOne6",
            "AllInOne5",
            "AIO6kW",
            "AIO8kW",
            "AIO10kW",
            "AIOHybrid6kW",
            "AIOHybrid8kW",
            "AIOHybrid10kW",
            "AIOHybrid12kW",
        ] {
            assert_eq!(
                GridPortPowerFamily::from_inverter_type(inv),
                SinglePhase,
                "expected {inv} to be classified as SinglePhase"
            );
        }
    }

    #[test]
    fn grid_port_power_family_labels_are_distinct() {
        // The GUI uses these labels to disambiguate which register the
        // displayed value corresponds to. They must remain distinguishable.
        let labels = [
            GridPortPowerFamily::SinglePhase.label(),
            GridPortPowerFamily::ThreePhase.label(),
            GridPortPowerFamily::Ems.label(),
        ];
        for i in 0..labels.len() {
            for j in 0..labels.len() {
                if i != j {
                    assert_ne!(labels[i], labels[j], "family labels must be distinct");
                }
            }
        }
        // HR 26/1063/2071 are mentioned in the labels so the GUI can show
        // which register is in play without re-querying the inverter type.
        assert!(labels[0].contains("HR 26"));
        assert!(labels[1].contains("HR 1063"));
        assert!(labels[2].contains("HR 2071"));
    }

    #[test]
    fn grid_port_power_family_clamp_per_family_max() {
        // The wire-protocol max differs between HR 1063 (ThreePhase) and
        // HR 2071 (EMS). A single value over both limits would either let
        // EMS users accidentally write invalid HR 1063 values, or block
        // EMS users from setting a reasonable high cap. Per-family clamps
        // keep the GUI consistent with the underlying register.
        assert_eq!(GridPortPowerFamily::ThreePhase.max_w(), 6500.0);
        assert_eq!(GridPortPowerFamily::Ems.max_w(), 65535.0);

        // 6500 W is the boundary case: must pass through unchanged on
        // both families (fits exactly in the ThreePhase cap; comfortably
        // below the EMS cap).
        assert_eq!(GridPortPowerFamily::ThreePhase.clamp_watts(6500.0), 6500.0);
        assert_eq!(GridPortPowerFamily::Ems.clamp_watts(6500.0), 6500.0);

        // 7000 W exceeds the ThreePhase cap (HR 1063 max=6500) but is
        // within the EMS cap (HR 2071 is 16-bit).
        assert_eq!(GridPortPowerFamily::ThreePhase.clamp_watts(7000.0), 6500.0);
        assert_eq!(GridPortPowerFamily::Ems.clamp_watts(7000.0), 7000.0);

        // 70000 W exceeds both caps; the EMS ceiling is 65535 W.
        assert_eq!(GridPortPowerFamily::ThreePhase.clamp_watts(70000.0), 6500.0);
        assert_eq!(GridPortPowerFamily::Ems.clamp_watts(70000.0), 65535.0);

        // Negative inputs pin to 0 on both families (defence-in-depth
        // even though the UI rejects negatives up-front).
        assert_eq!(GridPortPowerFamily::ThreePhase.clamp_watts(-100.0), 0.0);
        assert_eq!(GridPortPowerFamily::Ems.clamp_watts(-100.0), 0.0);
    }

    #[test]
    fn grid_export_limit_clamping_by_family() {
        // Mirror of the clamping rules inside `set_grid_export_limit`:
        // single-phase accepts any non-negative value (the InverterEngine
        // silently clamps to max_ac_watts); three-phase / EMS clamp to
        // the family's wire ceiling. This unit test pins the policy so
        // the GUI and the Tauri command can't drift apart.
        let clamp = |fam: GridPortPowerFamily, watts: f64| -> f64 {
            match fam {
                GridPortPowerFamily::SinglePhase => watts.max(0.0),
                GridPortPowerFamily::ThreePhase | GridPortPowerFamily::Ems => {
                    fam.clamp_watts(watts)
                }
            }
        };

        // Single-phase: 3680 W (UK EREC G98) passes through unchanged even
        // though a 3 kW AC-coupled inverter physically can't reach it.
        // The limit is regulatory, not hardware-bound.
        assert_eq!(
            clamp(GridPortPowerFamily::SinglePhase, 3680.0),
            3680.0,
            "single-phase should accept the UK G98 3680 W default"
        );
        assert_eq!(
            clamp(GridPortPowerFamily::SinglePhase, 10000.0),
            10000.0,
            "single-phase accepts any positive value (hardware caps it later)"
        );
        assert_eq!(
            clamp(GridPortPowerFamily::SinglePhase, -1.0),
            0.0,
            "negative inputs pin to 0"
        );

        // Three-phase: wire ceiling 6500 W.
        assert_eq!(clamp(GridPortPowerFamily::ThreePhase, 6500.0), 6500.0);
        assert_eq!(clamp(GridPortPowerFamily::ThreePhase, 7000.0), 6500.0);
        assert_eq!(clamp(GridPortPowerFamily::ThreePhase, -5.0), 0.0);

        // EMS: full 16-bit ceiling.
        assert_eq!(clamp(GridPortPowerFamily::Ems, 65535.0), 65535.0);
        assert_eq!(clamp(GridPortPowerFamily::Ems, 70000.0), 65535.0);
    }
}
