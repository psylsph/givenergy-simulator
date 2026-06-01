//! Device model trait, tick context, and plant state types.
//!
//! All device models (solar, load, battery, inverter) implement
//! [`DeviceModel`], called once per simulation tick with mutable access
//! to [`PlantState`].

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

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
pub trait DeviceModel {
    /// Advance the model by one tick.
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState);
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

// ---------------------------------------------------------------------------
// Sub-system state snapshots
// ---------------------------------------------------------------------------

/// Inverter state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InverterState {
    pub mode: InverterMode,
    /// AC power output in watts (positive = generating).
    pub ac_power_w: f64,
    /// Export limit in watts (only relevant in ExportLimit mode).
    pub export_limit_w: f64,
}

impl Default for InverterState {
    fn default() -> Self {
        Self {
            mode: InverterMode::Normal,
            ac_power_w: 0.0,
            export_limit_w: 3600.0,
        }
    }
}

/// Battery state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BatteryState {
    /// State of charge 0.0–100.0.
    pub soc_percent: f64,
    /// Capacity in kWh.
    pub capacity_kwh: f64,
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
}

impl Default for BatteryState {
    fn default() -> Self {
        Self {
            soc_percent: 50.0,
            capacity_kwh: 9.5,
            max_charge_kw: 3.0,
            max_discharge_kw: 3.0,
            min_soc: 10.0,
            max_soc: 100.0,
            power_kw: 0.0,
        }
    }
}

/// Solar / PV state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolarState {
    /// Current generation in watts.
    pub generation_w: f64,
}

impl Default for SolarState {
    fn default() -> Self {
        Self { generation_w: 0.0 }
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
// PlantState — the authoritative simulation state
// ---------------------------------------------------------------------------

/// Top-level simulation state. Register banks are projections of this.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlantState {
    pub timestamp: NaiveDateTime,
    pub inverter: InverterState,
    pub battery: BatteryState,
    pub solar: SolarState,
    pub load: LoadState,
    pub grid: GridState,
    /// Active fault IDs.
    pub active_faults: Vec<String>,
}

impl PlantState {
    /// Create a default state at the given timestamp.
    pub fn new(timestamp: NaiveDateTime) -> Self {
        Self {
            timestamp,
            inverter: InverterState::default(),
            battery: BatteryState::default(),
            solar: SolarState::default(),
            load: LoadState::default(),
            grid: GridState::default(),
            active_faults: Vec::new(),
        }
    }
}
