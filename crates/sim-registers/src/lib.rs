//! Register catalogue and mapping layer.
//!
//! Registers are projections of [`PlantState`](sim_models::PlantState).
//! Each register definition specifies address, type, scaling, and R/W capability.

use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Register definition
// ---------------------------------------------------------------------------

/// Data type stored in a register.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegisterType {
    U16,
    S16,
    U32,
    S32,
    F32,
}

/// Read / Write capability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Access {
    ReadOnly,
    ReadWrite,
}

/// A single register definition in the catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterDef {
    /// Modbus holding register address.
    pub address: u16,
    /// Human-readable name.
    pub name: String,
    /// Category grouping.
    pub category: RegisterCategory,
    pub typ: RegisterType,
    /// Multiplier to convert raw value → engineering units.
    pub scaling_factor: f64,
    pub access: Access,
}

/// Register grouping categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegisterCategory {
    Inverter,
    Battery,
    PV,
    Grid,
    Configuration,
    Schedules,
}

// ---------------------------------------------------------------------------
// Register store — the live register bank
// ---------------------------------------------------------------------------

/// The live register bank. Maps address → raw u16 value (simplified for MVP).
#[derive(Debug, Clone)]
pub struct RegisterStore {
    values: std::collections::HashMap<u16, u16>,
    defs: Vec<RegisterDef>,
}

impl RegisterStore {
    /// Create a store pre-populated from the given register definitions.
    pub fn new(defs: Vec<RegisterDef>) -> Self {
        let values = defs.iter().map(|d| (d.address, 0u16)).collect();
        Self { values, defs }
    }

    /// Read a register value.
    pub fn read(&self, address: u16) -> Option<u16> {
        self.values.get(&address).copied()
    }

    /// Write a register value (respects access control).
    pub fn write(&mut self, address: u16, value: u16) -> bool {
        if let Some(def) = self.defs.iter().find(|d| d.address == address) {
            if def.access == Access::ReadWrite {
                self.values.insert(address, value);
                return true;
            }
        }
        false
    }

    /// Update all register values from plant state.
    pub fn project_from_state(&mut self, state: &sim_models::PlantState) {
        // MVP: hand-code key projections. Phase 3 will auto-generate from real GivEnergy map.
        for def in &self.defs {
            let raw = match def.name.as_str() {
                "inverter_ac_power" => state.inverter.ac_power_w as u16,
                "battery_soc" => state.battery.soc_percent as u16,
                "pv_generation" => state.solar.generation_w as u16,
                "grid_power" => {
                    if state.grid.power_w >= 0.0 {
                        state.grid.power_w as u16
                    } else {
                        0
                    }
                }
                "load_power" => state.load.demand_w as u16,
                "inverter_mode" => match state.inverter.mode {
                    sim_models::InverterMode::Normal => 0,
                    sim_models::InverterMode::Eco => 1,
                    sim_models::InverterMode::ForceCharge => 2,
                    sim_models::InverterMode::ForceDischarge => 3,
                    sim_models::InverterMode::ExportLimit => 4,
                },
                _ => continue,
            };
            self.values.insert(def.address, raw);
        }
    }

    /// Iterator over all definitions.
    pub fn definitions(&self) -> &[RegisterDef] {
        &self.defs
    }
}

/// Return the default MVP register catalogue.
pub fn default_register_catalogue() -> Vec<RegisterDef> {
    use RegisterCategory as C;
    use RegisterType as T;
    use Access::*;

    vec![
        RegisterDef {
            address: 100,
            name: "inverter_mode".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
        },
        RegisterDef {
            address: 101,
            name: "inverter_ac_power".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
        },
        RegisterDef {
            address: 200,
            name: "battery_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
        },
        RegisterDef {
            address: 300,
            name: "pv_generation".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
        },
        RegisterDef {
            address: 400,
            name: "grid_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
        },
        RegisterDef {
            address: 401,
            name: "load_power".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_models::PlantState;
    use chrono::NaiveDate;

    fn test_ts() -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 1).unwrap().and_hms_opt(12, 0, 0).unwrap()
    }

    #[test]
    fn project_maps_state_to_registers() {
        let mut state = PlantState::new(test_ts());
        state.solar.generation_w = 3500.0;
        state.battery.soc_percent = 75.0;

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        assert_eq!(store.read(300), Some(3500)); // pv_generation
        assert_eq!(store.read(200), Some(75));   // battery_soc
    }

    #[test]
    fn write_respects_access_control() {
        let mut store = RegisterStore::new(default_register_catalogue());
        assert!(store.write(100, 2));  // inverter_mode = ReadWrite
        assert!(!store.write(200, 50)); // battery_soc = ReadOnly
    }
}
