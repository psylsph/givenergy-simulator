//! Device model trait, tick context, and plant state types.
//!
//! All device models (solar, load, battery, inverter) implement
//! [`DeviceModel`], called once per simulation tick with mutable access
//! to [`PlantState`].

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Calibration state
// ---------------------------------------------------------------------------

/// Battery calibration stages (matches GivEnergy HR 29 values).
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub enum CalibrationStage {
    #[default]
    Off = 0,
    /// Stage 1: Force charge to 100%
    ChargeToFull = 1,
    /// Stage 2: Hold at 100% for BMS settling (30 min real-time)
    HoldingFull = 2,
    /// Stage 3: Discharge to reserve SOC
    DischargeToEmpty = 3,
    /// Calibration complete
    Complete = 4,
}

impl CalibrationStage {
    pub fn from_u8(v: u8) -> Self {
        match v {
            1 => Self::ChargeToFull,
            2 => Self::HoldingFull,
            3 => Self::DischargeToEmpty,
            4 => Self::Complete,
            _ => Self::Off,
        }
    }
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

/// Tracks calibration progress for the battery bank.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CalibrationState {
    /// Current calibration stage.
    pub stage: CalibrationStage,
    /// Target module index (None = all modules).
    pub module: Option<usize>,
    /// Simulated seconds elapsed in current stage.
    pub stage_elapsed_secs: f64,
    /// Total simulated seconds for the full calibration cycle.
    pub total_elapsed_secs: f64,
}

// ---------------------------------------------------------------------------
// Tick context
// ---------------------------------------------------------------------------

/// Context provided to every device model on each tick.
pub struct TickContext {
    /// Current simulation timestamp.
    pub now: NaiveDateTime,
    /// Tick duration in fractional hours (e.g. 0.008_333 for 30 s).
    pub dt_hours: f64,
}

// ---------------------------------------------------------------------------
// Device model trait
// ---------------------------------------------------------------------------

/// A pluggable device model.
///
/// Implementors read from and write to [`PlantState`] each tick.
/// Devices are called in registration order — the simulation engine
/// must register them as: Solar → Load → Inverter → Battery.
pub trait DeviceModel: Send {
    /// Advance the model by one tick.
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState);
    /// Downcast support for schedule updates.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        unimplemented!()
    }
}

// ---------------------------------------------------------------------------
// Inverter mode
// ---------------------------------------------------------------------------

/// Inverter operating mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum InverterMode {
    Normal,
    Eco,
    ForceCharge,
    ForceDischarge,
    ExportLimit,
}

/// Source of the current effective inverter mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModeSource {
    /// Set directly by user via UI or Modbus.
    User,
    /// Overridden by active schedule window.
    Schedule,
    /// Forced by an active fault.
    Fault,
}

/// Composite mode state: the effective mode and why it is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModeState {
    pub effective: InverterMode,
    pub source: ModeSource,
    /// When `source == Schedule`, the mode the schedule wants.
    pub scheduled_mode: Option<InverterMode>,
}

impl Default for ModeState {
    fn default() -> Self {
        Self {
            effective: InverterMode::Eco,
            source: ModeSource::User,
            scheduled_mode: None,
        }
    }
}

impl ModeState {
    pub fn set_user(&mut self, mode: InverterMode) {
        self.effective = mode;
        self.source = ModeSource::User;
        self.scheduled_mode = None;
    }

    pub fn set_schedule(&mut self, mode: InverterMode) {
        self.effective = mode;
        self.source = ModeSource::Schedule;
        self.scheduled_mode = Some(mode);
    }

    pub fn set_fault(&mut self, mode: InverterMode) {
        self.effective = mode;
        self.source = ModeSource::Fault;
        self.scheduled_mode = None;
    }
}

// ---------------------------------------------------------------------------
// Sub-system state snapshots
// ---------------------------------------------------------------------------

/// Inverter state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InverterState {
    pub mode_state: ModeState,
    /// AC power output in watts (positive = generating).
    pub ac_power_w: f64,
    /// Export limit in watts (only relevant in ExportLimit mode).
    pub export_limit_w: f64,
    /// Inverter temperature in °C.
    pub temperature_celsius: f64,
}

impl Default for InverterState {
    fn default() -> Self {
        Self {
            mode_state: ModeState::default(),
            ac_power_w: 0.0,
            export_limit_w: 3600.0,
            temperature_celsius: 35.0,
        }
    }
}

/// Battery state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryState {
    /// State of charge 0.0–100.0.
    pub soc_percent: f64,
    /// Capacity in kWh (degrades with cycling).
    pub capacity_kwh: f64,
    /// Original capacity in kWh (before degradation).
    pub nominal_capacity_kwh: f64,
    /// Max charge rate in kW.
    pub max_charge_kw: f64,
    /// Max discharge rate in kW.
    pub max_discharge_kw: f64,
    /// Min SOC (%).
    pub min_soc: f64,
    /// Max SOC (%).
    pub max_soc: f64,
    /// Net power flow in kW: positive = charging, negative = discharging.
    pub power_kw: f64,
    /// Round-trip charging efficiency (0.0–1.0). Typical Li-ion ≈ 0.95.
    pub charge_efficiency: f64,
    /// Round-trip discharging efficiency (0.0–1.0). Typical Li-ion ≈ 0.95.
    pub discharge_efficiency: f64,
    /// Battery temperature in °C.
    pub temperature_celsius: f64,
    /// Cumulative energy throughput in kWh (for cycle counting).
    pub throughput_kwh: f64,
    /// State of Health (0.0–1.0). 1.0 = new, degrades over time.
    pub soh: f64,
    /// Equivalent full cycles (throughput / nominal_capacity).
    pub cycle_count: f64,
    /// Terminal voltage in volts.
    #[serde(default)]
    pub voltage_v: f64,
    /// Terminal current in amps (signed: positive = charging).
    #[serde(default)]
    pub current_a: f64,
}

impl Default for BatteryState {
    fn default() -> Self {
        Self {
            soc_percent: 50.0,
            capacity_kwh: 9.5,
            nominal_capacity_kwh: 9.5,
            max_charge_kw: 3.0,
            max_discharge_kw: 3.0,
            min_soc: 10.0,
            max_soc: 100.0,
            power_kw: 0.0,
            charge_efficiency: 0.95,
            discharge_efficiency: 0.95,
            temperature_celsius: 25.0,
            throughput_kwh: 0.0,
            soh: 1.0,
            cycle_count: 0.0,
            voltage_v: 48.0,
            current_a: 0.0,
        }
    }
}

/// Solar / PV state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolarState {
    /// Current total generation in watts (pv1_w + pv2_w).
    pub generation_w: f64,
    /// PV array 1 generation in watts.
    pub pv1_w: f64,
    /// PV array 2 generation in watts.
    pub pv2_w: f64,
}

impl Default for SolarState {
    fn default() -> Self {
        Self {
            generation_w: 0.0,
            pv1_w: 0.0,
            pv2_w: 0.0,
        }
    }
}

/// Grid connection state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GridState {
    /// Grid import (+) / export (−) in watts.
    pub power_w: f64,
    /// Whether the grid connection is live.
    pub connected: bool,
}

impl Default for GridState {
    fn default() -> Self {
        Self {
            power_w: 0.0,
            connected: true,
        }
    }
}

/// Household load state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadState {
    /// Current household consumption in watts.
    pub demand_w: f64,
}

impl Default for LoadState {
    fn default() -> Self {
        Self { demand_w: 0.0 }
    }
}

// ---------------------------------------------------------------------------
// Energy totals
// ---------------------------------------------------------------------------

/// Cumulative energy totals in kWh.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnergyTotals {
    /// Total energy imported from grid (kWh).
    pub grid_import_kwh: f64,
    /// Total energy exported to grid (kWh).
    pub grid_export_kwh: f64,
    /// Total energy charged into batteries (kWh).
    pub battery_charge_kwh: f64,
    /// Total energy discharged from batteries (kWh).
    pub battery_discharge_kwh: f64,
    /// Total solar energy generated (kWh).
    pub solar_generation_kwh: f64,
    /// Total energy consumed by household load (kWh).
    pub load_consumption_kwh: f64,
}

impl Default for EnergyTotals {
    fn default() -> Self {
        Self {
            grid_import_kwh: 0.0,
            grid_export_kwh: 0.0,
            battery_charge_kwh: 0.0,
            battery_discharge_kwh: 0.0,
            solar_generation_kwh: 0.0,
            load_consumption_kwh: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Plant configuration
// ---------------------------------------------------------------------------

/// Static configuration parameters for the simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlantConfig {
    /// Installed solar peak capacity in watts.
    pub solar_peak_watts: f64,
    /// Site latitude in degrees.
    pub latitude: f64,
    /// Tick interval in seconds.
    pub tick_interval_secs: u64,
    /// Inverter type identifier (e.g. "Gen3", "Gen3_8kW", "AC_Coupled").
    #[serde(default = "default_inverter_type")]
    pub inverter_type: String,
    /// Maximum AC output power in watts (set by inverter type).
    #[serde(default = "default_max_ac_watts")]
    pub max_ac_watts: f64,
    /// Peak capacity of PV array 2 in watts (0 = disabled).
    #[serde(default)]
    pub pv2_peak_watts: f64,
}

fn default_inverter_type() -> String {
    "Gen3".to_string()
}
fn default_max_ac_watts() -> f64 {
    5000.0
}

impl Default for PlantConfig {
    fn default() -> Self {
        Self {
            solar_peak_watts: 5000.0,
            latitude: 51.5,
            tick_interval_secs: 30,
            inverter_type: default_inverter_type(),
            max_ac_watts: default_max_ac_watts(),
            pv2_peak_watts: 0.0,
        }
    }
}

// ---------------------------------------------------------------------------
// PlantState — the authoritative simulation state
// ---------------------------------------------------------------------------

/// Top-level simulation state. Register banks are projections of this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlantState {
    pub timestamp: NaiveDateTime,
    pub inverter: InverterState,
    /// First battery module (convenience, also in `batteries[0]`).
    /// Kept for backward compatibility; always reflects `batteries[0]`.
    pub battery: BatteryState,
    /// Up to 3 battery modules. Index 0 matches `battery`.
    pub batteries: Vec<BatteryState>,
    pub solar: SolarState,
    pub load: LoadState,
    pub grid: GridState,
    /// Active fault IDs.
    pub active_faults: Vec<String>,
    /// Current weather condition (serde string representation).
    pub weather: String,
    /// Cumulative energy totals.
    pub energy_totals: EnergyTotals,
    /// Static plant configuration.
    pub config: PlantConfig,
    /// Manual override for solar generation (watts). None = use engine.
    #[serde(default)]
    pub solar_override: Option<f64>,
    /// Manual override for load demand (watts). None = use engine.
    #[serde(default)]
    pub load_override: Option<f64>,
    /// Battery calibration state.
    #[serde(default)]
    pub calibration: CalibrationState,
    /// Set by ScheduleEngine each tick when a charge schedule window is active.
    /// InverterEngine reads this to charge from grid while staying in Eco/Normal mode.
    #[serde(default)]
    pub scheduled_charge: bool,
    /// Set by ScheduleEngine each tick when a discharge schedule window is active.
    #[serde(default)]
    pub scheduled_discharge: bool,
}

impl PlantState {
    /// Create a default state at the given timestamp.
    pub fn new(timestamp: NaiveDateTime) -> Self {
        let default_battery = BatteryState::default();
        Self {
            timestamp,
            inverter: InverterState::default(),
            battery: default_battery.clone(),
            batteries: vec![default_battery],
            solar: SolarState::default(),
            load: LoadState::default(),
            grid: GridState::default(),
            active_faults: Vec::new(),
            weather: "Clear".to_string(),
            energy_totals: EnergyTotals::default(),
            solar_override: None,
            load_override: None,
            config: PlantConfig::default(),
            calibration: CalibrationState::default(),
            scheduled_charge: false,
            scheduled_discharge: false,
        }
    }

    /// Create a state with the given number of battery modules (1–3).
    pub fn with_battery_count(timestamp: NaiveDateTime, count: usize) -> Self {
        let count = count.clamp(1, 3);
        let batts: Vec<BatteryState> = (0..count).map(|_| BatteryState::default()).collect();
        Self {
            timestamp,
            inverter: InverterState::default(),
            battery: batts[0].clone(),
            batteries: batts,
            solar: SolarState::default(),
            load: LoadState::default(),
            grid: GridState::default(),
            active_faults: Vec::new(),
            weather: "Clear".to_string(),
            energy_totals: EnergyTotals::default(),
            solar_override: None,
            load_override: None,
            config: PlantConfig::default(),
            calibration: CalibrationState::default(),
            scheduled_charge: false,
            scheduled_discharge: false,
        }
    }

    /// Aggregate SOC across all battery modules (capacity-weighted average).
    pub fn aggregate_soc(&self) -> f64 {
        if self.batteries.is_empty() {
            return 0.0;
        }
        let total_cap: f64 = self.batteries.iter().map(|b| b.capacity_kwh).sum();
        if total_cap <= 0.0 {
            return 0.0;
        }
        self.batteries
            .iter()
            .map(|b| b.soc_percent * b.capacity_kwh / total_cap)
            .sum()
    }

    /// Total battery capacity in kWh.
    pub fn total_battery_capacity(&self) -> f64 {
        self.batteries.iter().map(|b| b.capacity_kwh).sum()
    }

    /// Aggregate max charge rate in kW.
    pub fn total_max_charge_kw(&self) -> f64 {
        self.batteries.iter().map(|b| b.max_charge_kw).sum()
    }

    /// Aggregate max discharge rate in kW.
    pub fn total_max_discharge_kw(&self) -> f64 {
        self.batteries.iter().map(|b| b.max_discharge_kw).sum()
    }

    /// Aggregate net power in kW (positive = charging bank).
    pub fn total_battery_power_kw(&self) -> f64 {
        self.batteries.iter().map(|b| b.power_kw).sum()
    }

    /// Sync `self.battery` to reflect `self.batteries[0]`.
    pub fn sync_battery_from_vec(&mut self) {
        if let Some(first) = self.batteries.first() {
            self.battery = first.clone();
        }
    }

    /// Sync `self.batteries[0]` to reflect `self.battery`.
    /// Used by tests and CLI code that sets `state.battery.x` directly.
    pub fn sync_vec_from_battery(&mut self) {
        if !self.batteries.is_empty() {
            self.batteries[0] = self.battery.clone();
        }
    }

    /// Distribute total power_kw evenly across all battery modules.
    /// Updates `power_kw` on each module, then syncs `battery` from `batteries[0]`.
    pub fn distribute_battery_power(&mut self, total_power_kw: f64) {
        let n = self.batteries.len().max(1);
        let per_module = total_power_kw / n as f64;
        for b in &mut self.batteries {
            b.power_kw = per_module;
        }
        self.sync_battery_from_vec();
    }

    /// Maximum SOC across all modules.
    pub fn max_aggregate_soc(&self) -> f64 {
        self.batteries
            .iter()
            .map(|b| b.max_soc)
            .min_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(100.0)
    }

    /// Minimum SOC across all modules.
    pub fn min_aggregate_soc(&self) -> f64 {
        self.batteries
            .iter()
            .map(|b| b.min_soc)
            .max_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
            .unwrap_or(0.0)
    }

    /// Average battery temperature across all modules.
    pub fn battery_temperature_celsius(&self) -> f64 {
        if self.batteries.is_empty() {
            return 25.0;
        }
        self.batteries
            .iter()
            .map(|b| b.temperature_celsius)
            .sum::<f64>()
            / self.batteries.len() as f64
    }
}

// ---------------------------------------------------------------------------
// Schedule — moved from sim-core to break circular dependency
// ---------------------------------------------------------------------------

/// Schedule parameters — two independent charge and discharge windows.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Schedule {
    /// Charge slot 1 start (decimal hours, e.g. 2.5 = 02:30).
    pub charge_start: f64,
    /// Charge slot 1 end (decimal hours).
    pub charge_end: f64,
    /// Discharge slot 1 start (decimal hours).
    pub discharge_start: f64,
    /// Discharge slot 1 end (decimal hours).
    pub discharge_end: f64,
    /// Charge slot 2 start (decimal hours).
    pub charge_start_2: f64,
    /// Charge slot 2 end (decimal hours).
    pub charge_end_2: f64,
    /// Discharge slot 2 start (decimal hours).
    pub discharge_start_2: f64,
    /// Discharge slot 2 end (decimal hours).
    pub discharge_end_2: f64,
    /// Target SOC for scheduled charging, slot 1 (%).
    pub charge_target_soc: f64,
    /// Target SOC for scheduled charging, slot 2 (%).
    pub charge_target_soc_2: f64,
    /// Target SOC for scheduled discharging, slot 1 (%).
    pub discharge_target_soc: f64,
    /// Target SOC for scheduled discharging, slot 2 (%).
    pub discharge_target_soc_2: f64,
    /// When true, charge any time SOC < target (no window restriction).
    pub enable_charge: bool,
    /// When true, discharge any time SOC > target (no window restriction).
    pub enable_discharge: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            charge_start: 0.0,
            charge_end: 5.5, // 05:30
            discharge_start: 0.0,
            discharge_end: 0.0,
            charge_start_2: 0.0,
            charge_end_2: 0.0,
            discharge_start_2: 0.0,
            discharge_end_2: 0.0,
            charge_target_soc: 100.0,
            charge_target_soc_2: 100.0,
            discharge_target_soc: 10.0,
            discharge_target_soc_2: 10.0,
            enable_charge: false,
            enable_discharge: false,
        }
    }
}
