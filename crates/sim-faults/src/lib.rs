//! Fault framework: categories, triggers, lifecycle, and effects.
//!
//! Faults may be manual, scheduled, or randomised.
//! Categories: Communication, Electrical, Sensor, Battery, Inverter.
//!
//! The [`FaultEngine`] device model applies fault effects to [`PlantState`]
//! every tick based on the active fault set.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// Broad fault category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaultCategory {
    Communication,
    Electrical,
    Sensor,
    Battery,
    Inverter,
}

/// How a fault is triggered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FaultTrigger {
    /// Injected manually by user / test / scenario.
    Manual,
    /// Fires at a specific simulation time.
    Scheduled,
    /// Fires probabilistically each tick.
    Randomised,
}

/// Definition of a fault that can occur in the simulation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FaultDef {
    /// Unique identifier (e.g. "grid_loss", "battery_over_temp").
    pub id: String,
    pub category: FaultCategory,
    pub trigger: FaultTrigger,
    /// Human-readable description.
    pub description: String,
    /// For Randomised faults: probability of firing per tick (0.0–1.0).
    #[serde(default)]
    pub probability: f64,
}

// ---------------------------------------------------------------------------
// Well-known fault IDs
// ---------------------------------------------------------------------------

pub mod well_known {
    pub const GRID_LOSS: &str = "grid_loss";
    pub const GRID_RESTORE: &str = "grid_restore";
    pub const BATTERY_OVER_TEMP: &str = "battery_over_temp";
    pub const INVERTER_TRIP: &str = "inverter_trip";
    pub const COMM_TIMEOUT: &str = "comm_timeout";
    pub const SENSOR_DRIFT: &str = "sensor_drift";
    pub const COLD_BATTERY: &str = "cold_battery";
}

/// Return the default catalogue of known faults.
pub fn default_fault_catalogue() -> Vec<FaultDef> {
    use FaultCategory as C;
    use FaultTrigger as T;

    vec![
        FaultDef {
            id: well_known::GRID_LOSS.to_string(),
            category: C::Electrical,
            trigger: T::Manual,
            description: "Grid connection lost".into(),
            probability: 0.0,
        },
        FaultDef {
            id: well_known::BATTERY_OVER_TEMP.to_string(),
            category: C::Battery,
            trigger: T::Manual,
            description: "Battery thermal limit exceeded — charging disabled".into(),
            probability: 0.0,
        },
        FaultDef {
            id: well_known::INVERTER_TRIP.to_string(),
            category: C::Inverter,
            trigger: T::Manual,
            description: "Inverter protective trip — output zeroed".into(),
            probability: 0.0,
        },
        FaultDef {
            id: well_known::COMM_TIMEOUT.to_string(),
            category: C::Communication,
            trigger: T::Manual,
            description: "Communication bus timeout".into(),
            probability: 0.0,
        },
        FaultDef {
            id: well_known::SENSOR_DRIFT.to_string(),
            category: C::Sensor,
            trigger: T::Manual,
            description: "Sensor reading drift detected".into(),
            probability: 0.0,
        },
        FaultDef {
            id: well_known::COLD_BATTERY.to_string(),
            category: C::Battery,
            trigger: T::Manual,
            description: "Battery temperature forced to 0 °C — reduced performance".into(),
            probability: 0.0,
        },
    ]
}

// ---------------------------------------------------------------------------
// FaultEngine — device model that applies fault effects
// ---------------------------------------------------------------------------

/// A device model that reads `state.active_faults` and applies consequences.
///
/// Must be registered **after** the inverter engine but **before** the battery
/// engine so that fault effects (e.g. zeroing inverter output) are reflected
/// in the battery SOC calculation.
///
/// Recommended order: Solar → Load → Inverter → **Faults** → Battery.
pub struct FaultEngine {
    /// Faults that were active last tick — used to detect cleared faults.
    prev_faults: Vec<String>,
}

impl FaultEngine {
    pub fn new() -> Self {
        Self {
            prev_faults: Vec::new(),
        }
    }
}

impl Default for FaultEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl sim_models::DeviceModel for FaultEngine {
    fn update(&mut self, _ctx: &sim_models::TickContext, state: &mut sim_models::PlantState) {
        // Collect active fault IDs first to avoid borrow conflicts
        let active: Vec<String> = state.active_faults.clone();

        // Detect cleared faults and restore state
        for prev in &self.prev_faults {
            if !active.contains(prev) {
                match prev.as_str() {
                    well_known::GRID_LOSS => {
                        state.grid.connected = true;
                    }
                    well_known::INVERTER_TRIP | well_known::BATTERY_OVER_TEMP => {
                        // Inverter/battery will naturally resume on next tick
                    }
                    well_known::COLD_BATTERY => {
                        // Restore battery temperature to a normal operating value
                        for b in &mut state.batteries {
                            b.temperature_celsius = 37.0;
                        }
                        state.sync_battery_from_vec();
                    }
                    _ => {}
                }
            }
        }

        // Apply active fault effects
        for fault_id in &active {
            match fault_id.as_str() {
                well_known::GRID_LOSS => {
                    state.grid.connected = false;
                    state.grid.power_w = 0.0;
                }
                well_known::INVERTER_TRIP => {
                    state.inverter.ac_power_w = 0.0;
                    for b in &mut state.batteries {
                        b.power_kw = 0.0;
                    }
                    state.battery.power_kw = 0.0;
                }
                well_known::BATTERY_OVER_TEMP => {
                    // Block charging on all battery modules
                    for b in &mut state.batteries {
                        if b.power_kw > 0.0 {
                            b.power_kw = 0.0;
                        }
                    }
                    state.sync_battery_from_vec();
                }
                well_known::COLD_BATTERY => {
                    // Force all battery modules to 0 °C
                    for b in &mut state.batteries {
                        b.temperature_celsius = 0.0;
                    }
                    state.sync_battery_from_vec();
                }
                well_known::COMM_TIMEOUT | well_known::SENSOR_DRIFT => {}
                _ => {}
            }
        }

        // Snapshot current faults for next tick
        self.prev_faults = active;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sim_models::{DeviceModel, PlantState, TickContext};

    fn ts(hour: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    #[test]
    fn catalogue_contains_all_well_known() {
        let cat = default_fault_catalogue();
        let ids: Vec<&str> = cat.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&well_known::GRID_LOSS));
        assert!(ids.contains(&well_known::BATTERY_OVER_TEMP));
        assert!(ids.contains(&well_known::INVERTER_TRIP));
    }

    #[test]
    fn grid_loss_disconnects_grid() {
        let mut state = PlantState::new(ts(12));
        state.grid.connected = true;
        state.active_faults.push(well_known::GRID_LOSS.to_string());

        let mut engine = FaultEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert!(!state.grid.connected);
        assert_eq!(state.grid.power_w, 0.0);
    }

    #[test]
    fn inverter_trip_zeros_output() {
        let mut state = PlantState::new(ts(12));
        state.inverter.ac_power_w = 3000.0;
        state.batteries[0].power_kw = 2.0;
        state.sync_battery_from_vec();
        state
            .active_faults
            .push(well_known::INVERTER_TRIP.to_string());

        let mut engine = FaultEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert_eq!(state.inverter.ac_power_w, 0.0);
        assert_eq!(state.total_battery_power_kw(), 0.0);
    }

    #[test]
    fn battery_over_temp_blocks_charging() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].power_kw = 3.0; // charging
        state.sync_battery_from_vec();
        state
            .active_faults
            .push(well_known::BATTERY_OVER_TEMP.to_string());

        let mut engine = FaultEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert_eq!(state.total_battery_power_kw(), 0.0);
    }

    #[test]
    fn battery_over_temp_allows_discharging() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].power_kw = -2.0; // discharging
        state.sync_battery_from_vec();
        state
            .active_faults
            .push(well_known::BATTERY_OVER_TEMP.to_string());

        let mut engine = FaultEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert_eq!(state.total_battery_power_kw(), -2.0);
    }

    #[test]
    fn grid_restore_clears_grid_loss() {
        let mut state = PlantState::new(ts(12));
        state.grid.connected = true;
        state.active_faults.push(well_known::GRID_LOSS.to_string());

        let mut engine = FaultEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        // Tick 1: fault active, grid disconnected
        engine.update(&ctx, &mut state);
        assert!(!state.grid.connected);

        // Tick 2: fault cleared
        state.active_faults.clear();
        engine.update(&ctx, &mut state);
        assert!(state.grid.connected);
    }

    #[test]
    fn cold_battery_forces_temp_to_zero() {
        let mut state = PlantState::new(ts(12));
        // Set initial temps above 0
        state.batteries[0].temperature_celsius = 30.0;
        state.batteries[0].power_kw = 0.0;
        state.sync_battery_from_vec();
        state
            .active_faults
            .push(well_known::COLD_BATTERY.to_string());

        let mut engine = FaultEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert_eq!(
            state.batteries[0].temperature_celsius, 0.0,
            "Cold battery should force temp to 0"
        );
    }

    #[test]
    fn cold_battery_affects_all_modules() {
        let mut state = PlantState::new(ts(12));
        // Add a second battery
        state.batteries.push(sim_models::BatteryState {
            temperature_celsius: 28.0,
            ..Default::default()
        });
        state.sync_battery_from_vec();

        state.batteries[0].temperature_celsius = 30.0;
        state.batteries[1].temperature_celsius = 28.0;
        state.sync_battery_from_vec();

        state
            .active_faults
            .push(well_known::COLD_BATTERY.to_string());

        let mut engine = FaultEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert_eq!(state.batteries[0].temperature_celsius, 0.0);
        assert_eq!(state.batteries[1].temperature_celsius, 0.0);
        assert_eq!(state.battery_temperature_celsius(), 0.0);
    }

    #[test]
    fn cold_battery_cleared_restores_naturally() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].temperature_celsius = 30.0;
        state.sync_battery_from_vec();
        state
            .active_faults
            .push(well_known::COLD_BATTERY.to_string());

        let mut engine = FaultEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };

        // Tick 1: fault active, temp forced to 0
        engine.update(&ctx, &mut state);
        assert_eq!(state.batteries[0].temperature_celsius, 0.0);

        // Tick 2: fault cleared — engine restores temp to 37 °C.
        state.active_faults.clear();
        engine.update(&ctx, &mut state);
        assert_eq!(state.batteries[0].temperature_celsius, 37.0);
    }

    #[test]
    fn catalogue_includes_cold_battery() {
        let cat = default_fault_catalogue();
        let ids: Vec<&str> = cat.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&well_known::COLD_BATTERY));
    }
}
