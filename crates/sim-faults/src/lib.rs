//! Fault framework: categories, triggers, lifecycle.
//!
//! Faults may be manual, scheduled, or randomised.
//! Categories: Communication, Electrical, Sensor, Battery, Inverter.

use serde::{Deserialize, Serialize};

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
    /// Injected manually by user / test.
    Manual,
    /// Fires at a specific simulation time.
    Scheduled,
    /// Fires probabilistically.
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
}

/// Well-known fault IDs used across crates.
pub mod well_known {
    pub const GRID_LOSS: &str = "grid_loss";
    pub const BATTERY_OVER_TEMP: &str = "battery_over_temp";
    pub const INVERTER_TRIP: &str = "inverter_trip";
    pub const COMM_TIMEOUT: &str = "comm_timeout";
    pub const SENSOR_DRIFT: &str = "sensor_drift";
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
        },
        FaultDef {
            id: well_known::BATTERY_OVER_TEMP.to_string(),
            category: C::Battery,
            trigger: T::Manual,
            description: "Battery thermal limit exceeded".into(),
        },
        FaultDef {
            id: well_known::INVERTER_TRIP.to_string(),
            category: C::Inverter,
            trigger: T::Manual,
            description: "Inverter protective trip".into(),
        },
        FaultDef {
            id: well_known::COMM_TIMEOUT.to_string(),
            category: C::Communication,
            trigger: T::Manual,
            description: "Communication bus timeout".into(),
        },
        FaultDef {
            id: well_known::SENSOR_DRIFT.to_string(),
            category: C::Sensor,
            trigger: T::Manual,
            description: "Sensor reading drift detected".into(),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalogue_contains_all_well_known() {
        let cat = default_fault_catalogue();
        let ids: Vec<&str> = cat.iter().map(|f| f.id.as_str()).collect();
        assert!(ids.contains(&well_known::GRID_LOSS));
        assert!(ids.contains(&well_known::BATTERY_OVER_TEMP));
    }
}
