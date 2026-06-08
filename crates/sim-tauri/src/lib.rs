//! Tauri application: GUI bridge between web frontend and simulation core.
//!
//! Implements IPC contracts from `docs/05-tauri-ipc-contracts.md`.
//! Also starts a Modbus TCP server on port 8899 (real GivEnergy port).

mod app_state;
mod commands;

use app_state::AppState;
use sim_modbus::ModbusCommand;
use std::sync::Arc;
use tauri::{Emitter, Manager};
const MODBUS_PORT: u16 = 8899;

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let reg_cat = sim_registers::default_register_catalogue();
    let register_store = Arc::new(tokio::sync::Mutex::new(sim_registers::RegisterStore::new(
        reg_cat,
    )));
    let modbus_cmds: Arc<std::sync::Mutex<Vec<ModbusCommand>>> =
        Arc::new(std::sync::Mutex::new(Vec::new()));
    let battery_snapshot = Arc::new(tokio::sync::Mutex::new(Vec::new()));
    let pending_time_regs = Arc::new(std::sync::Mutex::new([None; 6]));
    let evc_state = Arc::new(tokio::sync::Mutex::new(sim_models::EvcState::default()));

    let app_state = AppState {
        engine: Arc::new(tokio::sync::Mutex::new(None)),
        register_store: register_store.clone(),
        recording: Arc::new(tokio::sync::Mutex::new(Vec::new())),
        running: Arc::new(tokio::sync::Mutex::new(false)),
        schedule: Arc::new(tokio::sync::Mutex::new(None)),
        modbus_cmds: modbus_cmds.clone(),
        battery_snapshot: battery_snapshot.clone(),
        pending_time_regs: pending_time_regs.clone(),
        evc_state: evc_state.clone(),
    };

    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .manage(app_state)
        .invoke_handler(tauri::generate_handler![
            commands::create_plant,
            commands::load_scenario,
            commands::start_simulation,
            commands::pause_simulation,
            commands::inject_fault,
            commands::clear_fault,
            commands::set_mode,
            commands::set_weather,
            commands::set_solar_override,
            commands::set_load_override,
            commands::set_battery_soc,
            commands::set_battery_soh,
            commands::export_recording,
            commands::get_current_state,
            commands::save_plant,
            commands::has_saved_plant,
            commands::load_plant,
            commands::start_calibration,
            commands::cancel_calibration,
            commands::set_tick_interval,
            commands::export_config,
            commands::set_evc_enabled,
            commands::set_evc_charge_control,
            commands::set_evc_charge_current,
            commands::set_evc_charging_mode,
            commands::set_evc_cable_status,
            commands::get_evc_state,
            commands::set_dsp_firmware,
            commands::set_arm_firmware,
        ])
        .setup(move |app| {
            // Try to auto-load saved plant state + schedule
            let app_handle = app.handle().clone();
            if let Ok(data_dir) = app_handle.path().app_data_dir() {
                let save_path = data_dir.join("plant_state.json");
                if save_path.exists() {
                    tauri::async_runtime::spawn(async move {
                        if let Ok(json) = tokio::fs::read_to_string(&save_path).await {
                            // Try PersistedState first, fall back to plain PlantState
                            let (mut plant_state, schedule_opt) = if let Ok(ps) =
                                serde_json::from_str::<crate::app_state::PersistedState>(&json)
                            {
                                (ps.plant, ps.schedule)
                            } else if let Ok(ps) =
                                serde_json::from_str::<sim_models::PlantState>(&json)
                            {
                                (ps, None)
                            } else {
                                return;
                            };
                            plant_state.energy_totals.seed_for_testing_if_zero();

                            let app_state = app_handle.state::<AppState>();
                            let peak_watts = plant_state.config.solar_peak_watts;
                            let latitude = plant_state.config.latitude;
                            let tick_interval = plant_state.config.tick_interval_secs;

                            // Restore schedule
                            {
                                let mut sched = app_state.schedule.lock().await;
                                *sched = schedule_opt.clone();
                            }

                            let devices: Vec<Box<dyn sim_models::DeviceModel>> =
                                if let Some(ref sched) = schedule_opt {
                                    vec![
                                        Box::new(sim_core::ScheduleEngine::new(sched.clone())),
                                        Box::new(sim_core::SolarEngine::new(peak_watts, latitude)),
                                        Box::new(sim_core::LoadEngine::new(
                                            sim_core::LoadProfile::Family,
                                        )),
                                        Box::new(sim_core::InverterEngine::new()),
                                        Box::new(sim_faults::FaultEngine::new()),
                                        Box::new(sim_core::BatteryEngine::new()),
                                        Box::new(sim_core::EvcEngine::new()),
                                        Box::new(sim_core::EnergyTracker::new()),
                                    ]
                                } else {
                                    vec![
                                        Box::new(sim_core::SolarEngine::new(peak_watts, latitude)),
                                        Box::new(sim_core::LoadEngine::new(
                                            sim_core::LoadProfile::Family,
                                        )),
                                        Box::new(sim_core::InverterEngine::new()),
                                        Box::new(sim_faults::FaultEngine::new()),
                                        Box::new(sim_core::BatteryEngine::new()),
                                        Box::new(sim_core::EvcEngine::new()),
                                        Box::new(sim_core::EnergyTracker::new()),
                                    ]
                                };

                            let engine = sim_core::SimulationEngine::new(
                                plant_state,
                                devices,
                                tick_interval,
                            );
                            // Populate register store so Modbus clients see
                            // non-zero values before the first tick.
                            {
                                let mut rs = app_state.register_store.lock().await;
                                rs.project_from_state(&engine.state);
                            }
                            let dto = crate::app_state::PlantStateDto::with_schedule(
                                &engine.state,
                                schedule_opt.as_ref(),
                            );
                            {
                                let mut eng = app_state.engine.lock().await;
                                *eng = Some(engine);
                            }
                            let _ = app_handle.emit("state_changed", &dto);
                            tracing::info!("Auto-loaded saved plant from {}", save_path.display());
                        }
                    });
                }
            }

            // Start Modbus bridge & server inside Tauri's async runtime
            let modbus_cmds_for_task = modbus_cmds.clone();
            let (mb_tx, mut mb_rx) = tokio::sync::mpsc::unbounded_channel();

            tauri::async_runtime::spawn(async move {
                while let Some(cmd) = mb_rx.recv().await {
                    if let Ok(mut buf) = modbus_cmds_for_task.lock() {
                        buf.push(cmd);
                    }
                }
            });

            let modbus_store = register_store;
            let modbus_tx = mb_tx;
            let modbus_batteries = battery_snapshot.clone();
            let evc = evc_state.clone();
            tauri::async_runtime::spawn(async move {
                let addr: std::net::SocketAddr = format!("0.0.0.0:{MODBUS_PORT}")
                    .parse()
                    .expect("invalid Modbus addr");
                tracing::info!("Modbus TCP server listening on {addr}");
                // CT meter slaves are determined at runtime from the DTC in
                // the register store, so this adapts to inverter type changes.
                if let Err(e) =
                    sim_modbus::run_modbus_server(addr, modbus_store, modbus_tx, modbus_batteries)
                        .await
                {
                    tracing::error!("Modbus server error: {e}");
                }
            });
            // Start EVC standard Modbus TCP server
            tauri::async_runtime::spawn(async move {
                if let Err(e) = sim_modbus::run_evc_modbus_server(evc).await {
                    tracing::error!("EVC Modbus server error: {e}");
                }
            });

            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
