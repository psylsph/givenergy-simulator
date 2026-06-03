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
}

#[derive(serde::Serialize, Clone)]
pub struct ScheduleDto {
    pub enable_discharge: bool,
    pub enable_charge: bool,
    pub soc_reserve: f64,
    pub charge_target_soc: f64,
    pub charge_slot_1_start: u16,
    pub charge_slot_1_end: u16,
    pub charge_slot_2_start: u16,
    pub charge_slot_2_end: u16,
    pub discharge_slot_1_start: u16,
    pub discharge_slot_1_end: u16,
    pub discharge_slot_2_start: u16,
    pub discharge_slot_2_end: u16,
    pub battery_pause_mode: u16,
    pub pause_slot_start: u16,
    pub pause_slot_end: u16,
}

impl ScheduleDto {
    fn from_state(state: &PlantState, schedule: Option<&sim_core::Schedule>) -> Self {
        // Convert decimal hours (e.g. 5.5) to HHMM (e.g. 530).
        // 60 = disabled sentinel (minutes > 59).
        let hhmm = |decimal_hours: f64| -> u16 {
            if decimal_hours <= 0.0 {
                return 60;
            }
            let h = decimal_hours.floor() as u16;
            let m = ((decimal_hours - h as f64) * 60.0).round() as u16;
            h * 100 + m
        };

        let (cs, ce, ds, de, cs2, ce2, ds2, de2, ct, _dt) = match schedule {
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
                s.discharge_target_soc,
            ),
            None => (0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 100.0, 10.0),
        };

        Self {
            enable_charge: (cs != ce || cs2 != ce2)
                && state.inverter.mode_state.effective == sim_models::InverterMode::ForceCharge,
            enable_discharge: (ds != de || ds2 != de2)
                && state.inverter.mode_state.effective == sim_models::InverterMode::ForceDischarge,
            soc_reserve: state.min_aggregate_soc(),
            charge_target_soc: ct,
            charge_slot_1_start: hhmm(cs),
            charge_slot_1_end: hhmm(ce),
            charge_slot_2_start: hhmm(cs2),
            charge_slot_2_end: hhmm(ce2),
            discharge_slot_1_start: hhmm(ds),
            discharge_slot_1_end: hhmm(de),
            discharge_slot_2_start: hhmm(ds2),
            discharge_slot_2_end: hhmm(de2),
            battery_pause_mode: 0,
            pause_slot_start: 60,
            pause_slot_end: 60,
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
                let eco = state.inverter.mode_state.effective == sim_models::InverterMode::Eco;
                let enable_discharge =
                    state.inverter.mode_state.effective == sim_models::InverterMode::ForceDischarge;
                let soc_reserve = state.min_aggregate_soc();
                match (eco, enable_discharge, (soc_reserve.round() as u16) == 100) {
                    (true, false, false) => "Eco",
                    (true, false, true) => "EcoPaused",
                    (true, true, _) => "TimedDemand",
                    (false, true, _) => "TimedExport",
                    (false, false, false) => "ExportPaused",
                    (false, false, true) => "ExportPaused",
                }
                .to_string()
            },
            inverter_type: state.config.inverter_type.clone(),
            inverter_ac_power_w: state.inverter.ac_power_w,
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
