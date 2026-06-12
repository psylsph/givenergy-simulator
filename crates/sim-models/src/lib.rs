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
    /// Default returns a leaked `Box<()>` so that unexpected downcasts
    /// return `None` instead of panicking.
    /// Only [`ScheduleEngine`](sim_core::ScheduleEngine) overrides this.
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        // Leak a () so the reference is valid for 'static. This is fine
        // because there are at most N device models (single digits).
        Box::leak(Box::new(()))
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
    /// DSP firmware version (HR 19). Defaults to a value appropriate to the
    /// inverter type (449 for most hybrids) but can be overridden at runtime.
    #[serde(default = "default_dsp_firmware")]
    pub dsp_firmware_version: u16,
    /// ARM firmware version (HR 21). When set to 0 the projection falls back
    /// to a default per inverter type (which encodes the 'century' used by
    /// upstream clients to disambiguate the 0x2001 hybrid family).
    /// Set non-zero to override.
    #[serde(default)]
    pub arm_firmware_version: u16,
    /// Total powered-on runtime in hours (IR 47-48).
    #[serde(default)]
    pub work_time_hours: f64,
    /// HR 104: Battery self-heating enabled (hardware/batch-gated on real units).
    #[serde(default)]
    pub battery_self_heating: bool,
    /// HR 172: Manual battery heater enabled (hardware-gated like 104).
    #[serde(default)]
    pub manual_battery_heater: bool,
}

fn default_dsp_firmware() -> u16 {
    449
}

impl Default for InverterState {
    fn default() -> Self {
        Self {
            mode_state: ModeState::default(),
            ac_power_w: 0.0,
            export_limit_w: 3600.0,
            temperature_celsius: 35.0,
            dsp_firmware_version: default_dsp_firmware(),
            arm_firmware_version: 0,
            work_time_hours: 0.0,
            battery_self_heating: false,
            manual_battery_heater: false,
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
    /// One-way charging efficiency (0.0–1.0). Default 0.95.
    /// Round-trip efficiency is charge_efficiency × discharge_efficiency ≈ 0.9025 (90.25%).
    pub charge_efficiency: f64,
    /// One-way discharging efficiency (0.0–1.0). Default 0.95.
    /// Round-trip efficiency is charge_efficiency × discharge_efficiency ≈ 0.9025 (90.25%).
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
            min_soc: 4.0,
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
    /// Energy used for AC/grid charging today (kWh).
    #[serde(default)]
    pub ac_charge_kwh: f64,
    /// Total inverter AC output energy (kWh). Distinct from solar_generation:
    /// for hybrid inverters this includes battery-discharge contribution to AC.
    #[serde(default)]
    pub inverter_output_kwh: f64,
}

impl EnergyTotals {
    /// Small non-zero starter totals used by simulator frontends/register projection.
    ///
    /// Keeping core defaults at zero preserves deterministic physics tests, while
    /// seeding UI/API simulations with this fixture makes all common energy
    /// registers immediately testable before a full day of simulation has run.
    pub fn non_zero_test_fixture() -> Self {
        Self {
            grid_import_kwh: 1.5,
            grid_export_kwh: 2.5,
            battery_charge_kwh: 3.5,
            battery_discharge_kwh: 4.5,
            solar_generation_kwh: 8.5,
            load_consumption_kwh: 6.5,
            ac_charge_kwh: 0.7,
            inverter_output_kwh: 8.0,
        }
    }

    /// True when every energy bucket is exactly zero.
    pub fn is_all_zero(&self) -> bool {
        self.grid_import_kwh == 0.0
            && self.grid_export_kwh == 0.0
            && self.battery_charge_kwh == 0.0
            && self.battery_discharge_kwh == 0.0
            && self.solar_generation_kwh == 0.0
            && self.load_consumption_kwh == 0.0
            && self.ac_charge_kwh == 0.0
    }

    /// Replace an all-zero total set with the non-zero testing fixture.
    pub fn seed_for_testing_if_zero(&mut self) {
        if self.is_all_zero() {
            *self = Self::non_zero_test_fixture();
        }
    }
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
            ac_charge_kwh: 0.0,
            inverter_output_kwh: 0.0,
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
    /// Whether an external CT clamp meter is installed on slave 0x01.
    /// When false, IR 60-89 on slave 0x01 returns all zeros
    /// so the client's meter probe (`validate_meter_data`) fails.
    /// The inverter's built-in grid CT data still reports via IR 30,
    /// IR 42-43 on the inverter slave (0x32/0x31).
    #[serde(default = "default_true")]
    pub ct_meter_installed: bool,
}

fn default_inverter_type() -> String {
    "Gen3".to_string()
}
fn default_max_ac_watts() -> f64 {
    5000.0
}
fn default_percent_100() -> f64 {
    100.0
}
fn default_percent_4() -> f64 {
    4.0
}
fn default_disabled_hhmm() -> u16 {
    60
}
fn default_true() -> bool {
    true
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
            ct_meter_installed: true,
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
    /// GivEVC (Electric Vehicle Charger) simulation state.
    #[serde(default)]
    pub evc: EvcState,
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
    /// Never serialized — resets every tick, so persisted state would always be stale.
    #[serde(skip)]
    pub scheduled_charge: bool,
    /// Set by ScheduleEngine each tick when a discharge schedule window is active.
    /// Never serialized — resets every tick.
    #[serde(skip)]
    pub scheduled_discharge: bool,
    /// HR20 enable charge target flag.
    #[serde(default)]
    pub enable_charge_target: bool,
    /// HR50 active power rate percentage.
    #[serde(default = "default_percent_100")]
    pub active_power_rate_percent: f64,
    /// HR111 battery charge limit percentage.
    #[serde(default = "default_percent_100")]
    pub battery_charge_limit_percent: f64,
    /// HR112 battery discharge limit percentage.
    #[serde(default = "default_percent_100")]
    pub battery_discharge_limit_percent: f64,
    /// HR318 battery pause mode.
    #[serde(default)]
    pub battery_pause_mode: u16,
    /// HR319 battery pause slot start (HHMM).
    #[serde(default = "default_disabled_hhmm")]
    pub battery_pause_slot_start: u16,
    /// HR320 battery pause slot end (HHMM).
    #[serde(default = "default_disabled_hhmm")]
    pub battery_pause_slot_end: u16,
    /// HR114 battery discharge min power reserve (%).
    #[serde(default = "default_percent_4")]
    pub battery_discharge_min_power_reserve: f64,
    /// HR166 enable RTC (for persisting settings to EEPROM).
    #[serde(default)]
    pub enable_rtc: bool,
    /// HR311 export priority (0=Battery First, 1=Grid First, 2=Load First).
    ///
    /// **Gap (LOW-5):** This field is stored and projected to registers but no
    /// device model reads it. InverterEngine always uses Normal/Eco/Force
    /// priority irrespective of this setting.
    #[serde(default)]
    pub export_priority: u16,
    /// HR317 enable EPS (Emergency Power Supply) mode.
    #[serde(default)]
    pub enable_eps: bool,
    /// HR199 enable inverter parallel mode.
    #[serde(default)]
    pub enable_inverter_parallel_mode: bool,
    /// When true, EMS (Energy Management System) slot registers (HR 2044-2061)
    /// control the charge/discharge schedule instead of inverter-native slots.
    ///
    /// **Gap (LOW-6):** This flag is stored and projected but no logic switches
    /// between EMS slot offsets (2044-2061) and inverter-native slots (94-95,
    /// 56-57, etc.). ScheduleEngine always reads the native slot fields on
    /// Schedule. Implement offset switching based on this flag.
    #[serde(default)]
    pub ems_enabled: bool,
    /// When `Some(tick_count)`, the BatteryEngine must not change SOC for this many
    /// remaining ticks. Set by the GUI when the user manually drags the SOC slider.
    /// Allows the user's value to "stick" for a short wall-clock period before the
    /// simulation resumes normal battery physics.
    #[serde(skip)]
    pub manual_soc_hold_ticks: u64,
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
            enable_charge_target: false,
            active_power_rate_percent: 100.0,
            battery_charge_limit_percent: 100.0,
            battery_discharge_limit_percent: 100.0,
            battery_pause_mode: 0,
            battery_pause_slot_start: 60,
            battery_pause_slot_end: 60,
            battery_discharge_min_power_reserve: 4.0,
            enable_rtc: false,
            export_priority: 0,
            enable_eps: false,
            enable_inverter_parallel_mode: false,
            manual_soc_hold_ticks: 0,
            ems_enabled: false,
            evc: EvcState::default(),
        }
    }

    /// Create a state with the given number of battery modules (1–6).
    pub fn with_battery_count(timestamp: NaiveDateTime, count: usize) -> Self {
        let count = count.clamp(1, 6);
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
            enable_charge_target: false,
            active_power_rate_percent: 100.0,
            battery_charge_limit_percent: 100.0,
            battery_discharge_limit_percent: 100.0,
            battery_pause_mode: 0,
            battery_pause_slot_start: 60,
            battery_pause_slot_end: 60,
            battery_discharge_min_power_reserve: 4.0,
            enable_rtc: false,
            export_priority: 0,
            enable_eps: false,
            enable_inverter_parallel_mode: false,
            manual_soc_hold_ticks: 0,
            ems_enabled: false,
            evc: EvcState::default(),
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

    /// Effective max charge rate in watts, after applying HR 111 percentage limit.
    /// Used by InverterEngine so power allocation respects user-configured limits.
    pub fn effective_max_charge_w(&self) -> f64 {
        let scale = (self.battery_charge_limit_percent / 100.0).clamp(0.0, 1.0);
        self.total_max_charge_kw() * 1000.0 * scale
    }

    /// Effective max discharge rate in watts, after applying HR 112 percentage limit.
    /// Used by InverterEngine so power allocation respects user-configured limits.
    pub fn effective_max_discharge_w(&self) -> f64 {
        let scale = (self.battery_discharge_limit_percent / 100.0).clamp(0.0, 1.0);
        self.total_max_discharge_kw() * 1000.0 * scale
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
// MeterState — CT clamp meter data
// ---------------------------------------------------------------------------

/// State of a single CT clamp meter (grid import/export metering point).
/// Derived from PlantState.grid — stored separately for Modbus serving.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MeterState {
    /// Per-phase voltage (V ×0.1). Single-phase: v_phase_1 only.
    pub v_phase_1: f64,
    pub v_phase_2: f64,
    pub v_phase_3: f64,
    /// Per-phase current (A ×0.01)
    pub i_phase_1: f64,
    pub i_phase_2: f64,
    pub i_phase_3: f64,
    pub i_total: f64,
    /// Per-phase active power (W, signed: +export / −import)
    pub p_active_phase_1: f64,
    pub p_active_phase_2: f64,
    pub p_active_phase_3: f64,
    pub p_active_total: f64,
    /// Per-phase reactive power (var)
    pub p_reactive_phase_1: f64,
    pub p_reactive_phase_2: f64,
    pub p_reactive_phase_3: f64,
    pub p_reactive_total: f64,
    /// Per-phase apparent power (VA)
    pub p_apparent_phase_1: f64,
    pub p_apparent_phase_2: f64,
    pub p_apparent_phase_3: f64,
    pub p_apparent_total: f64,
    /// Per-phase power factor (×0.001)
    pub pf_phase_1: f64,
    pub pf_phase_2: f64,
    pub pf_phase_3: f64,
    pub pf_total: f64,
    /// Grid frequency (Hz ×0.01)
    pub frequency: f64,
    /// Import/export active energy today (kWh ×0.1)
    pub e_import_active: f64,
    pub e_export_active: f64,
}

impl Default for MeterState {
    fn default() -> Self {
        Self {
            v_phase_1: 240.0,
            v_phase_2: 0.0,
            v_phase_3: 0.0,
            i_phase_1: 0.0,
            i_phase_2: 0.0,
            i_phase_3: 0.0,
            i_total: 0.0,
            p_active_phase_1: 0.0,
            p_active_phase_2: 0.0,
            p_active_phase_3: 0.0,
            p_active_total: 0.0,
            p_reactive_phase_1: 0.0,
            p_reactive_phase_2: 0.0,
            p_reactive_phase_3: 0.0,
            p_reactive_total: 0.0,
            p_apparent_phase_1: 0.0,
            p_apparent_phase_2: 0.0,
            p_apparent_phase_3: 0.0,
            p_apparent_total: 0.0,
            pf_phase_1: 1000.0,
            pf_phase_2: 0.0,
            pf_phase_3: 0.0,
            pf_total: 1000.0,
            frequency: 50.0,
            e_import_active: 0.0,
            e_export_active: 0.0,
        }
    }
}

impl From<&PlantState> for MeterState {
    /// Derive meter readings from the current plant state.
    fn from(state: &PlantState) -> Self {
        let grid_w = state.grid.power_w; // +import / −export
        let grid_v = 240.0;
        let _grid_i = grid_w.abs() / grid_v;
        // Single-phase: all power on phase 1
        // Positive = import (active power positive = importing)
        // For GivEnergy convention: +W = import, −W = export
        let p1 = grid_w;
        let p2 = 0.0;
        let p3 = 0.0;
        let pt = grid_w;
        let i1 = if grid_v > 0.0 { p1 / grid_v } else { 0.0 };

        Self {
            v_phase_1: grid_v,
            v_phase_2: 0.0,
            v_phase_3: 0.0,
            i_phase_1: i1,
            i_phase_2: 0.0,
            i_phase_3: 0.0,
            i_total: i1,
            p_active_phase_1: p1.clamp(-32768.0, 32767.0),
            p_active_phase_2: p2,
            p_active_phase_3: p3,
            p_active_total: pt.clamp(-32768.0, 32767.0),
            p_reactive_phase_1: 0.0,
            p_reactive_phase_2: 0.0,
            p_reactive_phase_3: 0.0,
            p_reactive_total: 0.0,
            p_apparent_phase_1: p1.abs(),
            p_apparent_phase_2: 0.0,
            p_apparent_phase_3: 0.0,
            p_apparent_total: pt.abs(),
            pf_phase_1: if pt.abs() > 1.0 { 1000.0 } else { 0.0 },
            pf_phase_2: 0.0,
            pf_phase_3: 0.0,
            pf_total: if pt.abs() > 1.0 { 1000.0 } else { 0.0 },
            frequency: 50.0,
            e_import_active: state.energy_totals.grid_import_kwh,
            e_export_active: state.energy_totals.grid_export_kwh,
        }
    }
}

// ---------------------------------------------------------------------------
// EvcState — GivEnergy Electric Vehicle Charger
// ---------------------------------------------------------------------------

/// State of a GivEVC (Electric Vehicle Charger) wallbox.
/// Communicates via STANDARD Modbus TCP on port 502 (not proprietary GivEnergy framing).
///
/// Register map matches GivTCP evc.py / EVCLut.evc_lut (115 holding registers, HR 0-114).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct EvcState {
    /// Whether the EVC simulation is enabled. When false, the engine is idle
    /// regardless of cable state or charge_control.
    pub enabled: bool,
    /// HR 0: Charging state enum (0=Unknown, 1=Idle, 2=Connected, 3=Starting,
    /// 4=Charging, 5=Startup Failure, 6=End of Charging, 7=System Failure,
    /// 8=Scheduled, 9=Updating, 10=Unstable CP)
    pub charging_state: u16,
    /// HR 2: Connection status (0=Not Connected, 1=Connected)
    pub connection_status: u16,
    /// HR 4: Error code (0=Clear, 11=CP voltage abnormal, etc.)
    pub error_code: u16,
    /// HR 6: L1 current in deci-Amps (÷10 for Amps)
    pub current_l1: f64,
    /// HR 8: L2 current in deci-Amps (÷10 for Amps)
    pub current_l2: f64,
    /// HR 10: L3 current in deci-Amps (÷10 for Amps)
    pub current_l3: f64,
    /// HR 13: Active power total (Watts)
    pub active_power_w: f64,
    /// HR 17: Active power L1 (Watts)
    pub active_power_l1: f64,
    /// HR 20: Active power L2 (Watts)
    pub active_power_l2: f64,
    /// HR 24: Active power L3 (Watts)
    pub active_power_l3: f64,
    /// HR 29: Meter energy total (÷10 kWh)
    pub meter_energy_kwh: f64,
    /// HR 32: EVSE max current (hardware limit, typically 32A)
    pub evse_max_current: u16,
    /// HR 34: EVSE min current (hardware limit, typically 6A)
    pub evse_min_current: u16,
    /// HR 36: Charge limit in deci-Amps (÷10 for Amps)
    pub charge_limit: f64,
    /// HR 38-68: Serial number (ASCII, each register = one char)
    pub serial_number: String,
    /// HR 72: Charge session energy (kWh)
    pub session_energy_kwh: f64,
    /// HR 79: Charge session duration (seconds)
    pub session_duration_secs: u64,
    /// HR 93: Plug and Go (0=enable, 1=disable)
    pub plug_and_go: u16,
    /// HR 94 / write HR 91: Charge control (0=Ready, 1=Start, 2=Stop)
    pub charge_control: u16,
    /// Write HR 91: Charge current limit (raw, ×10 deci-Amps)
    pub charge_current_limit: u16,
    /// HR 109: Voltage L1 (÷10 V)
    pub voltage_l1: f64,
    /// HR 111: Voltage L2 (÷10 V)
    pub voltage_l2: f64,
    /// HR 113: Voltage L3 (÷10 V)
    pub voltage_l3: f64,
}

impl Default for EvcState {
    fn default() -> Self {
        Self {
            enabled: false,
            charging_state: 1,    // Idle
            connection_status: 0, // Not Connected
            error_code: 0,        // Clear
            current_l1: 0.0,
            current_l2: 0.0,
            current_l3: 0.0,
            active_power_w: 0.0,
            active_power_l1: 0.0,
            active_power_l2: 0.0,
            active_power_l3: 0.0,
            meter_energy_kwh: 1234.5, // realistic cumulative total
            evse_max_current: 32,
            evse_min_current: 6,
            charge_limit: 32.0, // 320 ÷ 10 = 32.0A
            serial_number: "11288853538258".to_string(),
            session_energy_kwh: 0.0,
            session_duration_secs: 0,
            plug_and_go: 0,            // enabled
            charge_control: 0,         // Ready
            charge_current_limit: 320, // 32.0A × 10
            voltage_l1: 241.0,
            voltage_l2: 241.0,
            voltage_l3: 241.0,
        }
    }
}

// ---------------------------------------------------------------------------
// Schedule — moved from sim-core to break circular dependency
// ---------------------------------------------------------------------------

/// Schedule parameters — up to 10 independent charge and discharge windows.
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
    // Slots 3-10 charge
    pub charge_start_3: f64,
    pub charge_end_3: f64,
    pub charge_target_soc_3: f64,
    pub charge_start_4: f64,
    pub charge_end_4: f64,
    pub charge_target_soc_4: f64,
    pub charge_start_5: f64,
    pub charge_end_5: f64,
    pub charge_target_soc_5: f64,
    pub charge_start_6: f64,
    pub charge_end_6: f64,
    pub charge_target_soc_6: f64,
    pub charge_start_7: f64,
    pub charge_end_7: f64,
    pub charge_target_soc_7: f64,
    pub charge_start_8: f64,
    pub charge_end_8: f64,
    pub charge_target_soc_8: f64,
    pub charge_start_9: f64,
    pub charge_end_9: f64,
    pub charge_target_soc_9: f64,
    pub charge_start_10: f64,
    pub charge_end_10: f64,
    pub charge_target_soc_10: f64,
    // Slots 3-10 discharge
    pub discharge_start_3: f64,
    pub discharge_end_3: f64,
    pub discharge_target_soc_3: f64,
    pub discharge_start_4: f64,
    pub discharge_end_4: f64,
    pub discharge_target_soc_4: f64,
    pub discharge_start_5: f64,
    pub discharge_end_5: f64,
    pub discharge_target_soc_5: f64,
    pub discharge_start_6: f64,
    pub discharge_end_6: f64,
    pub discharge_target_soc_6: f64,
    pub discharge_start_7: f64,
    pub discharge_end_7: f64,
    pub discharge_target_soc_7: f64,
    pub discharge_start_8: f64,
    pub discharge_end_8: f64,
    pub discharge_target_soc_8: f64,
    pub discharge_start_9: f64,
    pub discharge_end_9: f64,
    pub discharge_target_soc_9: f64,
    pub discharge_start_10: f64,
    pub discharge_end_10: f64,
    pub discharge_target_soc_10: f64,
    /// When true, charge any time SOC < target (no window restriction).
    pub enable_charge: bool,
    /// When true, discharge any time SOC > target (no window restriction).
    pub enable_discharge: bool,
    /// Export limit scheduling — 3 time windows with per-window SOC targets.
    /// Outside any export window the user-set export limit applies.
    /// Inside a window, if SOC > the window's target, export is allowed up to export_power_limit_w.
    /// If SOC <= target, export is curtailed to 0 (battery reserves for backup).
    pub export_start_1: f64,
    pub export_end_1: f64,
    pub export_target_soc_1: f64,
    pub export_start_2: f64,
    pub export_end_2: f64,
    pub export_target_soc_2: f64,
    pub export_start_3: f64,
    pub export_end_3: f64,
    pub export_target_soc_3: f64,
    /// Global export power limit in watts (applied during any active export window).
    pub export_power_limit_w: f64,
    /// When true, export limit scheduling is active.
    pub enable_export_schedule: bool,
}

impl Default for Schedule {
    fn default() -> Self {
        Self {
            charge_start: 0.0,
            charge_end: 0.0,
            discharge_start: 0.0,
            discharge_end: 0.0,
            charge_start_2: 0.0,
            charge_end_2: 0.0,
            discharge_start_2: 0.0,
            discharge_end_2: 0.0,
            charge_target_soc: 100.0,
            charge_target_soc_2: 100.0,
            discharge_target_soc: 4.0,
            discharge_target_soc_2: 4.0,
            charge_start_3: 0.0,
            charge_end_3: 0.0,
            charge_target_soc_3: 100.0,
            charge_start_4: 0.0,
            charge_end_4: 0.0,
            charge_target_soc_4: 100.0,
            charge_start_5: 0.0,
            charge_end_5: 0.0,
            charge_target_soc_5: 100.0,
            charge_start_6: 0.0,
            charge_end_6: 0.0,
            charge_target_soc_6: 100.0,
            charge_start_7: 0.0,
            charge_end_7: 0.0,
            charge_target_soc_7: 100.0,
            charge_start_8: 0.0,
            charge_end_8: 0.0,
            charge_target_soc_8: 100.0,
            charge_start_9: 0.0,
            charge_end_9: 0.0,
            charge_target_soc_9: 100.0,
            charge_start_10: 0.0,
            charge_end_10: 0.0,
            charge_target_soc_10: 100.0,
            discharge_start_3: 0.0,
            discharge_end_3: 0.0,
            discharge_target_soc_3: 4.0,
            discharge_start_4: 0.0,
            discharge_end_4: 0.0,
            discharge_target_soc_4: 4.0,
            discharge_start_5: 0.0,
            discharge_end_5: 0.0,
            discharge_target_soc_5: 4.0,
            discharge_start_6: 0.0,
            discharge_end_6: 0.0,
            discharge_target_soc_6: 4.0,
            discharge_start_7: 0.0,
            discharge_end_7: 0.0,
            discharge_target_soc_7: 4.0,
            discharge_start_8: 0.0,
            discharge_end_8: 0.0,
            discharge_target_soc_8: 4.0,
            discharge_start_9: 0.0,
            discharge_end_9: 0.0,
            discharge_target_soc_9: 4.0,
            discharge_start_10: 0.0,
            discharge_end_10: 0.0,
            discharge_target_soc_10: 4.0,
            enable_charge: false,
            enable_discharge: false,
            export_start_1: 0.0,
            export_end_1: 0.0,
            export_target_soc_1: 50.0,
            export_start_2: 0.0,
            export_end_2: 0.0,
            export_target_soc_2: 50.0,
            export_start_3: 0.0,
            export_end_3: 0.0,
            export_target_soc_3: 50.0,
            export_power_limit_w: 0.0,
            enable_export_schedule: false,
        }
    }
}
