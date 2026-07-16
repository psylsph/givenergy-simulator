//! Shared application state for Tauri commands.

use sim_core::{PlantState, Schedule};
use sim_recording::RecordingFrame;
use sim_registers::RegisterStore;
use std::sync::Arc;
use tokio::sync::Mutex;

/// Wrapper for what gets persisted to disk between sessions.
#[derive(serde::Serialize, serde::Deserialize)]
pub struct PersistedState {
    pub plant: PlantState,
    pub schedule: Option<Schedule>,
}

/// State shared between Tauri commands.
pub struct AppState {
    /// The simulation engine (None before create_plant).
    pub engine: Arc<Mutex<Option<sim_core::SimulationEngine>>>,
    /// Register store for Modbus/projection.
    pub register_store: Arc<Mutex<RegisterStore>>,
    /// Recording frames.
    pub recording: Arc<Mutex<Vec<RecordingFrame>>>,
    /// Whether the simulation is running.
    pub running: Arc<Mutex<bool>>,
    /// Current schedule (if any).
    pub schedule: Arc<Mutex<Option<Schedule>>>,
    /// Buffer for Modbus write commands, drained each tick.
    pub modbus_cmds: Arc<std::sync::Mutex<Vec<sim_modbus::ModbusCommand>>>,
    /// Snapshot of battery state for Modbus BMS reads.
    pub battery_snapshot: Arc<tokio::sync::Mutex<Vec<sim_models::BatteryState>>>,
    /// Accumulated time register writes from Modbus (HR 35-40), persists across drain cycles.
    pub pending_time_regs: Arc<std::sync::Mutex<[Option<u16>; 6]>>,
    /// Scratch buffer used while reconciling independent HR 318-320 writes.
    /// Missing fields are filled from live state so each write can take effect
    /// immediately without clobbering the rest of the pause configuration.
    pub pending_pause_regs: Arc<std::sync::Mutex<[Option<u16>; 3]>>,
    /// EVC (Electric Vehicle Charger) state, shared with standard Modbus TCP server.
    pub evc_state: Arc<tokio::sync::Mutex<sim_models::EvcState>>,
    /// EVC Modbus TCP port (default 5020).
    pub evc_port: Arc<std::sync::Mutex<u16>>,
    /// Dongle misbehaviour simulation mode.
    pub dongle_misbehaviour: Arc<std::sync::Mutex<sim_models::DongleMisbehaviourMode>>,
}

impl Default for AppState {
    fn default() -> Self {
        let reg_cat = sim_registers::default_register_catalogue();
        let (_modbus_cmd_tx, _) =
            tokio::sync::mpsc::unbounded_channel::<sim_modbus::ModbusCommand>();
        Self {
            engine: Arc::new(Mutex::new(None)),
            register_store: Arc::new(Mutex::new(RegisterStore::new(reg_cat))),
            recording: Arc::new(Mutex::new(Vec::new())),
            running: Arc::new(Mutex::new(false)),
            schedule: Arc::new(Mutex::new(None)),
            modbus_cmds: Arc::new(std::sync::Mutex::new(Vec::new())),
            battery_snapshot: Arc::new(tokio::sync::Mutex::new(Vec::new())),
            pending_time_regs: Arc::new(std::sync::Mutex::new([None; 6])),
            pending_pause_regs: Arc::new(std::sync::Mutex::new([None; 3])),
            evc_state: Arc::new(tokio::sync::Mutex::new(sim_models::EvcState::default())),
            evc_port: Arc::new(std::sync::Mutex::new(5020)),
            dongle_misbehaviour: Arc::new(std::sync::Mutex::new(
                sim_models::DongleMisbehaviourMode::Off,
            )),
        }
    }
}

// ---------------------------------------------------------------------------

#[derive(serde::Serialize, Clone)]
pub struct BatteryModuleDto {
    pub soc_percent: f64,
    pub power_kw: f64,
    pub voltage_v: f64,
    pub current_a: f64,
    pub temperature_celsius: f64,
    pub capacity_kwh: f64,
    pub nominal_capacity_kwh: f64,
    pub soh: f64,
    pub cycle_count: f64,
}

impl From<&sim_models::BatteryState> for BatteryModuleDto {
    fn from(b: &sim_models::BatteryState) -> Self {
        Self {
            soc_percent: b.soc_percent,
            power_kw: b.power_kw,
            voltage_v: b.voltage_v,
            current_a: b.current_a,
            temperature_celsius: b.temperature_celsius,
            capacity_kwh: b.capacity_kwh,
            nominal_capacity_kwh: b.nominal_capacity_kwh,
            soh: b.soh,
            cycle_count: b.cycle_count,
        }
    }
}

#[derive(serde::Serialize, Clone)]
pub struct PlantStateDto {
    pub timestamp: String,
    pub inverter_mode: String,
    pub battery_mode: String,
    pub inverter_type: String,
    pub inverter_ac_power_w: f64,
    pub inverter_max_output_w: f64,
    /// Live inverter temperature (°C) — IR 41 (`ge_ir_inverter_temperature`).
    pub inverter_temperature_celsius: f64,
    /// Manual inverter temperature override (°C). `Some` = thermal model
    /// pinned; `None` = thermal model active. Surfaced so the GUI can show
    /// the override state and offer a Clear control.
    pub inverter_temperature_override: Option<f64>,
    /// Live export power limit (W). Drives HR 1063 (`p_export_limit`, three-
    /// phase / HV / AIO, ×0.1 dW encoding) and HR 2071 (`ems_export_power_limit`,
    /// EMS / EmsCommercial / Gateway, raw watts). For single-phase / AC-coupled
    /// / Gen1-4 the wire register HR 26 is read-only and mirrors
    /// `inverter_max_output_w` instead — the GUI uses the family classifier
    /// (see `sim_tauri::commands::GridPortPowerFamily`) to pick which field to
    /// display in the "Grid Port Max Power Output" sidebar.
    pub export_limit_w: f64,
    pub charge_power_limit_percent: f64,
    pub discharge_power_limit_percent: f64,
    /// ARM firmware (HR 21) — reported as projected to the Modbus server.
    pub arm_firmware_version: u16,
    /// DSP firmware (HR 19) — user-overridable.
    pub dsp_firmware_version: u16,
    pub aggregate_soc: f64,
    pub battery_power_kw: f64,
    pub battery_temperature_celsius: f64,
    pub battery_module_count: usize,
    pub battery_modules: Vec<BatteryModuleDto>,
    pub solar_generation_w: f64,
    pub solar_pv1_w: f64,
    pub solar_pv2_w: f64,
    pub pv2_peak_watts: f64,
    pub solar_override: Option<f64>,
    pub load_demand_w: f64,
    pub load_override: Option<f64>,
    pub grid_power_w: f64,
    pub grid_connected: bool,
    pub active_faults: Vec<String>,
    pub weather: String,
    pub energy_totals: EnergyTotalsDto,
    pub schedule: ScheduleDto,
    pub evc: sim_models::EvcState,
    pub ct_meter_installed: bool,
    pub battery_self_heating: bool,
    pub manual_battery_heater: bool,
    pub enable_eps: bool,
    /// Number of parallel AIO units behind a Gateway (1-3, 0 for non-gateway).
    pub parallel_aio_num: u16,
    /// Current dongle misbehaviour simulation mode.
    pub dongle_misbehaviour: String,
}

#[derive(serde::Serialize, Clone)]
pub struct ScheduleDto {
    pub enable_discharge: bool,
    pub enable_charge: bool,
    pub soc_reserve: f64,
    pub charge_target_soc: f64,
    pub charge_target_soc_2: f64,
    pub discharge_target_soc: f64,
    pub discharge_target_soc_2: f64,
    pub charge_slot_1_start: u16,
    pub charge_slot_1_end: u16,
    pub charge_slot_2_start: u16,
    pub charge_slot_2_end: u16,
    pub charge_slot_3_start: u16,
    pub charge_slot_3_end: u16,
    pub charge_target_soc_3: f64,
    pub charge_slot_4_start: u16,
    pub charge_slot_4_end: u16,
    pub charge_target_soc_4: f64,
    pub charge_slot_5_start: u16,
    pub charge_slot_5_end: u16,
    pub charge_target_soc_5: f64,
    pub charge_slot_6_start: u16,
    pub charge_slot_6_end: u16,
    pub charge_target_soc_6: f64,
    pub charge_slot_7_start: u16,
    pub charge_slot_7_end: u16,
    pub charge_target_soc_7: f64,
    pub charge_slot_8_start: u16,
    pub charge_slot_8_end: u16,
    pub charge_target_soc_8: f64,
    pub charge_slot_9_start: u16,
    pub charge_slot_9_end: u16,
    pub charge_target_soc_9: f64,
    pub charge_slot_10_start: u16,
    pub charge_slot_10_end: u16,
    pub charge_target_soc_10: f64,
    pub discharge_slot_1_start: u16,
    pub discharge_slot_1_end: u16,
    pub discharge_slot_2_start: u16,
    pub discharge_slot_2_end: u16,
    pub discharge_slot_3_start: u16,
    pub discharge_slot_3_end: u16,
    pub discharge_target_soc_3: f64,
    pub discharge_slot_4_start: u16,
    pub discharge_slot_4_end: u16,
    pub discharge_target_soc_4: f64,
    pub discharge_slot_5_start: u16,
    pub discharge_slot_5_end: u16,
    pub discharge_target_soc_5: f64,
    pub discharge_slot_6_start: u16,
    pub discharge_slot_6_end: u16,
    pub discharge_target_soc_6: f64,
    pub discharge_slot_7_start: u16,
    pub discharge_slot_7_end: u16,
    pub discharge_target_soc_7: f64,
    pub discharge_slot_8_start: u16,
    pub discharge_slot_8_end: u16,
    pub discharge_target_soc_8: f64,
    pub discharge_slot_9_start: u16,
    pub discharge_slot_9_end: u16,
    pub discharge_target_soc_9: f64,
    pub discharge_slot_10_start: u16,
    pub discharge_slot_10_end: u16,
    pub discharge_target_soc_10: f64,
    pub battery_pause_mode: u16,
    pub pause_slot_start: u16,
    pub pause_slot_end: u16,
}

impl ScheduleDto {
    fn from_state(state: &PlantState, schedule: Option<&sim_core::Schedule>) -> Self {
        // Convert decimal hours (e.g. 5.5) to HHMM (e.g. 530).
        // 0 or negative = disabled sentinel 60 (matches register projector).
        // Convert decimal hours to HHMM. 0.0 → 0 (valid 00:00 midnight).
        // Only used for slots that are confirmed active (start != end).
        let hhmm = |decimal_hours: f64| -> u16 {
            if decimal_hours < 0.0 {
                return 60;
            }
            let h = decimal_hours.floor() as u16;
            let m = ((decimal_hours - h as f64) * 60.0).round() as u16;
            if m > 59 || h > 23 {
                return 60;
            }
            h * 100 + m
        };
        let raw_or = |address: u16, derived: u16| {
            schedule.map_or(derived, |s| s.raw_time_or(address, derived))
        };

        let (
            cs,
            ce,
            ds,
            de,
            cs2,
            ce2,
            ds2,
            de2,
            ct1,
            ct2,
            dt1,
            dt2,
            cs3,
            ce3,
            ct3,
            cs4,
            ce4,
            ct4,
            cs5,
            ce5,
            ct5,
            cs6,
            ce6,
            ct6,
            cs7,
            ce7,
            ct7,
            cs8,
            ce8,
            ct8,
            cs9,
            ce9,
            ct9,
            cs10,
            ce10,
            ct10,
            ds3,
            de3,
            dt3,
            ds4,
            de4,
            dt4,
            ds5,
            de5,
            dt5,
            ds6,
            de6,
            dt6,
            ds7,
            de7,
            dt7,
            ds8,
            de8,
            dt8,
            ds9,
            de9,
            dt9,
            ds10,
            de10,
            dt10,
        ) = match schedule {
            Some(s) => (
                s.charge_start,
                s.charge_end,
                s.discharge_start,
                s.discharge_end,
                s.charge_start_2,
                s.charge_end_2,
                s.discharge_start_2,
                s.discharge_end_2,
                s.charge_target_soc,
                s.charge_target_soc_2,
                s.discharge_target_soc,
                s.discharge_target_soc_2,
                s.charge_start_3,
                s.charge_end_3,
                s.charge_target_soc_3,
                s.charge_start_4,
                s.charge_end_4,
                s.charge_target_soc_4,
                s.charge_start_5,
                s.charge_end_5,
                s.charge_target_soc_5,
                s.charge_start_6,
                s.charge_end_6,
                s.charge_target_soc_6,
                s.charge_start_7,
                s.charge_end_7,
                s.charge_target_soc_7,
                s.charge_start_8,
                s.charge_end_8,
                s.charge_target_soc_8,
                s.charge_start_9,
                s.charge_end_9,
                s.charge_target_soc_9,
                s.charge_start_10,
                s.charge_end_10,
                s.charge_target_soc_10,
                s.discharge_start_3,
                s.discharge_end_3,
                s.discharge_target_soc_3,
                s.discharge_start_4,
                s.discharge_end_4,
                s.discharge_target_soc_4,
                s.discharge_start_5,
                s.discharge_end_5,
                s.discharge_target_soc_5,
                s.discharge_start_6,
                s.discharge_end_6,
                s.discharge_target_soc_6,
                s.discharge_start_7,
                s.discharge_end_7,
                s.discharge_target_soc_7,
                s.discharge_start_8,
                s.discharge_end_8,
                s.discharge_target_soc_8,
                s.discharge_start_9,
                s.discharge_end_9,
                s.discharge_target_soc_9,
                s.discharge_start_10,
                s.discharge_end_10,
                s.discharge_target_soc_10,
            ),
            None => (
                0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 100.0, 100.0, 10.0, 10.0, 0.0, 0.0, 100.0,
                0.0, 0.0, 100.0, 0.0, 0.0, 100.0, 0.0, 0.0, 100.0, 0.0, 0.0, 100.0, 0.0, 0.0,
                100.0, 0.0, 0.0, 100.0, 0.0, 0.0, 100.0, 0.0, 0.0, 10.0, 0.0, 0.0, 10.0, 0.0, 0.0,
                10.0, 0.0, 0.0, 10.0, 0.0, 0.0, 10.0, 0.0, 0.0, 10.0, 0.0, 0.0, 10.0, 0.0, 0.0,
                10.0,
            ),
        };

        Self {
            // HR 96/59 are independent controls. Do not infer them from
            // configured windows: disabled schedules retain their slot values.
            enable_charge: schedule.is_some_and(|s| s.enable_charge),
            enable_discharge: schedule.is_some_and(|s| s.enable_discharge),
            soc_reserve: state.min_aggregate_soc(),
            charge_target_soc: ct1,
            charge_target_soc_2: ct2,
            charge_target_soc_3: ct3,
            charge_target_soc_4: ct4,
            charge_target_soc_5: ct5,
            charge_target_soc_6: ct6,
            charge_target_soc_7: ct7,
            charge_target_soc_8: ct8,
            charge_target_soc_9: ct9,
            charge_target_soc_10: ct10,
            discharge_target_soc: dt1,
            discharge_target_soc_2: dt2,
            discharge_target_soc_3: dt3,
            discharge_target_soc_4: dt4,
            discharge_target_soc_5: dt5,
            discharge_target_soc_6: dt6,
            discharge_target_soc_7: dt7,
            discharge_target_soc_8: dt8,
            discharge_target_soc_9: dt9,
            discharge_target_soc_10: dt10,
            charge_slot_1_start: raw_or(94, hhmm(cs)),
            charge_slot_1_end: raw_or(95, hhmm(ce)),
            charge_slot_2_start: raw_or(243, raw_or(31, hhmm(cs2))),
            charge_slot_2_end: raw_or(244, raw_or(32, hhmm(ce2))),
            charge_slot_3_start: raw_or(246, hhmm(cs3)),
            charge_slot_3_end: raw_or(247, hhmm(ce3)),
            charge_slot_4_start: raw_or(249, hhmm(cs4)),
            charge_slot_4_end: raw_or(250, hhmm(ce4)),
            charge_slot_5_start: raw_or(252, hhmm(cs5)),
            charge_slot_5_end: raw_or(253, hhmm(ce5)),
            charge_slot_6_start: raw_or(255, hhmm(cs6)),
            charge_slot_6_end: raw_or(256, hhmm(ce6)),
            charge_slot_7_start: raw_or(258, hhmm(cs7)),
            charge_slot_7_end: raw_or(259, hhmm(ce7)),
            charge_slot_8_start: raw_or(261, hhmm(cs8)),
            charge_slot_8_end: raw_or(262, hhmm(ce8)),
            charge_slot_9_start: raw_or(264, hhmm(cs9)),
            charge_slot_9_end: raw_or(265, hhmm(ce9)),
            charge_slot_10_start: raw_or(267, hhmm(cs10)),
            charge_slot_10_end: raw_or(268, hhmm(ce10)),
            discharge_slot_1_start: raw_or(56, hhmm(ds)),
            discharge_slot_1_end: raw_or(57, hhmm(de)),
            discharge_slot_2_start: raw_or(44, hhmm(ds2)),
            discharge_slot_2_end: raw_or(45, hhmm(de2)),
            discharge_slot_3_start: raw_or(276, hhmm(ds3)),
            discharge_slot_3_end: raw_or(277, hhmm(de3)),
            discharge_slot_4_start: raw_or(279, hhmm(ds4)),
            discharge_slot_4_end: raw_or(280, hhmm(de4)),
            discharge_slot_5_start: raw_or(282, hhmm(ds5)),
            discharge_slot_5_end: raw_or(283, hhmm(de5)),
            discharge_slot_6_start: raw_or(285, hhmm(ds6)),
            discharge_slot_6_end: raw_or(286, hhmm(de6)),
            discharge_slot_7_start: raw_or(288, hhmm(ds7)),
            discharge_slot_7_end: raw_or(289, hhmm(de7)),
            discharge_slot_8_start: raw_or(291, hhmm(ds8)),
            discharge_slot_8_end: raw_or(292, hhmm(de8)),
            discharge_slot_9_start: raw_or(294, hhmm(ds9)),
            discharge_slot_9_end: raw_or(295, hhmm(de9)),
            discharge_slot_10_start: raw_or(297, hhmm(ds10)),
            discharge_slot_10_end: raw_or(298, hhmm(de10)),
            battery_pause_mode: state.battery_pause_mode,
            pause_slot_start: state.battery_pause_slot_start,
            pause_slot_end: state.battery_pause_slot_end,
        }
    }
}

impl PlantStateDto {
    /// Build a DTO with schedule data from AppState.
    pub fn with_schedule(state: &PlantState, schedule: Option<&sim_core::Schedule>) -> Self {
        let mut dto = Self::from(state);
        dto.schedule = ScheduleDto::from_state(state, schedule);
        dto
    }
}

#[derive(serde::Serialize, Clone)]
pub struct EnergyTotalsDto {
    pub grid_import_kwh: f64,
    pub grid_export_kwh: f64,
    pub battery_charge_kwh: f64,
    pub battery_discharge_kwh: f64,
    pub solar_generation_kwh: f64,
    pub load_consumption_kwh: f64,
}

impl From<&PlantState> for PlantStateDto {
    fn from(state: &PlantState) -> Self {
        Self {
            timestamp: state.timestamp.format("%Y-%m-%dT%H:%M:%S").to_string(),
            inverter_mode: format!("{:?}", state.inverter.mode_state.effective),
            battery_mode: {
                let scheduled_charge = state.scheduled_charge;
                let scheduled_discharge = state.scheduled_discharge;
                let eco = state.inverter.mode_state.effective == sim_models::InverterMode::Eco;
                let force_charge =
                    state.inverter.mode_state.effective == sim_models::InverterMode::ForceCharge;
                let enable_discharge =
                    state.inverter.mode_state.effective == sim_models::InverterMode::ForceDischarge;
                let soc_reserve = state.min_aggregate_soc();
                match (
                    scheduled_charge,
                    scheduled_discharge,
                    eco,
                    force_charge,
                    enable_discharge,
                    (soc_reserve.round() as u16) == 100,
                ) {
                    (true, _, _, _, _, _) => "ScheduledCharge",
                    (_, true, _, _, _, _) => "ScheduledDischarge",
                    (_, _, true, false, false, false) => "Eco",
                    (_, _, true, false, false, true) => "EcoPaused",
                    (_, _, true, false, true, _) => "TimedDemand",
                    (_, _, false, true, _, _) => "ForceCharge",
                    (_, _, false, false, true, _) => "TimedExport",
                    _ => "ExportPaused",
                }
                .to_string()
            },
            inverter_type: state.config.inverter_type.clone(),
            inverter_ac_power_w: state.inverter.ac_power_w,
            inverter_max_output_w: state.config.max_ac_watts,
            inverter_temperature_celsius: state.inverter.temperature_celsius,
            inverter_temperature_override: state.inverter.temperature_override,
            export_limit_w: state.inverter.export_limit_w,
            charge_power_limit_percent: if state.battery_charge_limit_percent <= 0.0 {
                100.0
            } else {
                state.battery_charge_limit_percent
            },
            discharge_power_limit_percent: if state.battery_discharge_limit_percent <= 0.0 {
                100.0
            } else {
                state.battery_discharge_limit_percent
            },
            arm_firmware_version: if state.inverter.arm_firmware_version != 0 {
                state.inverter.arm_firmware_version
            } else {
                match state.config.inverter_type.as_str() {
                    "Gen1Hybrid" => 252,
                    "Gen2Hybrid" => 852,
                    "Gen3Hybrid" => 318,
                    "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 452,
                    _ => 318,
                }
            },
            dsp_firmware_version: state.inverter.dsp_firmware_version,
            aggregate_soc: state.aggregate_soc(),
            battery_power_kw: state.total_battery_power_kw(),
            battery_temperature_celsius: state.battery_temperature_celsius(),
            battery_module_count: state.batteries.len(),
            battery_modules: state.batteries.iter().map(BatteryModuleDto::from).collect(),
            solar_generation_w: state.solar.generation_w,
            solar_pv1_w: state.solar.pv1_w,
            solar_pv2_w: state.solar.pv2_w,
            pv2_peak_watts: state.config.pv2_peak_watts,
            solar_override: state.solar_override,
            load_demand_w: state.load.demand_w,
            load_override: state.load_override,
            grid_power_w: state.grid.power_w,
            grid_connected: state.grid.connected,
            active_faults: state.active_faults.clone(),
            weather: state.weather.clone(),
            schedule: ScheduleDto::from_state(state, None),
            evc: state.evc.clone(),
            ct_meter_installed: state.config.ct_meter_installed,
            battery_self_heating: state.inverter.battery_self_heating,
            manual_battery_heater: state.inverter.manual_battery_heater,
            enable_eps: state.enable_eps,
            parallel_aio_num: state.config.parallel_aio_num,
            dongle_misbehaviour: format!("{:?}", state.dongle_misbehaviour),
            energy_totals: EnergyTotalsDto {
                grid_import_kwh: state.energy_totals.grid_import_kwh,
                grid_export_kwh: state.energy_totals.grid_export_kwh,
                battery_charge_kwh: state.energy_totals.battery_charge_kwh,
                battery_discharge_kwh: state.energy_totals.battery_discharge_kwh,
                solar_generation_kwh: state.energy_totals.solar_generation_kwh,
                load_consumption_kwh: state.energy_totals.load_consumption_kwh,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn ts() -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    #[test]
    fn dto_exposes_limits_and_inverter_max_output() {
        let mut state = PlantState::new(ts());
        state.config.max_ac_watts = 8000.0;
        state.battery_charge_limit_percent = 80.0;
        state.battery_discharge_limit_percent = 70.0;

        let dto = PlantStateDto::from(&state);

        assert_eq!(dto.inverter_max_output_w, 8000.0);
        assert_eq!(dto.charge_power_limit_percent, 80.0);
        assert_eq!(dto.discharge_power_limit_percent, 70.0);
    }

    #[test]
    fn dto_treats_zero_power_limits_as_100_percent_default() {
        let mut state = PlantState::new(ts());
        // Guards old persisted state or transient zero values from displaying as 0% limits.
        state.battery_charge_limit_percent = 0.0;
        state.battery_discharge_limit_percent = 0.0;

        let dto = PlantStateDto::from(&state);

        assert_eq!(dto.charge_power_limit_percent, 100.0);
        assert_eq!(dto.discharge_power_limit_percent, 100.0);
    }

    #[test]
    fn ac_coupled_schedule_dto_exposes_discharge_slot_1() {
        let mut state = PlantState::new(ts());
        state.config.inverter_type = "ACCoupled".to_string();
        let sched = sim_core::Schedule {
            discharge_start: 17.0,
            discharge_end: 21.0,
            discharge_target_soc: 25.0,
            enable_discharge: true,
            ..Default::default()
        };

        let dto = ScheduleDto::from_state(&state, Some(&sched));

        assert!(dto.enable_discharge);
        assert_eq!(dto.discharge_slot_1_start, 1700);
        assert_eq!(dto.discharge_slot_1_end, 2100);
        assert_eq!(dto.discharge_target_soc, 25.0);
    }

    #[test]
    fn schedule_dto_keeps_disabled_flag_and_stored_raw_times_independent() {
        let state = PlantState::new(ts());
        let mut schedule = sim_core::Schedule {
            charge_start: 1.0,
            charge_end: 4.0,
            enable_charge: false,
            ..Default::default()
        };
        schedule.apply_modbus_updates(&[(94, 2360), (95, u16::MAX)].into());

        let dto = ScheduleDto::from_state(&state, Some(&schedule));

        assert!(!dto.enable_charge);
        assert_eq!(dto.charge_slot_1_start, 2360);
        assert_eq!(dto.charge_slot_1_end, u16::MAX);
    }

    #[test]
    fn schedule_dto_reflects_live_pause_slot_state() {
        // Regression: ScheduleDto previously hard-coded the HR 318-320 pause
        // slot to (mode=0, start=60, end=60), so Modbus writes to those
        // registers never surfaced in the GUI. The DTO must read them from
        // state so a client write is visible after the next refresh.
        let mut state = PlantState::new(ts());
        state.battery_pause_mode = 2;
        state.battery_pause_slot_start = 400; // 04:00
        state.battery_pause_slot_end = 300; // 03:00

        let dto = ScheduleDto::from_state(&state, None);

        assert_eq!(dto.battery_pause_mode, 2);
        assert_eq!(dto.pause_slot_start, 400);
        assert_eq!(dto.pause_slot_end, 300);
    }

    #[test]
    fn schedule_dto_pause_slot_defaults_when_unset() {
        // A freshly-constructed PlantState has the disabled sentinels
        // (mode=0, start=60, end=60); the DTO must echo those, not invent
        // values.
        let state = PlantState::new(ts());
        let dto = ScheduleDto::from_state(&state, None);

        assert_eq!(dto.battery_pause_mode, 0);
        assert_eq!(dto.pause_slot_start, 60);
        assert_eq!(dto.pause_slot_end, 60);
    }

    #[test]
    fn gateway_aio_battery_defaults() {
        // Verify that the Gateway AIO battery stack matches the info card:
        // 3 × GIV-BAT-3.4-HV (10.2 kWh, 51.2V nominal per module).
        let hv_capacity: f64 = 3.4;
        let count = 3usize;
        let max_batt_kw = 6000.0;
        let per_module_max_kw = max_batt_kw / count as f64;
        let batts: Vec<sim_models::BatteryState> = (0..count)
            .map(|_| {
                let c_rate_kw = (hv_capacity * 0.7).min(10.0);
                sim_models::BatteryState {
                    capacity_kwh: hv_capacity,
                    nominal_capacity_kwh: hv_capacity,
                    voltage_v: 51.2, // GIV-BAT-3.4-HV: 16S LFP @ 3.2V nominal
                    soh: 1.0,
                    max_charge_kw: c_rate_kw.min(per_module_max_kw),
                    max_discharge_kw: c_rate_kw.min(per_module_max_kw),
                    ..sim_models::BatteryState::default()
                }
            })
            .collect();

        assert_eq!(batts.len(), 3, "must create 3 HV modules");
        for (i, b) in batts.iter().enumerate() {
            assert_eq!(
                b.capacity_kwh, 3.4,
                "module {i}: capacity_kwh must be 3.4, got {}",
                b.capacity_kwh
            );
            assert_eq!(
                b.nominal_capacity_kwh, 3.4,
                "module {i}: nominal_capacity_kwh must be 3.4, got {}",
                b.nominal_capacity_kwh
            );
            assert_eq!(
                b.voltage_v, 51.2,
                "module {i}: voltage_v must be 51.2 (HV 16S), got {}",
                b.voltage_v
            );
            assert!(
                b.max_charge_kw > 0.0,
                "module {i}: max_charge_kw must be positive"
            );
        }

        // Total stack capacity = 3 × 3.4 = 10.2 kWh
        let total_cap: f64 = batts.iter().map(|b| b.capacity_kwh).sum();
        assert!(
            (total_cap - 10.2).abs() < 0.01,
            "total stack capacity must be 10.2 kWh, got {total_cap}"
        );

        // Simulate the full PlantState::new + overwrite flow that create_plant uses
        let now = chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut state = sim_models::PlantState::new(now);
        state.batteries = batts;
        state.sync_battery_from_vec();

        // Now verify the DTO that the UI would receive
        let dto = PlantStateDto::from(&state);
        assert_eq!(dto.battery_modules.len(), 3, "DTO must have 3 modules");
        assert_eq!(
            dto.battery_modules[0].capacity_kwh, 3.4,
            "DTO module 0 capacity must be 3.4, got {}",
            dto.battery_modules[0].capacity_kwh
        );
        assert_eq!(
            dto.battery_modules[0].nominal_capacity_kwh, 3.4,
            "DTO module 0 nominal capacity must be 3.4, got {}",
            dto.battery_modules[0].nominal_capacity_kwh
        );
        assert_eq!(
            dto.battery_modules[0].voltage_v, 51.2,
            "DTO module 0 voltage must be 51.2, got {}",
            dto.battery_modules[0].voltage_v
        );
        // Verify the UI display string: "3.4 / 3.4 kWh"
        let display = format!(
            "{:.1} / {:.1} kWh",
            dto.battery_modules[0].capacity_kwh, dto.battery_modules[0].nominal_capacity_kwh
        );
        assert_eq!(display, "3.4 / 3.4 kWh", "UI display must match");
    }
}
