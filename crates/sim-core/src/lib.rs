//! Simulation core: [`PlantState`], tick scheduler, command queue.
//!
//! State transitions occur only during simulation ticks.
//! All external writes become [`Command`]s applied between ticks.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use sim_models::DeviceModel;

// ---------------------------------------------------------------------------
// Sub-system state snapshots
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

/// Inverter state snapshot.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InverterState {
    pub mode: InverterMode,
    /// AC power output in watts (positive = exporting to house/grid).
    pub ac_power_w: f64,
    /// Export limit in watts (only relevant in ExportLimit mode).
    pub export_limit_w: f64,
}

impl Default for InverterState {
    fn default() -> Self {
        Self {
            mode: InverterMode::Normal,
            ac_power_w: 0.0,
            export_limit_w: 0.0,
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
    /// Net power flow: positive = charging, negative = discharging.
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
    /// Grid import (+) / export (-) in watts.
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

// ---------------------------------------------------------------------------
// Commands — external writes become these, applied between ticks
// ---------------------------------------------------------------------------

/// Commands that external actors (UI, Modbus client) can issue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Command {
    SetInverterMode(InverterMode),
    SetExportLimit(f64),
    SetMinSoc(f64),
    SetMaxSoc(f64),
    InjectFault(String),
    ClearFault(String),
}

// ---------------------------------------------------------------------------
// Tick scheduler
// ---------------------------------------------------------------------------

/// Runs the simulation loop: drain commands → tick all devices → repeat.
pub struct SimulationEngine {
    pub state: PlantState,
    devices: Vec<Box<dyn DeviceModel>>,
    command_queue: Vec<Command>,
    /// Tick interval in seconds.
    pub tick_interval_secs: u64,
}

impl SimulationEngine {
    pub fn new(
        state: PlantState,
        devices: Vec<Box<dyn DeviceModel>>,
        tick_interval_secs: u64,
    ) -> Self {
        Self {
            state,
            devices,
            command_queue: Vec::new(),
            tick_interval_secs,
        }
    }

    /// Enqueue a command to be applied before the next tick.
    pub fn enqueue(&mut self, cmd: Command) {
        self.command_queue.push(cmd);
    }

    /// Apply all pending commands to plant state.
    fn apply_commands(&mut self) {
        for cmd in self.command_queue.drain(..) {
            match cmd {
                Command::SetInverterMode(mode) => self.state.inverter.mode = mode,
                Command::SetExportLimit(limit) => self.state.inverter.export_limit_w = limit,
                Command::SetMinSoc(v) => self.state.battery.min_soc = v,
                Command::SetMaxSoc(v) => self.state.battery.max_soc = v,
                Command::InjectFault(id) => {
                    if !self.state.active_faults.contains(&id) {
                        self.state.active_faults.push(id);
                    }
                }
                Command::ClearFault(id) => {
                    self.state.active_faults.retain(|f| f != &id);
                }
            }
        }
    }

    /// Advance simulation by one tick.
    ///
    /// 1. Apply pending commands.
    /// 2. Build tick context.
    /// 3. Call `update` on every device model.
    /// 4. Advance the timestamp.
    pub fn tick(&mut self) {
        self.apply_commands();

        let dt_hours = self.tick_interval_secs as f64 / 3600.0;
        let ctx = sim_models::TickContext {
            now: self.state.timestamp,
            dt_hours,
        };

        for device in &mut self.devices {
            device.update(&ctx);
        }

        // Advance timestamp
        self.state.timestamp += chrono::TimeDelta::seconds(self.tick_interval_secs as i64);
    }

    /// Convenience: run `n` ticks.
    pub fn run_for(&mut self, ticks: usize) {
        for _ in 0..ticks {
            self.tick();
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    #[test]
    fn plant_state_default_values() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let state = PlantState::new(ts);
        assert_eq!(state.inverter.mode, InverterMode::Normal);
        assert_eq!(state.battery.soc_percent, 50.0);
        assert!(state.active_faults.is_empty());
    }

    #[test]
    fn command_changes_inverter_mode() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut engine = SimulationEngine::new(PlantState::new(ts), vec![], 30);
        engine.enqueue(Command::SetInverterMode(InverterMode::ForceCharge));
        engine.tick();
        assert_eq!(engine.state.inverter.mode, InverterMode::ForceCharge);
    }

    #[test]
    fn tick_advances_timestamp() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut engine = SimulationEngine::new(PlantState::new(ts), vec![], 30);
        engine.tick();
        assert_eq!(
            engine.state.timestamp,
            NaiveDate::from_ymd_opt(2025, 6, 1)
                .unwrap()
                .and_hms_opt(12, 0, 30)
                .unwrap()
        );
    }
}
