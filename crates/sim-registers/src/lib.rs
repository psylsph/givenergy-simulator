//! Register catalogue and mapping layer.
//!
//! Registers are projections of [`PlantState`](sim_models::PlantState).
//! Each register definition specifies address, type, scaling, and R/W capability.
//!
//! Two register sets are maintained:
//! - **GivEnergy-native** addresses (IR 0-59, HR 0-119) for compatibility
//!   with real GivEnergy client apps
//! - **Simulator-internal** addresses (100+, 200+, etc.) for the Tauri GUI
//!
//! Input registers (fn 0x04) and holding registers (fn 0x03/0x06) are stored
//! in separate ranges to avoid address collisions:
//! - Input registers: keys = address as-is (0-9999)
//! - Holding registers: keys = address + 10000 (10000-19999)

use chrono::{Datelike, Timelike};
use serde::{Deserialize, Serialize};

/// Offset for holding register keys in the flat store.
const HOLDING_OFFSET: u32 = 10000;

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

/// Whether this register lives in the input or holding register space.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum RegisterSpace {
    /// Input register (fn 0x04) — read-only telemetry.
    Input,
    /// Holding register (fn 0x03/0x06) — read/write config & telemetry.
    Holding,
}

/// A single register definition in the catalogue.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterDef {
    /// Modbus register address.
    pub address: u16,
    /// Human-readable name.
    pub name: String,
    /// Category grouping.
    pub category: RegisterCategory,
    pub typ: RegisterType,
    /// Multiplier to convert raw value → engineering units.
    pub scaling_factor: f64,
    pub access: Access,
    /// Input vs holding register space.
    pub space: RegisterSpace,
}

impl RegisterDef {
    /// Compute the internal storage key for this register.
    pub fn store_key(&self) -> u32 {
        match self.space {
            RegisterSpace::Input => self.address as u32,
            RegisterSpace::Holding => self.address as u32 + HOLDING_OFFSET,
        }
    }
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

/// The live register bank. Maps composite key → raw u16 value.
#[derive(Debug, Clone)]
pub struct RegisterStore {
    values: std::collections::HashMap<u32, u16>,
    defs: Vec<RegisterDef>,
}

impl RegisterStore {
    /// Create a store pre-populated from the given register definitions.
    pub fn new(defs: Vec<RegisterDef>) -> Self {
        let values = defs.iter().map(|d| (d.store_key(), 0u16)).collect();
        Self { values, defs }
    }

    /// Read a register by address and space.
    pub fn read_by_space(&self, address: u16, space: RegisterSpace) -> Option<u16> {
        let key = match space {
            RegisterSpace::Input => address as u32,
            RegisterSpace::Holding => address as u32 + HOLDING_OFFSET,
        };
        self.values.get(&key).copied()
    }

    /// Read a holding register by address (backward-compat helper).
    pub fn read(&self, address: u16) -> Option<u16> {
        self.read_by_space(address, RegisterSpace::Holding)
            .or_else(|| self.read_by_space(address, RegisterSpace::Input))
    }

    /// Write a holding register value by address (respects access control).
    pub fn write(&mut self, address: u16, value: u16) -> bool {
        let key = address as u32 + HOLDING_OFFSET;
        if let Some(def) = self.defs.iter().find(|d| d.store_key() == key)
            && def.access == Access::ReadWrite
        {
            self.values.insert(key, value);
            return true;
        }
        false
    }

    /// Update all register values from plant state.
    pub fn project_from_state(&mut self, state: &sim_models::PlantState) {
        let seeded_state = if state.energy_totals.is_all_zero() {
            let mut seeded = state.clone();
            seeded.energy_totals = sim_models::EnergyTotals::non_zero_test_fixture();
            Some(seeded)
        } else {
            None
        };
        let state = seeded_state.as_ref().unwrap_or(state);

        let u32_words = |engineering: f64, scaling: f64| -> (u16, u16) {
            let raw = if scaling > 0.0 {
                (engineering / scaling).max(0.0).round() as u32
            } else {
                engineering.max(0.0).round() as u32
            };
            ((raw >> 16) as u16, (raw & 0xFFFF) as u16)
        };
        let i32_words = |engineering: f64, scaling: f64| -> (u16, u16) {
            let raw = if scaling > 0.0 {
                (engineering / scaling).round() as i32
            } else {
                engineering.round() as i32
            } as u32;
            ((raw >> 16) as u16, (raw & 0xFFFF) as u16)
        };
        let clamp_i16_word = |watts: f64| -> u16 { watts.clamp(-32768.0, 32767.0) as i16 as u16 };
        let fault_code = || -> u32 {
            let mut code = 0u32;
            for fault in &state.active_faults {
                match fault.as_str() {
                    // Align to common GivEnergy-style fault meanings where possible.
                    "grid_loss" => code |= 1 << 8,     // No Utility
                    "inverter_trip" => code |= 1 << 9, // relay/consistent inverter fault bucket
                    "battery_over_temp" => code |= 1 << 1, // generic battery/charger warning bucket
                    "comm_timeout" => code |= 1 << 25, // ARM/DSP comms bucket
                    "sensor_drift" => code |= 1 << 0,
                    _ => code |= 1,
                }
            }
            code
        };

        for def in &self.defs {
            let key = def.store_key();

            let engineering: Option<f64> = match def.name.as_str() {
                // ================================================================
                // GivEnergy-native Input Registers (IR 0-59)
                // ================================================================

                // IR 0: Inverter status
                "ge_ir_status" => {
                    self.values.insert(key, 1); // always normal
                    continue;
                }
                // IR 1: PV1 voltage (×0.1 V)
                "ge_ir_pv1_voltage" => Some(if state.solar.pv1_w > 0.0 { 350.0 } else { 0.0 }),
                // IR 2: PV2 voltage (×0.1 V)
                // Show voltage whenever PV2 is configured (even not generating)
                "ge_ir_pv2_voltage" => Some(
                    if state.solar.pv2_w > 0.0 || state.config.pv2_peak_watts > 0.0 {
                        350.0
                    } else {
                        0.0
                    },
                ),
                // IR 3-4: DC bus voltages (×0.1 V)
                "ge_ir_p_bus_voltage" => Some(if state.solar.generation_w > 0.0 {
                    380.0
                } else {
                    0.0
                }),
                "ge_ir_n_bus_voltage" => Some(0.0),
                // IR 5: Grid voltage (×0.1 V)
                "ge_ir_grid_voltage" => Some(240.0),
                // IR 6-7: Battery throughput total (uint32, ×0.1 kWh)
                "ge_ir_battery_throughput_high" | "ge_ir_battery_throughput_low" => {
                    let total: f64 = state.batteries.iter().map(|b| b.throughput_kwh).sum();
                    let (hi, lo) = u32_words(total, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 8: PV1 current (×0.1 A) — per-string: pv1_w / 350V
                "ge_ir_pv1_current" => Some(state.solar.pv1_w / 350.0),
                // IR 9: PV2 current (×0.1 A)
                "ge_ir_pv2_current" => Some(state.solar.pv2_w / 350.0),
                // IR 10: AC current phase 1 (×0.1 A)
                "ge_ir_ac_current" => Some(state.inverter.ac_power_w.abs() / 240.0),
                // IR 11-12: PV total lifetime (uint32, ×0.1 kWh)
                "ge_ir_pv_total_high" | "ge_ir_pv_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 13: Grid frequency (×0.01 Hz)
                "ge_ir_grid_frequency" => Some(50.0),
                // IR 14-16: status/bus/power-factor telemetry
                "ge_ir_charge_status" => Some(if state.total_battery_power_kw() > 0.01 {
                    1.0
                } else if state.total_battery_power_kw() < -0.01 {
                    2.0
                } else {
                    0.0
                }),
                "ge_ir_high_bus_voltage" => Some(if state.solar.generation_w > 0.0 {
                    380.0
                } else {
                    0.0
                }),
                "ge_ir_inverter_output_pf" => Some(10000.0),
                // IR 17: PV1 energy today (×0.1 kWh) — proportional to power split
                "ge_ir_pv1_energy_today" => {
                    let total = state.energy_totals.solar_generation_kwh;
                    if state.config.pv2_peak_watts > 0.0 {
                        Some(total * 0.45)
                    } else {
                        Some(total)
                    }
                }
                // IR 18: PV1 power (W)
                "ge_ir_pv1_power" => Some(state.solar.pv1_w),
                // IR 19: PV2 energy today (×0.1 kWh)
                "ge_ir_pv2_energy_today" => {
                    let total = state.energy_totals.solar_generation_kwh;
                    if state.config.pv2_peak_watts > 0.0 {
                        Some(total * 0.55)
                    } else {
                        Some(0.0)
                    }
                }
                // IR 20: PV2 power (W)
                "ge_ir_pv2_power" => Some(state.solar.pv2_w),
                // IR 21-22: Grid export total (uint32, ×0.1 kWh)
                "ge_ir_grid_export_total_high" | "ge_ir_grid_export_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.grid_export_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 23-24: solar diverter energy and inverter AC-terminal real power
                "ge_ir_solar_diverter_energy" => Some(0.0),
                "ge_ir_inverter_terminal_power" => {
                    self.values
                        .insert(key, clamp_i16_word(state.inverter.ac_power_w));
                    continue;
                }
                // IR 25: Export energy today (×0.1 kWh)
                "ge_ir_today_export_energy" => Some(state.energy_totals.grid_export_kwh),
                // IR 26: Import energy today (×0.1 kWh)
                "ge_ir_today_import_energy" => Some(state.energy_totals.grid_import_kwh),
                // IR 27-28: inverter input/AC-charge total (uint32, ×0.1 kWh)
                "ge_ir_inverter_input_total_high" | "ge_ir_inverter_input_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.ac_charge_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 29: discharge energy this year (×0.1 kWh). Simulator has no yearly reset, so use cumulative bucket.
                "ge_ir_discharge_year" => Some(state.energy_totals.battery_discharge_kwh),
                // IR 30: Grid power (signed, +exporting/-importing per GE convention)
                "ge_ir_grid_power" => {
                    // Negate: our internal has positive=import, GE wire has positive=export
                    self.values.insert(key, clamp_i16_word(-state.grid.power_w));
                    continue;
                }
                // IR 31: EPS backup power
                "ge_ir_eps_backup_power" => Some(if state.enable_eps {
                    state.load.demand_w
                } else {
                    0.0
                }),
                // IR 32-33: Grid import total (uint32, ×0.1 kWh)
                "ge_ir_grid_import_total_high" | "ge_ir_grid_import_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.grid_import_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 35: AC charge today (×0.1 kWh)
                "ge_ir_ac_charge_today" => {
                    let raw = (state.energy_totals.ac_charge_kwh * 10.0).round() as u16;
                    self.values.insert(key, raw);
                    continue;
                }
                // IR 36: Battery charge today (×0.1 kWh)
                "ge_ir_today_charge_energy" => Some(state.energy_totals.battery_charge_kwh),
                // IR 37: Battery discharge today (×0.1 kWh)
                "ge_ir_today_discharge_energy" => Some(state.energy_totals.battery_discharge_kwh),
                // IR 38: countdown
                "ge_ir_countdown" => Some(0.0),
                // IR 39-40: inverter fault bitmask (uint32)
                "ge_ir_fault_code_high" | "ge_ir_fault_code_low" => {
                    let code = fault_code();
                    self.values.insert(
                        key,
                        if def.name.ends_with("_high") {
                            (code >> 16) as u16
                        } else {
                            (code & 0xFFFF) as u16
                        },
                    );
                    continue;
                }
                // IR 41: Inverter temperature (×0.1 °C)
                "ge_ir_inverter_temperature" => Some(state.inverter.temperature_celsius),
                // IR 42-43: house load and inverter terminal apparent power
                "ge_ir_load_demand" => Some(state.load.demand_w),
                "ge_ir_grid_apparent" => Some(state.inverter.ac_power_w.abs()),
                // IR 44: PV generation today (×0.1 kWh)
                "ge_ir_pv_generation_today" => Some(state.energy_totals.solar_generation_kwh),
                // IR 45-46: PV generation total (uint32, ×0.1 kWh)
                "ge_ir_pv_generation_total_high" | "ge_ir_pv_generation_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 47-48: powered runtime hours (uint32)
                "ge_ir_work_time_total_high" | "ge_ir_work_time_total_low" => {
                    let (hi, lo) = u32_words(state.inverter.work_time_hours, 1.0);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 49: system/work mode (2=on-grid, 1=off-grid, 3=fault)
                "ge_ir_system_mode" => Some(if !state.active_faults.is_empty() {
                    3.0
                } else if state.grid.connected {
                    2.0
                } else {
                    1.0
                }),
                // IR 50: Battery voltage (×0.01 V)
                "ge_ir_battery_voltage" => {
                    let soc = state.aggregate_soc();
                    Some(44.0 + soc * 0.08)
                }
                // IR 51: Battery current (signed, ×0.01 A)
                "ge_ir_battery_current" => {
                    // Same convention as battery power: negate so positive = discharging
                    let amps = -(state.total_battery_power_kw() * 1000.0 / 48.0);
                    self.values.insert(key, amps as i16 as u16);
                    continue;
                }
                // IR 52: Battery power (signed, +charging)
                "ge_ir_battery_power" => {
                    // GivEnergy convention: raw positive = discharging (power OUT of battery)
                    // Our internal: total_battery_power_kw positive = charging
                    // So we negate for the wire format.
                    let w = -(state.total_battery_power_kw() * 1000.0);
                    self.values.insert(key, w as i16 as u16);
                    continue;
                }
                // IR 53-55: EPS/grid output voltage/frequency and charger temperature
                "ge_ir_eps_voltage" => Some(if state.enable_eps { 240.0 } else { 0.0 }),
                "ge_ir_eps_frequency" => Some(if state.enable_eps { 50.0 } else { 0.0 }),
                "ge_ir_charger_temperature" => Some(state.inverter.temperature_celsius),
                // IR 56: Battery temperature (×0.1 °C)
                "ge_ir_battery_temperature" => Some(state.battery_temperature_celsius()),
                // IR 57-58: warning and inverter AC-terminal current
                "ge_ir_charger_warning_code" => Some(
                    if state.active_faults.iter().any(|f| f == "battery_over_temp") {
                        1.0
                    } else {
                        0.0
                    },
                ),
                "ge_ir_grid_port_current" => Some(state.inverter.ac_power_w.abs() / 240.0),
                // IR 59: Battery SOC (%)
                "ge_ir_battery_soc" => Some(state.aggregate_soc()),
                // IR 180-183: model-dependent battery energy alt sources
                "ge_ir_battery_discharge_total_alt1" => {
                    Some(state.energy_totals.battery_discharge_kwh)
                }
                "ge_ir_battery_charge_total_alt1" => Some(state.energy_totals.battery_charge_kwh),
                "ge_ir_battery_discharge_today_alt2" => {
                    Some(state.energy_totals.battery_discharge_kwh)
                }
                "ge_ir_battery_charge_today_alt2" => Some(state.energy_totals.battery_charge_kwh),
                // IR 247-248: Gen3 combined generation (uint32 W)
                "ge_ir_combined_generation_high" | "ge_ir_combined_generation_low" => {
                    let (hi, lo) = u32_words(state.solar.generation_w, 1.0);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }

                // ================================================================
                // CT Clamp Meter Input Registers (IR 60-89)
                // ================================================================
                "meter_v_phase_1" => Some(240.0),
                "meter_v_phase_2" => Some(0.0),
                "meter_v_phase_3" => Some(0.0),
                "meter_i_phase_1" => {
                    let i = state.grid.power_w.abs() / 240.0;
                    self.values.insert(key, (i * 100.0) as u16);
                    continue;
                }
                "meter_i_phase_2" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_i_phase_3" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_i_ln" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_i_total" => {
                    let i = state.grid.power_w.abs() / 240.0;
                    self.values.insert(key, (i * 100.0) as u16);
                    continue;
                }
                "meter_p_active_phase_1" => {
                    // GivEnergy convention: +W = import, −W = export
                    let clamped = state.grid.power_w.clamp(-32768.0, 32767.0);
                    self.values.insert(key, clamped as i16 as u16);
                    continue;
                }
                "meter_p_active_phase_2" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_p_active_phase_3" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_p_active_total" => {
                    let clamped = state.grid.power_w.clamp(-32768.0, 32767.0);
                    self.values.insert(key, clamped as i16 as u16);
                    continue;
                }
                "meter_p_reactive_phase_1"
                | "meter_p_reactive_phase_2"
                | "meter_p_reactive_phase_3"
                | "meter_p_reactive_total" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_p_apparent_phase_1" => {
                    self.values.insert(key, state.grid.power_w.abs() as u16);
                    continue;
                }
                "meter_p_apparent_phase_2" | "meter_p_apparent_phase_3" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_p_apparent_total" => {
                    self.values.insert(key, state.grid.power_w.abs() as u16);
                    continue;
                }
                "meter_pf_phase_1" => {
                    if state.grid.power_w.abs() > 1.0 {
                        self.values.insert(key, 1000i16 as u16);
                    } else {
                        self.values.insert(key, 0);
                    }
                    continue;
                }
                "meter_pf_phase_2" | "meter_pf_phase_3" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_pf_total" => {
                    if state.grid.power_w.abs() > 1.0 {
                        self.values.insert(key, 1000i16 as u16);
                    } else {
                        self.values.insert(key, 0);
                    }
                    continue;
                }
                "meter_frequency" => Some(50.0),
                "meter_e_import_active" => Some(123.4), // hardcoded lifetime CT import total → raw 1234 at ×0.1
                "meter_e_import_reactive" => Some(0.0),
                "meter_e_export_active" => Some(432.1), // hardcoded lifetime CT export total → raw 4321 at ×0.1
                "meter_e_export_reactive" => Some(0.0),
                "meter_reserved" => Some(0.0),

                // ================================================================
                // EMS Input Registers (IR 2040-2095) — plant status & telemetry
                // ================================================================
                "ems_ir_status" => Some(1.0),
                "ems_ir_inverter_count" => Some(state.batteries.len() as f64),
                "ems_ir_inverter_1_soc" => Some(state.aggregate_soc()),
                "ems_ir_inverter_1_power" => {
                    let clamped =
                        (state.total_battery_power_kw() * 1000.0).clamp(-32768.0, 32767.0) as i16;
                    self.values.insert(key, clamped as u16);
                    continue;
                }
                "ems_ir_inverter_1_temp" => Some(state.inverter.temperature_celsius),
                "ems_ir_grid_meter_power" => {
                    let clamped = state.grid.power_w.clamp(-32768.0, 32767.0) as i16;
                    self.values.insert(key, clamped as u16);
                    continue;
                }
                "ems_ir_total_battery_power" => {
                    let clamped =
                        (state.total_battery_power_kw() * 1000.0).clamp(-32768.0, 32767.0) as i16;
                    self.values.insert(key, clamped as u16);
                    continue;
                }
                "ems_ir_remaining_battery_wh" => {
                    let wh = (state.aggregate_soc() / 100.0
                        * state.total_battery_capacity()
                        * 1000.0) as u16;
                    self.values.insert(key, wh);
                    continue;
                }
                // Catch-all for other EMS IR registers: default to 0
                name if name.starts_with("ems_ir_") => Some(0.0),

                // ================================================================
                // GivEnergy-native Holding Registers (HR 0-119)
                // ================================================================

                // HR 0: Device type
                "ge_hr_device_type" => {
                    // Encode inverter type as DTC hex code.
                    // NOTE: 0x2001 is a FAMILY code shared by Gen1/Gen2/Gen3 hybrids.
                    // Disambiguation happens via HR(21) ARM firmware century:
                    //   century 2 → Gen1, century 3 → Gen3, century 8/9 → Gen2.
                    let dtc: u16 = match state.config.inverter_type.as_str() {
                        "Gen1Hybrid" | "Gen2Hybrid" | "Gen3Hybrid" => 0x2001,
                        "Hybrid4600" => 0x2002,
                        "Hybrid3600" => 0x2003,
                        "Polar5kW" => 0x2101,
                        "Polar4600" => 0x2102,
                        "Polar3600" => 0x2103,
                        "Polar6kW" => 0x2104,
                        "Polar7kW" => 0x2105,
                        "Gen3Hybrid8kW" => 0x2101,
                        "Gen3Hybrid10kW" => 0x2102,
                        "Polar8kW" => 0x2106,
                        "Gen3Plus6kW" => 0x2201,
                        "Gen3Plus4600" => 0x2202,
                        "Gen3Plus3600" => 0x2203,
                        "Gen3Plus6kW2" => 0x2204,
                        "Gen3Plus7kW" => 0x2205,
                        "Gen3Plus8kW" => 0x2206,
                        "PVInverter5kW" => 0x2301,
                        "PVInverter4600" => 0x2302,
                        "PVInverter3600" => 0x2303,
                        "PVInverter6kW" => 0x2304,
                        "ACCoupled" => 0x3001,
                        "ACCoupled2" => 0x3002,
                        "ThreePhase" => 0x4001,
                        "ThreePhase8kW" => 0x4002,
                        "ThreePhase10kW" => 0x4003,
                        "ThreePhase11kW" => 0x4004,
                        "AIOCommercial" => 0x4101,
                        "EMS" => 0x5001,
                        "EMSCommercial" => 0x5101,
                        "ACThreePhase" => 0x6001,
                        "Gateway12kW" => 0x7001,
                        "AllInOne6" => 0x8001,
                        "AllInOne" => 0x8002,
                        "AllInOne5" => 0x8003,
                        "AIO6kW" => 0x8101,
                        "AIO8kW" => 0x8102,
                        "AIO10kW" => 0x8103,
                        "AIOHybrid6kW" => 0x8201,
                        "AIOHybrid8kW" => 0x8202,
                        "AIOHybrid10kW" => 0x8203,
                        "AIOHybrid12kW" => 0x8204,
                        "Gen4Hybrid6kW" => 0x8304,
                        _ => 0x2001,
                    };
                    self.values.insert(key, dtc);
                    continue;
                }
                // HR 1-3: module and phase/MPPT metadata
                "ge_hr_module_high" | "ge_hr_module_low" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_mppt_phase_count" => {
                    let mppt = if state.config.pv2_peak_watts > 0.0 {
                        2u16
                    } else {
                        1u16
                    };
                    let phases = if state.config.inverter_type.starts_with("ThreePhase")
                        || state.config.inverter_type == "ACThreePhase"
                    {
                        3u16
                    } else {
                        1u16
                    };
                    self.values.insert(key, (mppt << 8) | phases);
                    continue;
                }
                "ge_hr_enable_ammeter" => {
                    self.values.insert(key, 1);
                    continue;
                }
                // HR 8-12: First battery serial number (simulated)
                "ge_hr_first_battery_serial_0" => {
                    self.values.insert(key, (b'B' as u16) << 8 | b'A' as u16);
                    continue;
                }
                "ge_hr_first_battery_serial_1" => {
                    self.values.insert(key, (b'T' as u16) << 8 | b'0' as u16);
                    continue;
                }
                "ge_hr_first_battery_serial_2" => {
                    self.values.insert(key, (b'0' as u16) << 8 | b'0' as u16);
                    continue;
                }
                "ge_hr_first_battery_serial_3" => {
                    self.values.insert(key, (b'0' as u16) << 8 | b'1' as u16);
                    continue;
                }
                "ge_hr_first_battery_serial_4" => {
                    self.values.insert(key, (b' ' as u16) << 8 | b' ' as u16);
                    continue;
                }
                // HR 13-17: Serial number (simulated)
                // ThreePhase11kW uses "ZAAA111111", others use "SIM012345".
                "ge_hr_serial_0" => {
                    let v = if state.config.inverter_type == "ThreePhase11kW" {
                        (b'Z' as u16) << 8 | b'A' as u16
                    } else {
                        (b'S' as u16) << 8 | b'I' as u16
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "ge_hr_serial_1" => {
                    let v = if state.config.inverter_type == "ThreePhase11kW" {
                        (b'A' as u16) << 8 | b'A' as u16
                    } else {
                        (b'M' as u16) << 8 | b'0' as u16
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "ge_hr_serial_2" => {
                    let v = if state.config.inverter_type == "ThreePhase11kW" {
                        (b'1' as u16) << 8 | b'1' as u16
                    } else {
                        (b'0' as u16) << 8 | b'1' as u16
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "ge_hr_serial_3" => {
                    let v = if state.config.inverter_type == "ThreePhase11kW" {
                        (b'1' as u16) << 8 | b'1' as u16
                    } else {
                        (b'2' as u16) << 8 | b'3' as u16
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "ge_hr_serial_4" => {
                    let v = if state.config.inverter_type == "ThreePhase11kW" {
                        (b'1' as u16) << 8 | b'1' as u16
                    } else {
                        (b'4' as u16) << 8 | b'5' as u16
                    };
                    self.values.insert(key, v);
                    continue;
                }
                // HR 18: First battery BMS firmware version
                "ge_hr_first_battery_bms_firmware" => {
                    self.values.insert(key, 100);
                    continue;
                }
                // HR 19: DSP firmware version (user-overridable, defaults
                // to a value typical for the inverter type).
                "ge_hr_dsp_firmware" => {
                    self.values.insert(key, state.inverter.dsp_firmware_version);
                    continue;
                }
                // HR 21: ARM firmware version. The century (fw/100) disambiguates
                // the 0x2001 hybrid family:
                //   2xx → Gen1, 3xx → Gen3, 8xx/9xx → Gen2.
                // If the user has set a non-zero arm_firmware_version override
                // (e.g. via the set_arm_firmware command), honour that instead.
                "ge_hr_arm_firmware" => {
                    let fw = if state.inverter.arm_firmware_version != 0 {
                        state.inverter.arm_firmware_version
                    } else if state.config.inverter_type.starts_with("ThreePhase")
                        || state.config.inverter_type == "ACThreePhase"
                    {
                        612
                    } else {
                        match state.config.inverter_type.as_str() {
                            "Gen1Hybrid" => 252, // century 2 → Gen1
                            "Gen2Hybrid" => 852, // century 8 → Gen2
                            "Gen3Hybrid" => 318, // century 3 → Gen3 (D318-A318 26-Aug-2025)
                            "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 452,
                            _ => 318,
                        }
                    };
                    self.values.insert(key, fw);
                    continue;
                }
                // HR 22-26: USB/variable/power metadata
                "ge_hr_usb_device_inserted" => {
                    self.values.insert(key, 1);
                    continue;
                }
                "ge_hr_select_arm_chip" | "ge_hr_variable_address" | "ge_hr_variable_value" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_grid_port_max_power_output" => Some(state.config.max_ac_watts),
                // HR 29: Battery calibration stage (0=off)
                "ge_hr_battery_calibration_stage" => {
                    self.values
                        .insert(key, state.calibration.stage.as_u8() as u16);
                    continue;
                }
                // HR 20: Enable charge target
                "ge_hr_enable_charge_target" => {
                    self.values.insert(key, state.enable_charge_target as u16);
                    continue;
                }
                // HR 27: Battery power mode (0=export, 1=eco)
                "ge_hr_battery_power_mode" => {
                    let v = match state.inverter.mode_state.effective {
                        sim_models::InverterMode::Eco => 1,
                        _ => 0,
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "ge_hr_enable_60hz_freq_mode" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_modbus_address" => {
                    self.values.insert(key, 0x32);
                    continue;
                }
                // HR 31: Charge slot 2 start (HHMM)
                "ge_hr_charge_slot_2_start" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 32: Charge slot 2 end (HHMM)
                "ge_hr_charge_slot_2_end" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 35-40: System time
                "ge_hr_system_time_year" => {
                    self.values.insert(key, state.timestamp.year() as u16);
                    continue;
                }
                "ge_hr_system_time_month" => {
                    self.values.insert(key, state.timestamp.month() as u16);
                    continue;
                }
                "ge_hr_system_time_day" => {
                    self.values.insert(key, state.timestamp.day() as u16);
                    continue;
                }
                "ge_hr_system_time_hour" => {
                    self.values.insert(key, state.timestamp.hour() as u16);
                    continue;
                }
                "ge_hr_system_time_minute" => {
                    self.values.insert(key, state.timestamp.minute() as u16);
                    continue;
                }
                "ge_hr_system_time_second" => {
                    self.values.insert(key, state.timestamp.second() as u16);
                    continue;
                }
                // HR 33-43 metadata / feature config
                "ge_hr_user_code" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_modbus_version" => Some(1.0),
                "ge_hr_enable_drm_rj45_port" | "ge_hr_enable_reversed_ct_clamp" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_charge_discharge_soc" => {
                    self.values.insert(
                        key,
                        ((state.aggregate_soc().round() as u16) << 8)
                            | (state.min_aggregate_soc().round() as u16),
                    );
                    continue;
                }
                // HR 44: Discharge slot 2 start (HHMM)
                "ge_hr_discharge_slot_2_start" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 45: Discharge slot 2 end (HHMM)
                "ge_hr_discharge_slot_2_end" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 46-54 meter and inverter config
                "ge_hr_bms_firmware_version" => {
                    self.values.insert(key, 100);
                    continue;
                }
                "ge_hr_meter_type" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_enable_reversed_115_meter" | "ge_hr_enable_reversed_418_meter" => {
                    self.values.insert(key, 0);
                    continue;
                }
                // HR 50: Active power rate (%)
                "ge_hr_active_power_rate" => Some(state.active_power_rate_percent),
                "ge_hr_reactive_power_rate" => Some(100.0),
                "ge_hr_power_factor" => {
                    self.values.insert(key, 10000);
                    continue;
                }
                "ge_hr_enable_inverter_flags" => {
                    self.values.insert(key, 0x0101);
                    continue;
                }
                "ge_hr_battery_type" => {
                    self.values.insert(key, 1);
                    continue;
                }
                // HR 55: Battery capacity in Ah (total system)
                // kWh = Ah * nominal_voltage / 1000 → Ah = kWh * 1000 / V
                "ge_hr_battery_capacity_ah" => {
                    let nom_v = if state.config.inverter_type.starts_with("ThreePhase")
                        || state.config.inverter_type == "ACThreePhase"
                    {
                        76.8
                    } else {
                        match state.config.inverter_type.as_str() {
                            "AllInOne6" | "AllInOne" | "AllInOne5" | "AIO8kW" | "AIO10kW" => 307.0,
                            _ => 51.2,
                        }
                    };
                    let total_kwh = state.total_battery_capacity();
                    let ah = (total_kwh * 1000.0 / nom_v).round() as u16;
                    self.values.insert(key, ah);
                    continue;
                }
                // HR 56: Discharge slot 1 start (HHMM)
                "ge_hr_discharge_slot_1_start" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 57: Discharge slot 1 end (HHMM)
                "ge_hr_discharge_slot_1_end" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 58-60 low-level config
                "ge_hr_enable_auto_judge_battery_type" => {
                    self.values.insert(key, 1);
                    continue;
                }
                // HR 59: Enable discharge
                "ge_hr_enable_discharge" => {
                    let v = match state.inverter.mode_state.effective {
                        sim_models::InverterMode::ForceDischarge => 1,
                        _ => 0,
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "ge_hr_pv_start_voltage" => Some(120.0),
                "ge_hr_start_countdown_timer" | "ge_hr_restart_delay_time" => {
                    self.values.insert(key, 0);
                    continue;
                }
                // HR 94: Charge slot 1 start (HHMM)
                "ge_hr_charge_slot_1_start" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 95: Charge slot 1 end (HHMM)
                "ge_hr_charge_slot_1_end" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 96: Enable charge
                "ge_hr_enable_charge" => {
                    let v = match state.inverter.mode_state.effective {
                        sim_models::InverterMode::ForceCharge => 1,
                        _ => 0,
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "ge_hr_battery_low_voltage_protection_limit" => Some(40.0),
                "ge_hr_battery_high_voltage_protection_limit" => Some(60.0),
                "ge_hr_battery_voltage_adjust" => Some(0.0),
                "ge_hr_battery_low_force_charge_time" => Some(0.0),
                "ge_hr_enable_bms_read" => {
                    self.values.insert(key, 1);
                    continue;
                }
                // HR 110: Battery SOC reserve (%)
                "ge_hr_battery_soc_reserve" => Some(state.min_aggregate_soc()),
                // HR 111: Battery charge limit (%)
                "ge_hr_battery_charge_limit" => Some(state.battery_charge_limit_percent),
                // HR 112: Battery discharge limit (%)
                "ge_hr_battery_discharge_limit" => Some(state.battery_discharge_limit_percent),
                // HR 313/314: AC-coupled battery charge/discharge limit (%)
                "ge_hr_battery_charge_limit_ac" => Some(state.battery_charge_limit_percent),
                "ge_hr_battery_discharge_limit_ac" => Some(state.battery_discharge_limit_percent),
                // HR 1108/1110: 3-phase battery discharge/charge limit (%)
                "tph_battery_discharge_limit_ac" => Some(state.battery_discharge_limit_percent),
                "tph_battery_charge_limit_ac" => Some(state.battery_charge_limit_percent),
                "ge_hr_enable_buzzer" => {
                    self.values.insert(key, 0);
                    continue;
                }
                // HR 114: Battery discharge min power reserve (%)
                "ge_hr_battery_discharge_min_power_reserve" => {
                    Some(state.battery_discharge_min_power_reserve)
                }
                "ge_hr_island_check_continue" => {
                    self.values.insert(key, 0);
                    continue;
                }
                // HR 116: Charge target SOC (%)
                "ge_hr_charge_target_soc" => Some(100.0),
                "ge_hr_charge_soc_stop_2"
                | "ge_hr_discharge_soc_stop_2"
                | "ge_hr_charge_soc_stop_1"
                | "ge_hr_discharge_soc_stop_1" => {
                    self.values.insert(key, 100);
                    continue;
                }
                "ge_hr_enable_local_command_test"
                | "ge_hr_enable_low_voltage_fault_ride_through"
                | "ge_hr_enable_frequency_derating"
                | "ge_hr_enable_above_6kw_system"
                | "ge_hr_start_system_auto_test"
                | "ge_hr_enable_spi" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_power_factor_function_model" | "ge_hr_frequency_load_limit_rate" => {
                    self.values.insert(key, 0);
                    continue;
                }
                // HR 163: Inverter reboot (write-only, always reads 0)
                "ge_hr_inverter_reboot" => {
                    self.values.insert(key, 0);
                    continue;
                }
                // HR 166: Enable RTC
                "ge_hr_rtc_enable" => {
                    self.values.insert(key, state.enable_rtc as u16);
                    continue;
                }
                "ge_hr_threephase_balance_mode"
                | "ge_hr_threephase_abc"
                | "ge_hr_threephase_balance_1"
                | "ge_hr_threephase_balance_2"
                | "ge_hr_threephase_balance_3" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_enable_battery_on_pv_or_grid"
                | "ge_hr_debug_inverter"
                | "ge_hr_enable_ups_mode"
                | "ge_hr_enable_g100_limit_switch"
                | "ge_hr_enable_battery_cable_impedance_alarm" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "ge_hr_enable_inverter_parallel_mode" => {
                    self.values
                        .insert(key, state.enable_inverter_parallel_mode as u16);
                    continue;
                }
                // HR 318: Battery pause mode
                "ge_hr_battery_pause_mode" => {
                    self.values.insert(key, state.battery_pause_mode);
                    continue;
                }
                // HR 319: Battery pause slot 1 start (HHMM)
                "ge_hr_battery_pause_slot_1_start" => {
                    self.values.insert(key, state.battery_pause_slot_start);
                    continue;
                }
                // HR 320: Battery pause slot 1 end (HHMM)
                "ge_hr_battery_pause_slot_1_end" => {
                    self.values.insert(key, state.battery_pause_slot_end);
                    continue;
                }
                // HR 311: Export priority
                "ge_hr_export_priority" => {
                    self.values.insert(key, state.export_priority);
                    continue;
                }
                // HR 317: Enable EPS
                "ge_hr_enable_eps" => {
                    self.values.insert(key, state.enable_eps as u16);
                    continue;
                }
                // HR 4107-4114 / 4141-4142 high-energy/reference blocks
                "ge_hr_pv_power_setting_high" | "ge_hr_pv_power_setting_low" => {
                    let (hi, lo) = u32_words(state.config.solar_peak_watts, 1.0);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "ge_hr_battery_discharge_total_alt2_high"
                | "ge_hr_battery_discharge_total_alt2_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.battery_discharge_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "ge_hr_battery_charge_total_alt2_high" | "ge_hr_battery_charge_total_alt2_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.battery_charge_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "ge_hr_battery_discharge_today_alt3" => {
                    Some(state.energy_totals.battery_discharge_kwh)
                }
                "ge_hr_battery_charge_today_alt3" => Some(state.energy_totals.battery_charge_kwh),
                "ge_hr_inverter_export_total_high" | "ge_hr_inverter_export_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.grid_export_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // HR 2040: EMS plant enable
                "ems_plant_enable" => {
                    self.values.insert(key, state.ems_enabled as u16);
                    continue;
                }

                // ================================================================
                // Simulator-internal registers (100+, 200+, etc.)
                // ================================================================
                "inverter_mode" => {
                    let v = match state.inverter.mode_state.effective {
                        sim_models::InverterMode::Normal => 0,
                        sim_models::InverterMode::Eco => 1,
                        sim_models::InverterMode::ForceCharge => 2,
                        sim_models::InverterMode::ForceDischarge => 3,
                        sim_models::InverterMode::ExportLimit => 4,
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "tph_force_charge_enable" => {
                    self.values.insert(
                        key,
                        (state.inverter.mode_state.effective
                            == sim_models::InverterMode::ForceCharge)
                            as u16,
                    );
                    continue;
                }
                "tph_force_discharge_enable" => {
                    self.values.insert(
                        key,
                        (state.inverter.mode_state.effective
                            == sim_models::InverterMode::ForceDischarge)
                            as u16,
                    );
                    continue;
                }
                "inverter_ac_power" => Some(state.inverter.ac_power_w),
                "inverter_export_limit_w" => Some(state.inverter.export_limit_w),
                "inverter_temperature" => Some(state.inverter.temperature_celsius),
                "inverter_firmware_state" => {
                    self.values.insert(key, 1);
                    continue;
                }
                "battery_soc" => Some(state.aggregate_soc()),
                "battery_soc_2" => {
                    Some(state.batteries.get(1).map(|b| b.soc_percent).unwrap_or(0.0))
                }
                "battery_soc_3" => {
                    Some(state.batteries.get(2).map(|b| b.soc_percent).unwrap_or(0.0))
                }
                "battery_power" => {
                    let w = state.total_battery_power_kw() * 1000.0;
                    self.values.insert(key, w as i16 as u16);
                    continue;
                }
                "battery_voltage" => {
                    let soc = state.aggregate_soc();
                    Some(44.0 + soc * 0.08)
                }
                "battery_current" => {
                    let amps = state.total_battery_power_kw() * 1000.0 / 48.0;
                    self.values.insert(key, amps as i16 as u16);
                    continue;
                }
                "battery_temperature" => Some(state.battery_temperature_celsius()),
                "battery_capacity_kwh" => Some(state.total_battery_capacity()),
                "battery_max_charge_kw" => Some(state.total_max_charge_kw()),
                "battery_max_discharge_kw" => Some(state.total_max_discharge_kw()),
                "battery_min_soc" => Some(state.min_aggregate_soc()),
                "battery_max_soc" => Some(state.max_aggregate_soc()),
                "battery_charge_efficiency" => Some(
                    state
                        .batteries
                        .first()
                        .map(|b| b.charge_efficiency)
                        .unwrap_or(0.95),
                ),
                "battery_discharge_efficiency" => Some(
                    state
                        .batteries
                        .first()
                        .map(|b| b.discharge_efficiency)
                        .unwrap_or(0.95),
                ),
                "battery_module_count" => {
                    self.values.insert(key, state.batteries.len() as u16);
                    continue;
                }
                "pv_generation" => Some(state.solar.generation_w),
                "pv_voltage" => Some(if state.solar.generation_w > 0.0 {
                    350.0
                } else {
                    0.0
                }),
                "pv_current" => Some(state.solar.generation_w / 350.0),
                "pv_energy_today" => Some(state.energy_totals.solar_generation_kwh),
                "pv_peak_capacity" => Some(state.config.solar_peak_watts),
                "grid_power" => {
                    // Clamp to i16 range to avoid panic on saturating cast
                    let clamped = state.grid.power_w.clamp(-32768.0, 32767.0);
                    self.values.insert(key, clamped as i16 as u16);
                    continue;
                }
                "grid_voltage" => Some(240.0),
                "grid_frequency" => Some(50.0),
                "grid_connected" => {
                    self.values.insert(key, state.grid.connected as u16);
                    continue;
                }
                "load_power" => Some(state.load.demand_w),
                "grid_import_kwh" => Some(state.energy_totals.grid_import_kwh),
                "grid_export_kwh" => Some(state.energy_totals.grid_export_kwh),
                "battery_charge_kwh" => Some(state.energy_totals.battery_charge_kwh),
                "battery_discharge_kwh" => Some(state.energy_totals.battery_discharge_kwh),
                "solar_generation_kwh" => Some(state.energy_totals.solar_generation_kwh),
                "load_consumption_kwh" => Some(state.energy_totals.load_consumption_kwh),
                "config_battery_count" => {
                    self.values.insert(key, state.batteries.len() as u16);
                    continue;
                }
                "config_tick_interval" => {
                    self.values
                        .insert(key, state.config.tick_interval_secs as u16);
                    continue;
                }
                "config_weather" => {
                    let v = match state.weather.as_str() {
                        "Clear" => 0,
                        "PartlyCloudy" => 1,
                        "Overcast" => 2,
                        "Storm" => 3,
                        _ => 0,
                    };
                    self.values.insert(key, v);
                    continue;
                }
                "schedule_charge_start"
                | "schedule_charge_end"
                | "schedule_discharge_start"
                | "schedule_discharge_end" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "schedule_charge_target_soc" => {
                    self.values.insert(key, 100);
                    continue;
                }
                "schedule_discharge_target_soc" => {
                    self.values.insert(key, 10);
                    continue;
                }

                // ================================================================
                // Three-phase Input Registers (IR 1001-1413)
                // ================================================================
                // Real three-phase clients use these high input-register addresses
                // instead of the single-phase IR 0-59 block.
                "tph_ir_pv1_voltage" => Some(if state.solar.pv1_w > 0.0 { 350.0 } else { 0.0 }),
                "tph_ir_pv2_voltage" => Some(if state.config.pv2_peak_watts > 0.0 {
                    350.0
                } else {
                    0.0
                }),
                "tph_ir_pv1_current" => Some(state.solar.pv1_w / 350.0),
                "tph_ir_pv2_current" => Some(state.solar.pv2_w / 350.0),
                "tph_ir_pv1_power_high" | "tph_ir_pv1_power_low" => {
                    let (hi, lo) = u32_words(state.solar.pv1_w, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_pv2_power_high" | "tph_ir_pv2_power_low" => {
                    let (hi, lo) = u32_words(state.solar.pv2_w, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_v_ac1" | "tph_ir_v_ac2" | "tph_ir_v_ac3" => Some(240.0),
                "tph_ir_i_ac1" | "tph_ir_i_ac2" | "tph_ir_i_ac3" => {
                    Some(state.inverter.ac_power_w.abs() / 240.0 / 3.0)
                }
                "tph_ir_f_ac1" => Some(50.0),
                "tph_ir_power_factor" => Some(10000.0),
                "tph_ir_p_inverter_out_high" | "tph_ir_p_inverter_out_low" => {
                    let (hi, lo) = i32_words(state.inverter.ac_power_w, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_p_grid_apparent_high" | "tph_ir_p_grid_apparent_low" => {
                    let (hi, lo) = u32_words(state.inverter.ac_power_w.abs(), 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_p_meter_import_high" | "tph_ir_p_meter_import_low" => {
                    // Hardcoded lifetime CT meter import total for testing
                    let (hi, lo) = u32_words(123.4, 0.1); // raw 1234 at ×0.1
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_p_meter_export_high" | "tph_ir_p_meter_export_low" => {
                    // Hardcoded lifetime CT meter export total for testing
                    let (hi, lo) = u32_words(432.1, 0.1); // raw 4321 at ×0.1
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_p_export_high" | "tph_ir_p_export_low" => {
                    // Hardcoded lifetime CT export total for testing
                    let (hi, lo) = u32_words(432.1, 0.1); // raw 4321 at ×0.1
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_p_meter2_high" | "tph_ir_p_meter2_low" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "tph_ir_system_mode" => Some(if !state.active_faults.is_empty() {
                    3.0
                } else if state.grid.connected {
                    2.0
                } else {
                    1.0
                }),
                "tph_ir_status" | "tph_ir_dc_status" => Some(1.0),
                "tph_ir_p_load_ac1" | "tph_ir_p_load_ac2" | "tph_ir_p_load_ac3" => {
                    Some(state.load.demand_w / 3.0)
                }
                "tph_ir_p_load_all_high" | "tph_ir_p_load_all_low" => {
                    let (hi, lo) = u32_words(state.load.demand_w, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_t_inverter" => Some(state.inverter.temperature_celsius),
                "tph_ir_t_boost" => Some(state.inverter.temperature_celsius - 2.0),
                "tph_ir_t_buck_boost" => Some(state.inverter.temperature_celsius + 3.0),
                "tph_ir_v_battery_bms" => Some(76.8),
                "tph_ir_battery_soc" => Some(state.aggregate_soc()),
                "tph_ir_p_battery_discharge_high" | "tph_ir_p_battery_discharge_low" => {
                    let discharge_w = (-state.total_battery_power_kw()).max(0.0) * 1000.0;
                    let (hi, lo) = u32_words(discharge_w, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_p_battery_charge_high" | "tph_ir_p_battery_charge_low" => {
                    let charge_w = state.total_battery_power_kw().max(0.0) * 1000.0;
                    let (hi, lo) = u32_words(charge_w, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_v_battery_pcs" => Some(76.8),
                "tph_ir_v_dc_bus" => Some(380.0),
                "tph_ir_v_inv_bus" => Some(380.0),
                "tph_ir_i_battery" => {
                    let amps = -(state.total_battery_power_kw() * 1000.0 / 76.8);
                    self.values.insert(key, (amps * 10.0) as i16 as u16);
                    continue;
                }
                "tph_ir_f_nominal_eps" => Some(if state.enable_eps { 50.0 } else { 0.0 }),
                "tph_ir_v_eps_ac1" | "tph_ir_v_eps_ac2" | "tph_ir_v_eps_ac3" => {
                    Some(if state.enable_eps { 240.0 } else { 0.0 })
                }
                "tph_ir_ac_dsp_firmware_version" | "tph_ir_dc_dsp_firmware_version" => {
                    self.values.insert(key, state.inverter.dsp_firmware_version);
                    continue;
                }
                "tph_ir_arm_firmware_version" => {
                    let fw = if state.inverter.arm_firmware_version != 0 {
                        state.inverter.arm_firmware_version
                    } else if state.config.inverter_type.starts_with("ThreePhase")
                        || state.config.inverter_type == "ACThreePhase"
                    {
                        612
                    } else {
                        match state.config.inverter_type.as_str() {
                            "Gen1Hybrid" => 252,
                            "Gen2Hybrid" => 852,
                            "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 452,
                            _ => 318,
                        }
                    };
                    self.values.insert(key, fw);
                    continue;
                }
                "tph_ir_e_pv1_today_high" | "tph_ir_e_pv1_today_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh / 2.0, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_pv2_today_high" | "tph_ir_e_pv2_today_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh / 2.0, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_import_today_high" | "tph_ir_e_import_today_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.grid_import_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_export_today_high" | "tph_ir_e_export_today_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.grid_export_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_battery_discharge_today_high"
                | "tph_ir_e_battery_discharge_today_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.battery_discharge_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_battery_charge_today_high" | "tph_ir_e_battery_charge_today_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.battery_charge_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_load_today_high" | "tph_ir_e_load_today_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.load_consumption_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_inverter_out_today_high"
                | "tph_ir_e_inverter_out_today_low"
                | "tph_ir_e_inverter_out_total_high"
                | "tph_ir_e_inverter_out_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_pv1_total_high" | "tph_ir_e_pv1_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh / 2.0, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_pv2_total_high" | "tph_ir_e_pv2_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh / 2.0, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_pv_total_high" | "tph_ir_e_pv_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_pv_today_high" | "tph_ir_e_pv_today_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_ac_charge_today_high"
                | "tph_ir_e_ac_charge_today_low"
                | "tph_ir_e_ac_charge_total_high"
                | "tph_ir_e_ac_charge_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.ac_charge_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_import_total_high" | "tph_ir_e_import_total_low" => {
                    // Hardcoded lifetime CT clamp values for testing
                    let (hi, lo) = u32_words(123.4, 0.1); // raw 1234 at ×0.1
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_export_total_high" | "tph_ir_e_export_total_low" => {
                    // Hardcoded lifetime CT clamp values for testing
                    let (hi, lo) = u32_words(432.1, 0.1); // raw 4321 at ×0.1
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_battery_discharge_total_high"
                | "tph_ir_e_battery_discharge_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.battery_discharge_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_battery_charge_total_high" | "tph_ir_e_battery_charge_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.battery_charge_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_load_total_high" | "tph_ir_e_load_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.load_consumption_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_export2_today_high"
                | "tph_ir_e_export2_today_low"
                | "tph_ir_e_export2_total_high"
                | "tph_ir_e_export2_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.grid_export_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }

                _ => continue,
            };

            if let Some(eng) = engineering {
                let raw = if def.scaling_factor > 0.0 {
                    (eng / def.scaling_factor).round() as u16
                } else {
                    eng.round() as u16
                };
                self.values.insert(key, raw);
            }
        }
    }

    /// Project schedule parameters into GE holding registers.
    ///
    /// Call after `project_from_state` — this overwrites the hardcoded
    /// "disabled sentinel" values with the actual schedule.
    pub fn project_schedule(&mut self, schedule: &sim_models::Schedule) {
        self.project_schedule_for(schedule, "");
    }

    /// Like `project_schedule` but accepts inverter type string.
    /// AC-coupled inverters (prefix "ACCoupled") skip discharge slot projection.
    pub fn project_schedule_for(&mut self, schedule: &sim_models::Schedule, inverter_type: &str) {
        let is_ac_coupled = inverter_type.starts_with("ACCoupled");
        // Convert decimal hours to HHMM.
        // 0.0 → 0 (valid 00:00 midnight). Negative → 60 (disabled).
        let hrs_to_hhmm = |h: f64| -> u16 {
            if h < 0.0 {
                return 60; /* disabled */
            }
            let hours = h.floor() as u16;
            let mins = ((h - hours as f64) * 60.0).round() as u16;
            if mins > 59 || hours > 23 {
                return 60; /* invalid = disabled */
            }
            hours * 100 + mins
        };
        // Helper: if start == end the slot is disabled → write 60/60.
        let slot_pair = |start: f64, end: f64| -> (u16, u16) {
            if (start - end).abs() < 0.001 {
                (60, 60)
            } else {
                (hrs_to_hhmm(start), hrs_to_hhmm(end))
            }
        };

        // Charge slot 1 (HR 94-95)
        let (cs1_start, cs1_end) = slot_pair(schedule.charge_start, schedule.charge_end);
        self.write(94, cs1_start);
        self.write(95, cs1_end);
        let (cs2_start, cs2_end) = slot_pair(schedule.charge_start_2, schedule.charge_end_2);
        self.write(31, cs2_start);
        self.write(32, cs2_end);
        self.write(243, cs2_start);
        self.write(244, cs2_end);
        let (cs3_s, cs3_e) = slot_pair(schedule.charge_start_3, schedule.charge_end_3);
        self.write(246, cs3_s);
        self.write(247, cs3_e);
        let (cs4_s, cs4_e) = slot_pair(schedule.charge_start_4, schedule.charge_end_4);
        self.write(249, cs4_s);
        self.write(250, cs4_e);
        let (cs5_s, cs5_e) = slot_pair(schedule.charge_start_5, schedule.charge_end_5);
        self.write(252, cs5_s);
        self.write(253, cs5_e);
        let (cs6_s, cs6_e) = slot_pair(schedule.charge_start_6, schedule.charge_end_6);
        self.write(255, cs6_s);
        self.write(256, cs6_e);
        let (cs7_s, cs7_e) = slot_pair(schedule.charge_start_7, schedule.charge_end_7);
        self.write(258, cs7_s);
        self.write(259, cs7_e);
        let (cs8_s, cs8_e) = slot_pair(schedule.charge_start_8, schedule.charge_end_8);
        self.write(261, cs8_s);
        self.write(262, cs8_e);
        let (cs9_s, cs9_e) = slot_pair(schedule.charge_start_9, schedule.charge_end_9);
        self.write(264, cs9_s);
        self.write(265, cs9_e);
        let (cs10_s, cs10_e) = slot_pair(schedule.charge_start_10, schedule.charge_end_10);
        self.write(267, cs10_s);
        self.write(268, cs10_e);

        let (ds1_start, ds1_end) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start, schedule.discharge_end)
        };
        self.write(56, ds1_start);
        self.write(57, ds1_end);
        let (ds2_start, ds2_end) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_2, schedule.discharge_end_2)
        };
        self.write(44, ds2_start);
        self.write(45, ds2_end);
        let (ds3_s, ds3_e) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_3, schedule.discharge_end_3)
        };
        self.write(276, ds3_s);
        self.write(277, ds3_e);
        let (ds4_s, ds4_e) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_4, schedule.discharge_end_4)
        };
        self.write(279, ds4_s);
        self.write(280, ds4_e);
        let (ds5_s, ds5_e) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_5, schedule.discharge_end_5)
        };
        self.write(282, ds5_s);
        self.write(283, ds5_e);
        let (ds6_s, ds6_e) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_6, schedule.discharge_end_6)
        };
        self.write(285, ds6_s);
        self.write(286, ds6_e);
        let (ds7_s, ds7_e) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_7, schedule.discharge_end_7)
        };
        self.write(288, ds7_s);
        self.write(289, ds7_e);
        let (ds8_s, ds8_e) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_8, schedule.discharge_end_8)
        };
        self.write(291, ds8_s);
        self.write(292, ds8_e);
        let (ds9_s, ds9_e) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_9, schedule.discharge_end_9)
        };
        self.write(294, ds9_s);
        self.write(295, ds9_e);
        let (ds10_s, ds10_e) = if is_ac_coupled {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_10, schedule.discharge_end_10)
        };
        self.write(297, ds10_s);
        self.write(298, ds10_e);

        let charge_enabled = schedule.enable_charge
            || schedule.charge_start != schedule.charge_end
            || schedule.charge_start_2 != schedule.charge_end_2
            || schedule.charge_start_3 != schedule.charge_end_3
            || schedule.charge_start_4 != schedule.charge_end_4
            || schedule.charge_start_5 != schedule.charge_end_5
            || schedule.charge_start_6 != schedule.charge_end_6
            || schedule.charge_start_7 != schedule.charge_end_7
            || schedule.charge_start_8 != schedule.charge_end_8
            || schedule.charge_start_9 != schedule.charge_end_9
            || schedule.charge_start_10 != schedule.charge_end_10;
        self.write(96, if charge_enabled { 1 } else { 0 });

        let discharge_enabled = !is_ac_coupled
            && (schedule.enable_discharge
                || schedule.discharge_start != schedule.discharge_end
                || schedule.discharge_start_2 != schedule.discharge_end_2
                || schedule.discharge_start_3 != schedule.discharge_end_3
                || schedule.discharge_start_4 != schedule.discharge_end_4
                || schedule.discharge_start_5 != schedule.discharge_end_5
                || schedule.discharge_start_6 != schedule.discharge_end_6
                || schedule.discharge_start_7 != schedule.discharge_end_7
                || schedule.discharge_start_8 != schedule.discharge_end_8
                || schedule.discharge_start_9 != schedule.discharge_end_9
                || schedule.discharge_start_10 != schedule.discharge_end_10);
        self.write(59, if discharge_enabled { 1 } else { 0 });

        self.write(116, schedule.charge_target_soc as u16);
        self.write(242, schedule.charge_target_soc as u16);
        self.write(245, schedule.charge_target_soc_2 as u16);
        self.write(248, schedule.charge_target_soc_3 as u16);
        self.write(251, schedule.charge_target_soc_4 as u16);
        self.write(254, schedule.charge_target_soc_5 as u16);
        self.write(257, schedule.charge_target_soc_6 as u16);
        self.write(260, schedule.charge_target_soc_7 as u16);
        self.write(263, schedule.charge_target_soc_8 as u16);
        self.write(266, schedule.charge_target_soc_9 as u16);
        self.write(269, schedule.charge_target_soc_10 as u16);
        self.write(1111, schedule.charge_target_soc as u16);
        self.write(1109, 10);
        self.write(1112, if charge_enabled { 1 } else { 0 });
        self.write(
            272,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc as u16
            },
        );
        self.write(
            275,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_2 as u16
            },
        );
        self.write(
            278,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_3 as u16
            },
        );
        self.write(
            281,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_4 as u16
            },
        );
        self.write(
            284,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_5 as u16
            },
        );
        self.write(
            287,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_6 as u16
            },
        );
        self.write(
            290,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_7 as u16
            },
        );
        self.write(
            293,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_8 as u16
            },
        );
        self.write(
            296,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_9 as u16
            },
        );
        self.write(
            299,
            if is_ac_coupled {
                0
            } else {
                schedule.discharge_target_soc_10 as u16
            },
        );

        // TPH mirror slot time registers (HR 1113-1121)
        // TPH_CHARGE_SLOT_1_START/END = 1113-1114 (mirrors HR 94-95)
        self.write(1113, cs1_start);
        self.write(1114, cs1_end);
        // TPH_CHARGE_SLOT_2_START/END = 1115-1116 (mirrors HR 31-32 / 243-244)
        self.write(1115, cs2_start);
        self.write(1116, cs2_end);
        // TPH_DISCHARGE_SLOT_1_START/END = 1118-1119 (mirrors HR 56-57)
        self.write(1118, ds1_start);
        self.write(1119, ds1_end);
        // TPH_DISCHARGE_SLOT_2_START/END = 1120-1121 (mirrors HR 44-45)
        self.write(1120, ds2_start);
        self.write(1121, ds2_end);
        // TPH_BATTERY_SOC_RESERVE = 1109 (mirrors HR 110)
        let reserve = self
            .read_by_space(110, RegisterSpace::Holding)
            .filter(|&v| v != 0)
            .unwrap_or(10);
        self.write(1109, reserve);

        // Export schedule slots (HR 2062-2071)
        let (es1_s, es1_e) = slot_pair(schedule.export_start_1, schedule.export_end_1);
        self.write(2062, es1_s);
        self.write(2063, es1_e);
        self.write(2064, schedule.export_target_soc_1 as u16);
        let (es2_s, es2_e) = slot_pair(schedule.export_start_2, schedule.export_end_2);
        self.write(2065, es2_s);
        self.write(2066, es2_e);
        self.write(2067, schedule.export_target_soc_2 as u16);
        let (es3_s, es3_e) = slot_pair(schedule.export_start_3, schedule.export_end_3);
        self.write(2068, es3_s);
        self.write(2069, es3_e);
        self.write(2070, schedule.export_target_soc_3 as u16);
        self.write(2071, schedule.export_power_limit_w as u16);

        // EMS charge/discharge slots (HR 2044-2061) — share the same
        // Schedule fields as native inverter slots.
        let (ems_ds1_s, ems_ds1_e) = slot_pair(schedule.discharge_start, schedule.discharge_end);
        self.write(2044, ems_ds1_s);
        self.write(2045, ems_ds1_e);
        self.write(2046, schedule.discharge_target_soc as u16);
        let (ems_ds2_s, ems_ds2_e) =
            slot_pair(schedule.discharge_start_2, schedule.discharge_end_2);
        self.write(2047, ems_ds2_s);
        self.write(2048, ems_ds2_e);
        self.write(2049, schedule.discharge_target_soc_2 as u16);
        let (ems_ds3_s, ems_ds3_e) =
            slot_pair(schedule.discharge_start_3, schedule.discharge_end_3);
        self.write(2050, ems_ds3_s);
        self.write(2051, ems_ds3_e);
        self.write(2052, schedule.discharge_target_soc_3 as u16);
        let (ems_cs1_s, ems_cs1_e) = slot_pair(schedule.charge_start, schedule.charge_end);
        self.write(2053, ems_cs1_s);
        self.write(2054, ems_cs1_e);
        self.write(2055, schedule.charge_target_soc as u16);
        let (ems_cs2_s, ems_cs2_e) = slot_pair(schedule.charge_start_2, schedule.charge_end_2);
        self.write(2056, ems_cs2_s);
        self.write(2057, ems_cs2_e);
        self.write(2058, schedule.charge_target_soc_2 as u16);
        let (ems_cs3_s, ems_cs3_e) = slot_pair(schedule.charge_start_3, schedule.charge_end_3);
        self.write(2059, ems_cs3_s);
        self.write(2060, ems_cs3_e);
        self.write(2061, schedule.charge_target_soc_3 as u16);

        // Internal schedule registers (HR 700-729)
        self.write(700, cs1_start);
        self.write(701, cs1_end);
        self.write(702, ds1_start);
        self.write(703, ds1_end);
        self.write(704, schedule.charge_target_soc as u16);
        self.write(705, schedule.discharge_target_soc as u16);
        self.write(706, cs2_start);
        self.write(707, cs2_end);
        self.write(708, ds2_start);
        self.write(709, ds2_end);
        self.write(710, schedule.charge_target_soc_2 as u16);
        self.write(711, schedule.discharge_target_soc_2 as u16);
        self.write(712, cs3_s);
        self.write(713, cs3_e);
        self.write(714, ds3_s);
        self.write(715, ds3_e);
        self.write(716, schedule.charge_target_soc_3 as u16);
        self.write(717, schedule.discharge_target_soc_3 as u16);
        self.write(718, cs4_s);
        self.write(719, cs4_e);
        self.write(720, ds4_s);
        self.write(721, ds4_e);
        self.write(722, schedule.charge_target_soc_4 as u16);
        self.write(723, schedule.discharge_target_soc_4 as u16);
        self.write(724, cs5_s);
        self.write(725, cs5_e);
        self.write(726, ds5_s);
        self.write(727, ds5_e);
    }

    /// Iterator over all definitions.
    /// Project battery BMS data for a specific battery module (IR 60-119).
    ///
    /// Returns 60 u16 values for Input Registers 60-119.
    /// The reference client reads this on slave 0x32 for battery #1,
    /// and slaves 0x33-0x37 for additional batteries.
    pub fn project_battery_bms(
        &self,
        battery: &sim_models::BatteryState,
        module_index: usize,
    ) -> [u16; 60] {
        let mut regs = [0u16; 60];

        // Cell voltages: IR 60-75 (mV). Simulate 16 cells from total voltage.
        let cell_count = 16usize;
        let cell_mv = (battery.voltage_v * 1000.0 / cell_count as f64).round() as u16;
        for reg in regs.iter_mut().take(cell_count) {
            *reg = cell_mv; // IR 60+i
        }

        // Cell group temperatures: IR 76-79 (0.1 °C)
        let temp_deci = (battery.temperature_celsius * 10.0).round() as u16;
        for i in 0..4 {
            regs[16 + i] = temp_deci; // IR 76+i
        }

        // v_cells_sum: IR 80 (mV)
        regs[20] = (battery.voltage_v * 1000.0).round() as u16;

        // t_bms_mosfet: IR 81 (0.1 °C)
        regs[21] = temp_deci;

        // v_out: IR 82-83 (uint32 mV, little-endian u16 pair)
        let v_out_mv = (battery.voltage_v * 1000.0).round() as u32;
        regs[22] = (v_out_mv >> 16) as u16; // IR 82 high
        regs[23] = (v_out_mv & 0xFFFF) as u16; // IR 83 low

        // Capacity registers (uint32, 0.01 Ah)
        let cap_design_ah = (battery.nominal_capacity_kwh * 1000.0 / 51.2 * 100.0).round() as u32;
        let cap_calibrated_ah = (battery.capacity_kwh * 1000.0 / 51.2 * 100.0).round() as u32;
        let cap_remaining_ah =
            ((battery.capacity_kwh * battery.soc_percent / 100.0) * 1000.0 / 51.2 * 100.0).round()
                as u32;

        // cap_calibrated: IR 84-85
        regs[24] = (cap_calibrated_ah >> 16) as u16;
        regs[25] = (cap_calibrated_ah & 0xFFFF) as u16;
        // cap_design: IR 86-87
        regs[26] = (cap_design_ah >> 16) as u16;
        regs[27] = (cap_design_ah & 0xFFFF) as u16;
        // cap_remaining: IR 88-89
        regs[28] = (cap_remaining_ah >> 16) as u16;
        regs[29] = (cap_remaining_ah & 0xFFFF) as u16;

        // Status/warning words: IR 90-94. Keep healthy defaults but mark charge/discharge state.
        let state_code = if battery.power_kw > 0.01 {
            1u16
        } else if battery.power_kw < -0.01 {
            2u16
        } else {
            0u16
        };
        regs[30] = state_code << 8; // IR 90: status_1/status_2
        regs[31] = 0; // IR 91: status_3/status_4
        regs[32] = 0; // IR 92: status_5/status_6
        regs[33] = 0; // IR 93: status_7
        regs[34] = 0; // IR 94: warning_1/warning_2

        // num_cycles: IR 96
        regs[36] = battery.cycle_count.round() as u16;
        // num_cells: IR 97
        regs[37] = cell_count as u16;
        // bms_firmware_version: IR 98
        regs[38] = 100; // v1.00

        // SOC: IR 100
        regs[40] = battery.soc_percent.round() as u16;

        // cap_design2: IR 101-102 (mirror design capacity for clients that read the alt pair)
        regs[41] = (cap_design_ah >> 16) as u16;
        regs[42] = (cap_design_ah & 0xFFFF) as u16;

        // t_max: IR 103 (0.1 °C)
        regs[43] = temp_deci;
        // t_min: IR 104 (0.1 °C)
        regs[44] = (temp_deci as f64 * 0.98).round() as u16;

        // Serial number: IR 110-114 (5 regs = 10 Latin-1 chars)
        // Generate a simulated serial
        let serial_str = format!(
            "BAT{:04X}{:02X}  ",
            module_index + 1,
            (battery.soc_percent as u16) & 0xFF
        );
        for i in 0..5 {
            let c1 = serial_str.as_bytes().get(i * 2).copied().unwrap_or(b' ');
            let c2 = serial_str
                .as_bytes()
                .get(i * 2 + 1)
                .copied()
                .unwrap_or(b' ');
            regs[50 + i] = (c1 as u16) << 8 | c2 as u16;
        }

        // usb_device_inserted: IR 115 (raw; reference keeps as uint16)
        regs[55] = 1;

        regs
    }

    /// HV BMS discovery data (slave 0xA0), IR 60-64.
    ///
    /// A real client (givenergy-modbus / giv_tcp) reads IR(60,5) at slave 0xA0
    /// during cold HV discovery; IR(61) holds the number of BCUs present. We
    /// model single-stack GIV-BAT-HV systems as one BCU, so IR(61) = 1.
    pub fn project_battery_bms_discovery(&self, num_bcus: usize) -> [u16; 5] {
        let mut regs = [0u16; 5];
        // IR(60): BMS software version nibble (non-zero so the version is not blank).
        regs[0] = 1;
        // IR(61): number of BCUs present (the field the client decodes).
        regs[1] = num_bcus as u16;
        // IR(62-64): reserved / unused by either reference library.
        regs
    }

    /// HV BCU cluster data (slave 0x70+i), IR 60-119.
    ///
    /// Aggregates all modules in the stack into one cluster view, following the
    /// BCU register LUT shared by givenergy-modbus (model/hv_bcu.py) and giv_tcp
    /// (givenergy_modbus_async/model/hvbcu.py). Validity hinges on
    /// `pack_software_version` (IR 60-63) decoding to a non-blank string via the
    /// gateway_version converter, so the version encoding must round-trip.
    pub fn project_battery_bcu(&self, batteries: &[sim_models::BatteryState]) -> [u16; 60] {
        let mut regs = [0u16; 60];
        let n = batteries.len().max(1);

        // IR 60-63: pack_software_version. gateway_version(r60,r61,r62,r63) builds
        // prefix = latin1(r60||r61) with nulls stripped, digits = decimal bytes of
        // r62||r63. Encode "HV00000001" so is_valid() (not blank, not all '0') holds.
        regs[0] = 0x4856; // 'H','V'
        regs[1] = 0x3030; // '0','0'
        regs[2] = 0x3030; // '0','0'
        regs[3] = 0x3031; // '0','1'

        // IR 64: number_of_modules
        regs[4] = batteries.len() as u16;
        // IR 65: cells_per_module (all known HV stacks use 24 cells/module)
        regs[5] = 24;

        // Aggregate cluster values. Modules are in series: stack voltage sums,
        // current is shared, power sums, SOC/SOH average.
        let avg_soc = batteries.iter().map(|b| b.soc_percent).sum::<f64>() / n as f64;
        let stack_voltage = batteries.iter().map(|b| b.voltage_v).sum::<f64>();
        let stack_current = batteries.first().map(|b| b.current_a).unwrap_or(0.0);
        let power_kw = batteries.iter().map(|b| b.power_kw).sum::<f64>();
        let avg_soh = batteries.iter().map(|b| b.soh).sum::<f64>() / n as f64;
        let avg_cycles = batteries.iter().map(|b| b.cycle_count).sum::<f64>() / n as f64;
        let soc_max = batteries
            .iter()
            .map(|b| b.soc_percent)
            .fold(0.0_f64, f64::max)
            .round()
            .clamp(0.0, 100.0) as u16;
        let soc_min = batteries
            .iter()
            .map(|b| b.soc_percent)
            .fold(300.0_f64, f64::min)
            .round()
            .clamp(0.0, 100.0) as u16;

        // IR 73: battery_voltage (deci V)
        regs[13] = (stack_voltage * 10.0).round() as u16;
        // IR 74: load_voltage (deci V) — mirror stack voltage
        regs[14] = regs[13];
        // IR 76: battery_current (int16, deci A, signed: + = charging)
        regs[16] = (stack_current * 10.0).round() as i16 as u16;
        // IR 79: battery_power (milli: raw = kW * 1000, signed)
        regs[19] = (power_kw * 1000.0).round() as i16 as u16;
        // IR 80: SOC max/min (duint8: high byte = max, low byte = min)
        regs[20] = (soc_max << 8) | soc_min;
        // IR 81: battery_soh (uint16 %)
        regs[21] = (avg_soh * 100.0).round().clamp(0.0, 100.0) as u16;

        // IR 82-83: charge_energy_total (uint32, deci kWh)
        let charge_total = batteries.iter().map(|b| b.throughput_kwh).sum::<f64>();
        let ct_raw = (charge_total * 10.0).round() as u32;
        regs[22] = (ct_raw >> 16) as u16;
        regs[23] = (ct_raw & 0xFFFF) as u16;
        // IR 84-85: discharge_energy_total — mirror (sim doesn't track separately)
        regs[24] = regs[22];
        regs[25] = regs[23];

        // IR 90-91: charge_energy_today, IR 92-93: discharge_energy_today — left 0
        // (cluster-level daily totals are not modelled per-module).

        // IR 98: nominal_capacity (deci Ah). Series stack keeps Ah constant; the
        // GIV-BAT-3.4-HV module is 51 Ah nominal. giv_tcp multiplies this by the
        // module count for total kWh, so report the per-module figure.
        regs[38] = 510; // 51.0 Ah * 10
        // IR 99: remaining_battery_capacity (deci Ah), scaled by SOC
        regs[39] = (51.0 * avg_soc / 100.0 * 10.0).round() as u16;
        // IR 100: number_of_cycles (deci)
        regs[40] = (avg_cycles * 10.0).round() as u16;

        regs
    }

    /// HV BMU per-module data (slave 0x50+m), IR 60-119.
    ///
    /// Follows the BMU layout in givenergy-modbus (model/hv_bcu.py Bmu) and
    /// giv_tcp (model/hvbmu.py): 24 cell voltages at IR 60-83 (milli V), 24 cell
    /// temperatures at IR 90-113 (deci °C), and a 5-register serial at IR 114-118.
    /// Validity hinges on the serial decoding to a non-blank string.
    pub fn project_battery_bmu(
        &self,
        battery: &sim_models::BatteryState,
        module_index: usize,
    ) -> [u16; 60] {
        let mut regs = [0u16; 60];
        let cells = 24usize;

        // IR 60-83: 24 cell voltages (milli V). Nominal LFP ~3.2-3.4 V/cell,
        // nudged by SOC so the cluster looks healthy and varying.
        let cell_v = 3.3 + (battery.soc_percent - 50.0) * 0.003;
        let cell_mv = (cell_v * 1000.0).round() as u16;
        for reg in regs.iter_mut().take(cells) {
            *reg = cell_mv; // IR 60+i
        }

        // IR 90-113: 24 cell temperatures (deci °C)
        let temp_deci = (battery.temperature_celsius * 10.0).round() as u16;
        for i in 0..cells {
            if 30 + i < 60 {
                regs[30 + i] = temp_deci; // IR 90+i
            }
        }

        // IR 114-118: serial number (5 regs = 10 Latin-1 chars). MUST be non-blank
        // for Bmu.is_valid(). Mirror the LV projector's encoding.
        let serial_str = format!("BMU{:04X}{:02X} ", module_index + 1, cells as u16);
        for i in 0..5 {
            let c1 = serial_str.as_bytes().get(i * 2).copied().unwrap_or(b' ');
            let c2 = serial_str
                .as_bytes()
                .get(i * 2 + 1)
                .copied()
                .unwrap_or(b' ');
            regs[54 + i] = (c1 as u16) << 8 | c2 as u16; // IR 114+i
        }

        regs
    }

    pub fn definitions(&self) -> &[RegisterDef] {
        &self.defs
    }

    /// Return a snapshot of all register values as a HashMap.
    pub fn snapshot(&self) -> std::collections::HashMap<u32, u16> {
        self.values.clone()
    }

    /// Return a snapshot of holding register values with u16 keys (backward compat).
    pub fn snapshot_holding(&self) -> std::collections::HashMap<u16, u16> {
        self.values
            .iter()
            .filter(|(k, _)| **k >= HOLDING_OFFSET)
            .map(|(k, v)| ((*k - HOLDING_OFFSET) as u16, *v))
            .collect()
    }
}

/// Return the expanded register catalogue.
pub fn default_register_catalogue() -> Vec<RegisterDef> {
    use Access::*;
    use RegisterCategory as C;
    use RegisterSpace::*;
    use RegisterType as T;

    vec![
        // ================================================================
        // GivEnergy-native Input Registers (IR 0-59)
        // Read via fn 0x04 (Read Input Registers), slave 0x32
        // ================================================================
        RegisterDef {
            address: 0,
            name: "ge_ir_status".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1,
            name: "ge_ir_pv1_voltage".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2,
            name: "ge_ir_pv2_voltage".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 3,
            name: "ge_ir_p_bus_voltage".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 4,
            name: "ge_ir_n_bus_voltage".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 10,
            name: "ge_ir_ac_current".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 14,
            name: "ge_ir_charge_status".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 15,
            name: "ge_ir_high_bus_voltage".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 16,
            name: "ge_ir_inverter_output_pf".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 23,
            name: "ge_ir_solar_diverter_energy".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 24,
            name: "ge_ir_inverter_terminal_power".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 27,
            name: "ge_ir_inverter_input_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 28,
            name: "ge_ir_inverter_input_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 29,
            name: "ge_ir_discharge_year".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 31,
            name: "ge_ir_eps_backup_power".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 38,
            name: "ge_ir_countdown".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 39,
            name: "ge_ir_fault_code_high".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 40,
            name: "ge_ir_fault_code_low".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 42,
            name: "ge_ir_load_demand".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 43,
            name: "ge_ir_grid_apparent".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 47,
            name: "ge_ir_work_time_total_high".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 48,
            name: "ge_ir_work_time_total_low".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 49,
            name: "ge_ir_system_mode".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 53,
            name: "ge_ir_eps_voltage".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 54,
            name: "ge_ir_eps_frequency".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 55,
            name: "ge_ir_charger_temperature".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 57,
            name: "ge_ir_charger_warning_code".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 58,
            name: "ge_ir_grid_port_current".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 180,
            name: "ge_ir_battery_discharge_total_alt1".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 181,
            name: "ge_ir_battery_charge_total_alt1".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 182,
            name: "ge_ir_battery_discharge_today_alt2".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 183,
            name: "ge_ir_battery_charge_today_alt2".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 247,
            name: "ge_ir_combined_generation_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 248,
            name: "ge_ir_combined_generation_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 5,
            name: "ge_ir_grid_voltage".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 6,
            name: "ge_ir_battery_throughput_high".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 7,
            name: "ge_ir_battery_throughput_low".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 8,
            name: "ge_ir_pv1_current".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 9,
            name: "ge_ir_pv2_current".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 11,
            name: "ge_ir_pv_total_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 12,
            name: "ge_ir_pv_total_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 13,
            name: "ge_ir_grid_frequency".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 17,
            name: "ge_ir_pv1_energy_today".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 18,
            name: "ge_ir_pv1_power".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 19,
            name: "ge_ir_pv2_energy_today".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 20,
            name: "ge_ir_pv2_power".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 21,
            name: "ge_ir_grid_export_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 22,
            name: "ge_ir_grid_export_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 25,
            name: "ge_ir_today_export_energy".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 26,
            name: "ge_ir_today_import_energy".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 30,
            name: "ge_ir_grid_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 32,
            name: "ge_ir_grid_import_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 33,
            name: "ge_ir_grid_import_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 35,
            name: "ge_ir_ac_charge_today".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 36,
            name: "ge_ir_today_charge_energy".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 37,
            name: "ge_ir_today_discharge_energy".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 41,
            name: "ge_ir_inverter_temperature".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 44,
            name: "ge_ir_pv_generation_today".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 45,
            name: "ge_ir_pv_generation_total_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 46,
            name: "ge_ir_pv_generation_total_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 50,
            name: "ge_ir_battery_voltage".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 51,
            name: "ge_ir_battery_current".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 52,
            name: "ge_ir_battery_power".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 56,
            name: "ge_ir_battery_temperature".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 59,
            name: "ge_ir_battery_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        // ================================================================
        // CT Clamp Meter Input Registers (IR 60-89)
        // Served on device addresses 0x01-0x08 (separate from inverter 0x32)
        // ================================================================
        RegisterDef {
            address: 60,
            name: "meter_v_phase_1".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 61,
            name: "meter_v_phase_2".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 62,
            name: "meter_v_phase_3".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 63,
            name: "meter_i_phase_1".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 64,
            name: "meter_i_phase_2".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 65,
            name: "meter_i_phase_3".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 66,
            name: "meter_i_ln".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 67,
            name: "meter_i_total".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 68,
            name: "meter_p_active_phase_1".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 69,
            name: "meter_p_active_phase_2".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 70,
            name: "meter_p_active_phase_3".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 71,
            name: "meter_p_active_total".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 72,
            name: "meter_p_reactive_phase_1".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 73,
            name: "meter_p_reactive_phase_2".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 74,
            name: "meter_p_reactive_phase_3".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 75,
            name: "meter_p_reactive_total".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 76,
            name: "meter_p_apparent_phase_1".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 77,
            name: "meter_p_apparent_phase_2".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 78,
            name: "meter_p_apparent_phase_3".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 79,
            name: "meter_p_apparent_total".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 80,
            name: "meter_pf_phase_1".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 0.001,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 81,
            name: "meter_pf_phase_2".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 0.001,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 82,
            name: "meter_pf_phase_3".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 0.001,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 83,
            name: "meter_pf_total".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 0.001,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 84,
            name: "meter_frequency".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 85,
            name: "meter_e_import_active".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 86,
            name: "meter_e_import_reactive".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 87,
            name: "meter_e_export_active".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 88,
            name: "meter_e_export_reactive".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 89,
            name: "meter_reserved".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        // ================================================================
        // GivEnergy-native Holding Registers (HR 0-119)
        // Read via fn 0x03 (Read Holding Registers), slave 0x32
        // ================================================================
        RegisterDef {
            address: 0,
            name: "ge_hr_device_type".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 1,
            name: "ge_hr_module_high".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 2,
            name: "ge_hr_module_low".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 3,
            name: "ge_hr_mppt_phase_count".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 7,
            name: "ge_hr_enable_ammeter".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 8,
            name: "ge_hr_first_battery_serial_0".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 9,
            name: "ge_hr_first_battery_serial_1".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 10,
            name: "ge_hr_first_battery_serial_2".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 11,
            name: "ge_hr_first_battery_serial_3".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 12,
            name: "ge_hr_first_battery_serial_4".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        // HR 13-17: Serial number (5 regs, Latin-1, read-only)
        RegisterDef {
            address: 13,
            name: "ge_hr_serial_0".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 14,
            name: "ge_hr_serial_1".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 15,
            name: "ge_hr_serial_2".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 16,
            name: "ge_hr_serial_3".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 17,
            name: "ge_hr_serial_4".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 18,
            name: "ge_hr_first_battery_bms_firmware".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 19,
            name: "ge_hr_dsp_firmware".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 20,
            name: "ge_hr_enable_charge_target".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 21,
            name: "ge_hr_arm_firmware".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 22,
            name: "ge_hr_usb_device_inserted".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 23,
            name: "ge_hr_select_arm_chip".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 24,
            name: "ge_hr_variable_address".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 25,
            name: "ge_hr_variable_value".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 26,
            name: "ge_hr_grid_port_max_power_output".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 27,
            name: "ge_hr_battery_power_mode".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 28,
            name: "ge_hr_enable_60hz_freq_mode".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 29,
            name: "ge_hr_battery_calibration_stage".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 30,
            name: "ge_hr_modbus_address".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 31,
            name: "ge_hr_charge_slot_2_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 32,
            name: "ge_hr_charge_slot_2_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 35,
            name: "ge_hr_system_time_year".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 36,
            name: "ge_hr_system_time_month".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 37,
            name: "ge_hr_system_time_day".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 38,
            name: "ge_hr_system_time_hour".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 39,
            name: "ge_hr_system_time_minute".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 40,
            name: "ge_hr_system_time_second".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 33,
            name: "ge_hr_user_code".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 34,
            name: "ge_hr_modbus_version".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 41,
            name: "ge_hr_enable_drm_rj45_port".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 42,
            name: "ge_hr_enable_reversed_ct_clamp".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 43,
            name: "ge_hr_charge_discharge_soc".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 44,
            name: "ge_hr_discharge_slot_2_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 45,
            name: "ge_hr_discharge_slot_2_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 46,
            name: "ge_hr_bms_firmware_version".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 47,
            name: "ge_hr_meter_type".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 48,
            name: "ge_hr_enable_reversed_115_meter".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 49,
            name: "ge_hr_enable_reversed_418_meter".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 50,
            name: "ge_hr_active_power_rate".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 51,
            name: "ge_hr_reactive_power_rate".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 52,
            name: "ge_hr_power_factor".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 53,
            name: "ge_hr_enable_inverter_flags".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 54,
            name: "ge_hr_battery_type".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 55,
            name: "ge_hr_battery_capacity_ah".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 56,
            name: "ge_hr_discharge_slot_1_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 57,
            name: "ge_hr_discharge_slot_1_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 58,
            name: "ge_hr_enable_auto_judge_battery_type".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 59,
            name: "ge_hr_enable_discharge".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 60,
            name: "ge_hr_pv_start_voltage".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 61,
            name: "ge_hr_start_countdown_timer".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 62,
            name: "ge_hr_restart_delay_time".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 94,
            name: "ge_hr_charge_slot_1_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 95,
            name: "ge_hr_charge_slot_1_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 96,
            name: "ge_hr_enable_charge".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 97,
            name: "ge_hr_battery_low_voltage_protection_limit".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 98,
            name: "ge_hr_battery_high_voltage_protection_limit".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 105,
            name: "ge_hr_battery_voltage_adjust".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 108,
            name: "ge_hr_battery_low_force_charge_time".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 109,
            name: "ge_hr_enable_bms_read".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 110,
            name: "ge_hr_battery_soc_reserve".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 111,
            name: "ge_hr_battery_charge_limit".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 112,
            name: "ge_hr_battery_discharge_limit".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 116,
            name: "ge_hr_charge_target_soc".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 113,
            name: "ge_hr_enable_buzzer".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 114,
            name: "ge_hr_battery_discharge_min_power_reserve".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 115,
            name: "ge_hr_island_check_continue".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 117,
            name: "ge_hr_charge_soc_stop_2".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 118,
            name: "ge_hr_discharge_soc_stop_2".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 119,
            name: "ge_hr_charge_soc_stop_1".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 120,
            name: "ge_hr_discharge_soc_stop_1".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 121,
            name: "ge_hr_enable_local_command_test".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 122,
            name: "ge_hr_power_factor_function_model".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 123,
            name: "ge_hr_frequency_load_limit_rate".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 124,
            name: "ge_hr_enable_low_voltage_fault_ride_through".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 125,
            name: "ge_hr_enable_frequency_derating".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 126,
            name: "ge_hr_enable_above_6kw_system".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 127,
            name: "ge_hr_start_system_auto_test".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 128,
            name: "ge_hr_enable_spi".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 166,
            name: "ge_hr_rtc_enable".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 167,
            name: "ge_hr_threephase_balance_mode".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 168,
            name: "ge_hr_threephase_abc".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 169,
            name: "ge_hr_threephase_balance_1".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 170,
            name: "ge_hr_threephase_balance_2".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 171,
            name: "ge_hr_threephase_balance_3".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 175,
            name: "ge_hr_enable_battery_on_pv_or_grid".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 176,
            name: "ge_hr_debug_inverter".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 177,
            name: "ge_hr_enable_ups_mode".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 178,
            name: "ge_hr_enable_g100_limit_switch".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 179,
            name: "ge_hr_enable_battery_cable_impedance_alarm".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 199,
            name: "ge_hr_enable_inverter_parallel_mode".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 242,
            name: "ge_hr_charge_target_soc_1".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 243,
            name: "ge_hr_charge_slot_2_start_ext".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 244,
            name: "ge_hr_charge_slot_2_end_ext".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 245,
            name: "ge_hr_charge_target_soc_2".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 272,
            name: "ge_hr_discharge_target_soc_1".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 275,
            name: "ge_hr_discharge_target_soc_2".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 313,
            name: "ge_hr_battery_charge_limit_ac".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 314,
            name: "ge_hr_battery_discharge_limit_ac".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 163,
            name: "ge_hr_inverter_reboot".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 318,
            name: "ge_hr_battery_pause_mode".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 319,
            name: "ge_hr_battery_pause_slot_1_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 320,
            name: "ge_hr_battery_pause_slot_1_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        // Extended Gen3 schedule slots 3-10 (HR 246-299)
        RegisterDef {
            address: 246,
            name: "ge_hr_charge_slot_3_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 247,
            name: "ge_hr_charge_slot_3_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 248,
            name: "ge_hr_charge_target_soc_3".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 249,
            name: "ge_hr_charge_slot_4_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 250,
            name: "ge_hr_charge_slot_4_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 251,
            name: "ge_hr_charge_target_soc_4".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 252,
            name: "ge_hr_charge_slot_5_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 253,
            name: "ge_hr_charge_slot_5_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 254,
            name: "ge_hr_charge_target_soc_5".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 255,
            name: "ge_hr_charge_slot_6_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 256,
            name: "ge_hr_charge_slot_6_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 257,
            name: "ge_hr_charge_target_soc_6".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 258,
            name: "ge_hr_charge_slot_7_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 259,
            name: "ge_hr_charge_slot_7_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 260,
            name: "ge_hr_charge_target_soc_7".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 261,
            name: "ge_hr_charge_slot_8_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 262,
            name: "ge_hr_charge_slot_8_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 263,
            name: "ge_hr_charge_target_soc_8".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 264,
            name: "ge_hr_charge_slot_9_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 265,
            name: "ge_hr_charge_slot_9_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 266,
            name: "ge_hr_charge_target_soc_9".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 267,
            name: "ge_hr_charge_slot_10_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 268,
            name: "ge_hr_charge_slot_10_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 269,
            name: "ge_hr_charge_target_soc_10".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 276,
            name: "ge_hr_discharge_slot_3_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 277,
            name: "ge_hr_discharge_slot_3_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 278,
            name: "ge_hr_discharge_target_soc_3".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 279,
            name: "ge_hr_discharge_slot_4_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 280,
            name: "ge_hr_discharge_slot_4_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 281,
            name: "ge_hr_discharge_target_soc_4".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 282,
            name: "ge_hr_discharge_slot_5_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 283,
            name: "ge_hr_discharge_slot_5_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 284,
            name: "ge_hr_discharge_target_soc_5".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 285,
            name: "ge_hr_discharge_slot_6_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 286,
            name: "ge_hr_discharge_slot_6_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 287,
            name: "ge_hr_discharge_target_soc_6".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 288,
            name: "ge_hr_discharge_slot_7_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 289,
            name: "ge_hr_discharge_slot_7_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 290,
            name: "ge_hr_discharge_target_soc_7".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 291,
            name: "ge_hr_discharge_slot_8_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 292,
            name: "ge_hr_discharge_slot_8_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 293,
            name: "ge_hr_discharge_target_soc_8".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 294,
            name: "ge_hr_discharge_slot_9_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 295,
            name: "ge_hr_discharge_slot_9_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 296,
            name: "ge_hr_discharge_target_soc_9".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 297,
            name: "ge_hr_discharge_slot_10_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 298,
            name: "ge_hr_discharge_slot_10_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 299,
            name: "ge_hr_discharge_target_soc_10".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 311,
            name: "ge_hr_export_priority".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 317,
            name: "ge_hr_enable_eps".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1078,
            name: "tph_battery_discharge_min_power_reserve".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1108,
            name: "tph_battery_discharge_limit_ac".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1109,
            name: "tph_battery_soc_reserve".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1110,
            name: "tph_battery_charge_limit_ac".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1111,
            name: "tph_charge_target_soc".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1112,
            name: "tph_ac_charge_enable".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1113,
            name: "tph_charge_slot_1_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1114,
            name: "tph_charge_slot_1_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1115,
            name: "tph_charge_slot_2_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1116,
            name: "tph_charge_slot_2_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1118,
            name: "tph_discharge_slot_1_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1119,
            name: "tph_discharge_slot_1_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1120,
            name: "tph_discharge_slot_2_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1121,
            name: "tph_discharge_slot_2_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1122,
            name: "tph_force_discharge_enable".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 1123,
            name: "tph_force_charge_enable".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        // ============================================================
        // 3-Phase Input Registers (IR 1001-1413)
        // 3-phase clients read PV/Grid/Battery/EPS data from these
        // addresses, NOT the single-phase IR 0-59 block.
        // ============================================================
        // PV (IR 1001-1020)
        RegisterDef {
            address: 1001,
            name: "tph_ir_pv1_voltage".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1002,
            name: "tph_ir_pv2_voltage".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1009,
            name: "tph_ir_pv1_current".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1010,
            name: "tph_ir_pv2_current".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1017,
            name: "tph_ir_pv1_power_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1018,
            name: "tph_ir_pv1_power_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1019,
            name: "tph_ir_pv2_power_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1020,
            name: "tph_ir_pv2_power_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // Grid (IR 1061-1096)
        RegisterDef {
            address: 1061,
            name: "tph_ir_v_ac1".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1062,
            name: "tph_ir_v_ac2".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1063,
            name: "tph_ir_v_ac3".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1064,
            name: "tph_ir_i_ac1".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1065,
            name: "tph_ir_i_ac2".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1066,
            name: "tph_ir_i_ac3".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1067,
            name: "tph_ir_f_ac1".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1068,
            name: "tph_ir_power_factor".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1069,
            name: "tph_ir_p_inverter_out_high".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1070,
            name: "tph_ir_p_inverter_out_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1073,
            name: "tph_ir_p_grid_apparent_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1074,
            name: "tph_ir_p_grid_apparent_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1075,
            name: "tph_ir_system_mode".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1076,
            name: "tph_ir_status".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1079,
            name: "tph_ir_p_meter_import_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1080,
            name: "tph_ir_p_meter_import_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1081,
            name: "tph_ir_p_meter_export_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1082,
            name: "tph_ir_p_meter_export_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1083,
            name: "tph_ir_p_load_ac1".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1084,
            name: "tph_ir_p_load_ac2".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1085,
            name: "tph_ir_p_load_ac3".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1089,
            name: "tph_ir_p_load_all_high".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1090,
            name: "tph_ir_p_load_all_low".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // Battery (IR 1120-1140)
        RegisterDef {
            address: 1124,
            name: "tph_ir_dc_status".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1128,
            name: "tph_ir_t_inverter".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1129,
            name: "tph_ir_t_boost".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1130,
            name: "tph_ir_t_buck_boost".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // Battery block (IR 1131-1140)
        RegisterDef {
            address: 1131,
            name: "tph_ir_v_battery_bms".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1133,
            name: "tph_ir_v_battery_pcs".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1134,
            name: "tph_ir_v_dc_bus".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1135,
            name: "tph_ir_v_inv_bus".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1132,
            name: "tph_ir_battery_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1136,
            name: "tph_ir_p_battery_discharge_high".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1137,
            name: "tph_ir_p_battery_discharge_low".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1138,
            name: "tph_ir_p_battery_charge_high".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1139,
            name: "tph_ir_p_battery_charge_low".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1140,
            name: "tph_ir_i_battery".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // EPS (IR 1180-1192)
        RegisterDef {
            address: 1180,
            name: "tph_ir_f_nominal_eps".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1181,
            name: "tph_ir_v_eps_ac1".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1182,
            name: "tph_ir_v_eps_ac2".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1183,
            name: "tph_ir_v_eps_ac3".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // Additional power meters / CT exports (IR 1240-1245)
        RegisterDef {
            address: 1240,
            name: "tph_ir_p_export_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1241,
            name: "tph_ir_p_export_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1244,
            name: "tph_ir_p_meter2_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1245,
            name: "tph_ir_p_meter2_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // Firmware (IR 1325-1327)
        RegisterDef {
            address: 1325,
            name: "tph_ir_ac_dsp_firmware_version".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1326,
            name: "tph_ir_dc_dsp_firmware_version".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1327,
            name: "tph_ir_arm_firmware_version".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        // Energy totals (IR 1360-1413)
        RegisterDef {
            address: 1366,
            name: "tph_ir_e_pv1_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1367,
            name: "tph_ir_e_pv1_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1370,
            name: "tph_ir_e_pv2_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1371,
            name: "tph_ir_e_pv2_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1380,
            name: "tph_ir_e_import_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1381,
            name: "tph_ir_e_import_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1384,
            name: "tph_ir_e_export_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1385,
            name: "tph_ir_e_export_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1388,
            name: "tph_ir_e_battery_discharge_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1389,
            name: "tph_ir_e_battery_discharge_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1392,
            name: "tph_ir_e_battery_charge_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1393,
            name: "tph_ir_e_battery_charge_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1396,
            name: "tph_ir_e_load_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1397,
            name: "tph_ir_e_load_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // ============================================================
        // Additional three-phase energy registers (IR 1360-1413)
        // Most uint32 pairs: today + total (lifetime) counterparts.
        // The simulator uses the same cumulative bucket for both since
        // EnergyTracker has no daily-reset counter.
        // ============================================================

        // IR 1360-1361: e_inverter_out_today (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1360,
            name: "tph_ir_e_inverter_out_today_high".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1361,
            name: "tph_ir_e_inverter_out_today_low".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1362-1363: e_inverter_out_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1362,
            name: "tph_ir_e_inverter_out_total_high".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1363,
            name: "tph_ir_e_inverter_out_total_low".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1368-1369: e_pv1_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1368,
            name: "tph_ir_e_pv1_total_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1369,
            name: "tph_ir_e_pv1_total_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1372-1373: e_pv2_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1372,
            name: "tph_ir_e_pv2_total_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1373,
            name: "tph_ir_e_pv2_total_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1374-1375: e_pv_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1374,
            name: "tph_ir_e_pv_total_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1375,
            name: "tph_ir_e_pv_total_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1376-1377: e_ac_charge_today (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1376,
            name: "tph_ir_e_ac_charge_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1377,
            name: "tph_ir_e_ac_charge_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1378-1379: e_ac_charge_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1378,
            name: "tph_ir_e_ac_charge_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1379,
            name: "tph_ir_e_ac_charge_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1382-1383: e_import_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1382,
            name: "tph_ir_e_import_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1383,
            name: "tph_ir_e_import_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1386-1387: e_export_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1386,
            name: "tph_ir_e_export_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1387,
            name: "tph_ir_e_export_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1390-1391: e_battery_discharge_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1390,
            name: "tph_ir_e_battery_discharge_total_high".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1391,
            name: "tph_ir_e_battery_discharge_total_low".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1394-1395: e_battery_charge_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1394,
            name: "tph_ir_e_battery_charge_total_high".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1395,
            name: "tph_ir_e_battery_charge_total_low".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1398-1399: e_load_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1398,
            name: "tph_ir_e_load_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1399,
            name: "tph_ir_e_load_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1400-1401: e_export2_today (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1400,
            name: "tph_ir_e_export2_today_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1401,
            name: "tph_ir_e_export2_today_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1402-1403: e_export2_total (uint32 ×0.1 kWh)
        RegisterDef {
            address: 1402,
            name: "tph_ir_e_export2_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1403,
            name: "tph_ir_e_export2_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        // IR 1412-1413: e_pv_today (uint32 ×0.1 kWh) — combined PV today
        RegisterDef {
            address: 1412,
            name: "tph_ir_e_pv_today_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 1413,
            name: "tph_ir_e_pv_today_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2040,
            name: "ems_plant_enable".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2041,
            name: "ems_expected_inverter_count".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2042,
            name: "ems_expected_meter_count".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2043,
            name: "ems_expected_car_charger_count".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2044,
            name: "ems_discharge_slot_1_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2045,
            name: "ems_discharge_slot_1_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2046,
            name: "ems_discharge_target_soc_1".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2047,
            name: "ems_discharge_slot_2_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2048,
            name: "ems_discharge_slot_2_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2049,
            name: "ems_discharge_target_soc_2".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2050,
            name: "ems_discharge_slot_3_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2051,
            name: "ems_discharge_slot_3_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2052,
            name: "ems_discharge_target_soc_3".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2053,
            name: "ems_charge_slot_1_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2054,
            name: "ems_charge_slot_1_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2055,
            name: "ems_charge_target_soc_1".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2056,
            name: "ems_charge_slot_2_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2057,
            name: "ems_charge_slot_2_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2058,
            name: "ems_charge_target_soc_2".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2059,
            name: "ems_charge_slot_3_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2060,
            name: "ems_charge_slot_3_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2061,
            name: "ems_charge_target_soc_3".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2062,
            name: "ems_export_slot_1_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2063,
            name: "ems_export_slot_1_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2064,
            name: "ems_export_target_soc_1".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2065,
            name: "ems_export_slot_2_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2066,
            name: "ems_export_slot_2_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2067,
            name: "ems_export_target_soc_2".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2068,
            name: "ems_export_slot_3_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2069,
            name: "ems_export_slot_3_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2070,
            name: "ems_export_target_soc_3".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2071,
            name: "ems_export_power_limit".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2072,
            name: "ems_car_charge_mode".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2073,
            name: "ems_car_charge_boost".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2074,
            name: "ems_plant_charge_compensation".into(),
            category: C::Configuration,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2075,
            name: "ems_plant_discharge_compensation".into(),
            category: C::Configuration,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        // ================================================================
        // EMS Input Registers (IR 2040-2095) — plant status & telemetry
        // ================================================================
        RegisterDef {
            address: 2040,
            name: "ems_ir_status".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2041,
            name: "ems_ir_meter_count".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2042,
            name: "ems_ir_meter_types".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2043,
            name: "ems_ir_meter_status".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2044,
            name: "ems_ir_inverter_count".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2045,
            name: "ems_ir_inverter_status".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2046,
            name: "ems_ir_meter_1_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2047,
            name: "ems_ir_meter_2_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2048,
            name: "ems_ir_meter_3_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2049,
            name: "ems_ir_meter_4_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2050,
            name: "ems_ir_meter_5_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2051,
            name: "ems_ir_meter_6_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2052,
            name: "ems_ir_meter_7_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2053,
            name: "ems_ir_meter_8_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2054,
            name: "ems_ir_inverter_1_power".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2055,
            name: "ems_ir_inverter_2_power".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2056,
            name: "ems_ir_inverter_3_power".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2057,
            name: "ems_ir_inverter_4_power".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2058,
            name: "ems_ir_inverter_1_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2059,
            name: "ems_ir_inverter_2_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2060,
            name: "ems_ir_inverter_3_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2061,
            name: "ems_ir_inverter_4_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2062,
            name: "ems_ir_inverter_1_temp".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2063,
            name: "ems_ir_inverter_2_temp".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2064,
            name: "ems_ir_inverter_3_temp".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2065,
            name: "ems_ir_inverter_4_temp".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2066,
            name: "ems_ir_inverter_1_serial".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2086,
            name: "ems_ir_calc_load_power".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2087,
            name: "ems_ir_measured_load_power".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2088,
            name: "ems_ir_total_generation_load_power".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2089,
            name: "ems_ir_grid_meter_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2090,
            name: "ems_ir_total_battery_power".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2091,
            name: "ems_ir_remaining_battery_wh".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2094,
            name: "ems_ir_other_battery_power".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        // Added EMS serial registers (IR 2067-2085, step through inverter 2-4)
        RegisterDef {
            address: 2071,
            name: "ems_ir_inverter_2_serial".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2076,
            name: "ems_ir_inverter_3_serial".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        RegisterDef {
            address: 2081,
            name: "ems_ir_inverter_4_serial".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Input,
        },
        // ================================================================
        // Smart Load slots (HR 554-573) — 10 start/end pairs
        // ================================================================
        RegisterDef {
            address: 554,
            name: "ge_hr_smart_load_slot_1_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 555,
            name: "ge_hr_smart_load_slot_1_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 556,
            name: "ge_hr_smart_load_slot_2_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 557,
            name: "ge_hr_smart_load_slot_2_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 558,
            name: "ge_hr_smart_load_slot_3_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 559,
            name: "ge_hr_smart_load_slot_3_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 560,
            name: "ge_hr_smart_load_slot_4_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 561,
            name: "ge_hr_smart_load_slot_4_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 562,
            name: "ge_hr_smart_load_slot_5_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 563,
            name: "ge_hr_smart_load_slot_5_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 564,
            name: "ge_hr_smart_load_slot_6_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 565,
            name: "ge_hr_smart_load_slot_6_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 566,
            name: "ge_hr_smart_load_slot_7_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 567,
            name: "ge_hr_smart_load_slot_7_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 568,
            name: "ge_hr_smart_load_slot_8_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 569,
            name: "ge_hr_smart_load_slot_8_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 570,
            name: "ge_hr_smart_load_slot_9_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 571,
            name: "ge_hr_smart_load_slot_9_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 572,
            name: "ge_hr_smart_load_slot_10_start".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 573,
            name: "ge_hr_smart_load_slot_10_end".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        // ================================================================
        // High registers (HR 4107-4114) — PV power setting, battery energy alt sources
        // ================================================================
        RegisterDef {
            address: 4107,
            name: "ge_hr_pv_power_setting_high".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 4108,
            name: "ge_hr_pv_power_setting_low".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 4109,
            name: "ge_hr_battery_discharge_total_alt2_high".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 4110,
            name: "ge_hr_battery_discharge_total_alt2_low".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 4111,
            name: "ge_hr_battery_charge_total_alt2_high".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 4112,
            name: "ge_hr_battery_charge_total_alt2_low".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 4113,
            name: "ge_hr_battery_discharge_today_alt3".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 4114,
            name: "ge_hr_battery_charge_today_alt3".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 4141,
            name: "ge_hr_inverter_export_total_high".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 4142,
            name: "ge_hr_inverter_export_total_low".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        // ================================================================
        // Simulator-internal registers (100+, 200+, etc.)
        // All in holding register space.
        // ================================================================

        // ---- Inverter (100–119) ----
        RegisterDef {
            address: 100,
            name: "inverter_mode".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 101,
            name: "inverter_ac_power".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 102,
            name: "inverter_export_limit_w".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 103,
            name: "inverter_temperature".into(),
            category: C::Inverter,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 104,
            name: "inverter_firmware_state".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        // ---- Battery (200–239) ----
        RegisterDef {
            address: 200,
            name: "battery_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 201,
            name: "battery_soc_2".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 202,
            name: "battery_soc_3".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 203,
            name: "battery_power".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 204,
            name: "battery_voltage".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 205,
            name: "battery_current".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 206,
            name: "battery_temperature".into(),
            category: C::Battery,
            typ: T::S16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 207,
            name: "battery_capacity_kwh".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 208,
            name: "battery_max_charge_kw".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 209,
            name: "battery_max_discharge_kw".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 210,
            name: "battery_min_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 211,
            name: "battery_max_soc".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 212,
            name: "battery_charge_efficiency".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 213,
            name: "battery_discharge_efficiency".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 214,
            name: "battery_module_count".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        // ---- PV / Solar (300–319) ----
        RegisterDef {
            address: 300,
            name: "pv_generation".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 301,
            name: "pv_voltage".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 302,
            name: "pv_current".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 303,
            name: "pv_energy_today".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 304,
            name: "pv_peak_capacity".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        // ---- Grid (400–419) ----
        RegisterDef {
            address: 400,
            name: "grid_power".into(),
            category: C::Grid,
            typ: T::S16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 401,
            name: "grid_voltage".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 402,
            name: "grid_frequency".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.01,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 403,
            name: "grid_connected".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 404,
            name: "load_power".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        // ---- Energy Totals (500–519) ----
        RegisterDef {
            address: 500,
            name: "grid_import_kwh".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 501,
            name: "grid_export_kwh".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 502,
            name: "battery_charge_kwh".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 503,
            name: "battery_discharge_kwh".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 504,
            name: "solar_generation_kwh".into(),
            category: C::PV,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 505,
            name: "load_consumption_kwh".into(),
            category: C::Grid,
            typ: T::U16,
            scaling_factor: 0.1,
            access: ReadOnly,
            space: Holding,
        },
        // ---- Configuration (600–619) ----
        RegisterDef {
            address: 600,
            name: "config_battery_count".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 601,
            name: "config_tick_interval".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 602,
            name: "config_weather".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        // ---- Schedules (700–719) ----
        RegisterDef {
            address: 700,
            name: "schedule_charge_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 701,
            name: "schedule_charge_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 702,
            name: "schedule_discharge_start".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 703,
            name: "schedule_discharge_end".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 704,
            name: "schedule_charge_target_soc".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 705,
            name: "schedule_discharge_target_soc".into(),
            category: C::Schedules,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sim_models::PlantState;

    fn test_ts() -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap()
    }

    #[test]
    fn project_maps_state_to_registers() {
        let mut state = PlantState::new(test_ts());
        state.solar.generation_w = 3500.0;
        state.batteries[0].soc_percent = 75.0;
        state.sync_battery_from_vec();

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        assert_eq!(store.read(300), Some(3500)); // pv_generation
        assert_eq!(store.read(200), Some(75)); // battery_soc (aggregate)
    }

    #[test]
    fn write_respects_access_control() {
        let mut store = RegisterStore::new(default_register_catalogue());
        assert!(store.write(100, 2)); // inverter_mode = ReadWrite
        assert!(!store.write(200, 50)); // battery_soc = ReadOnly
    }

    #[test]
    fn snapshot_captures_all_registers() {
        let state = PlantState::new(test_ts());

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        let snap = store.snapshot();
        assert_eq!(snap.len(), default_register_catalogue().len());
    }

    #[test]
    fn project_maps_inverter_mode() {
        let mut state = PlantState::new(test_ts());
        state
            .inverter
            .mode_state
            .set_user(sim_models::InverterMode::ForceCharge);

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        assert_eq!(store.read(100), Some(2)); // ForceCharge = 2
    }

    #[test]
    fn project_maps_negative_grid_power() {
        let mut state = PlantState::new(test_ts());
        state.grid.power_w = -1500.0;

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        let raw = store.read(400).unwrap();
        let signed = raw as i16;
        assert!(signed < 0, "Expected negative grid power, got {signed}");
    }

    #[test]
    fn multi_battery_registers() {
        let mut state = PlantState::with_battery_count(test_ts(), 3);
        state.batteries[0].soc_percent = 60.0;
        state.batteries[1].soc_percent = 70.0;
        state.batteries[2].soc_percent = 80.0;
        state.sync_battery_from_vec();

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        assert_eq!(store.read(200), Some(70)); // aggregate SOC-weighted avg
        assert_eq!(store.read(201), Some(70)); // battery 2 SOC
        assert_eq!(store.read(202), Some(80)); // battery 3 SOC
    }

    #[test]
    fn supports_up_to_six_battery_modules() {
        // GIV-BAT-HV modular systems stack up to 6 x GIV-BAT-3.4-HV modules.
        // GivEnergy probes slave addresses 0x32-0x37 for up to 6 batteries.
        let state = PlantState::with_battery_count(test_ts(), 6);
        assert_eq!(state.batteries.len(), 6, "6 modules should be created");
        // Clamps above 6 stay at 6.
        let clamped = PlantState::with_battery_count(test_ts(), 99);
        assert_eq!(clamped.batteries.len(), 6);

        // BMS projection is index-agnostic: each of the 6 modules must yield
        // valid IR 60-119 cell/temperature data via project_battery_bms.
        let store = RegisterStore::new(default_register_catalogue());
        for i in 0..6 {
            let bms = store.project_battery_bms(&state.batteries[i], i);
            // IR 60 (first cell voltage, mV) must be non-zero for every module.
            assert_ne!(bms[0], 0, "module {i} BMS data must be populated");
        }
    }

    #[test]
    fn hv_bms_discovery_reports_one_bcu() {
        // Cold HV discovery reads IR(60,5) at slave 0xA0; IR(61) = number of BCUs.
        // Our single-stack model always reports 1 BCU.
        let store = RegisterStore::new(default_register_catalogue());
        let bms = store.project_battery_bms_discovery(1);
        assert_eq!(bms[0], 1, "IR(60) BMS version nibble must be non-zero");
        assert_eq!(bms[1], 1, "IR(61) must report 1 BCU for a single stack");
    }

    #[test]
    fn hv_bcu_cluster_decodes_valid_and_aggregates_modules() {
        // The user's GIV-BAT-17.0-HV case: 5 x GIV-BAT-3.4-HV modules behind a
        // ThreePhase inverter. The BCU at slave 0x70 must (a) decode to a valid
        // pack_software_version so Bcu.is_valid() holds, and (b) aggregate the
        // 5 modules into one cluster view.
        let mut state = PlantState::with_battery_count(test_ts(), 5);
        for b in &mut state.batteries {
            b.soc_percent = 60.0;
            b.voltage_v = 51.2;
            b.soh = 0.95;
        }
        state.sync_battery_from_vec();

        let store = RegisterStore::new(default_register_catalogue());
        let bcu = store.project_battery_bcu(&state.batteries);

        // IR 60-63 pack_software_version decodes via gateway_version to
        // "HV00000001" — not blank, not all '0', so Bcu.is_valid() is true.
        let prefix = [bcu[0], bcu[1]]
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .map(|b| b as char)
            .collect::<String>();
        assert_eq!(prefix, "HV00", "version prefix must be non-blank");

        // IR 64 number_of_modules = 5
        assert_eq!(bcu[4], 5, "IR(64) must report 5 modules");
        // IR 65 cells_per_module = 24
        assert_eq!(bcu[5], 24, "IR(65) must report 24 cells/module");
        // IR 73 battery_voltage (deci V) = 5 modules x 51.2 V summed = 2560
        assert_eq!(
            bcu[13], 2560,
            "IR(73) stack voltage must sum module voltages"
        );
        // IR 80 SOC duint8: high=max, low=min. All modules 60% -> 0x3C3C
        assert_eq!(bcu[20], (60 << 8) | 60, "IR(80) SOC max/min");
        // IR 81 SOH = 95%
        assert_eq!(bcu[21], 95, "IR(81) SOH");
        // IR 98 nominal_capacity = 51.0 Ah * 10 = 510
        assert_eq!(bcu[38], 510, "IR(98) per-module nominal Ah");
        // IR 99 remaining = 51 Ah * 60% * 10 = 306
        assert_eq!(bcu[39], 306, "IR(99) remaining capacity scales with SOC");
    }

    #[test]
    fn hv_bmu_per_module_decodes_valid_serial_and_cells() {
        // Each BMU at slave 0x50+m must (a) decode to a non-blank serial so
        // Bmu.is_valid() holds, and (b) populate 24 cell voltages/temps.
        let state = PlantState::with_battery_count(test_ts(), 5);
        let store = RegisterStore::new(default_register_catalogue());

        for m in 0..5 {
            let bmu = store.project_battery_bmu(&state.batteries[m], m);
            // IR 60-83: 24 cell voltages, all non-zero
            for (i, &v) in bmu.iter().take(24).enumerate() {
                assert_ne!(v, 0, "module {m} cell {i} voltage must be non-zero");
            }
            // IR 90-113: 24 cell temperatures, all non-zero
            for (i, &v) in bmu.iter().skip(30).take(24).enumerate() {
                assert_ne!(v, 0, "module {m} cell {i} temp must be non-zero");
            }
            // IR 114-118 serial: at least one register must carry a non-space byte
            // so the decoded serial is non-blank.
            let sn_bytes: Vec<u8> = (0..5).flat_map(|r| bmu[54 + r].to_be_bytes()).collect();
            assert!(
                sn_bytes.iter().any(|&b| b != b' ' && b != 0),
                "module {m} serial must be non-blank for Bmu.is_valid()"
            );
        }
    }

    #[test]
    fn givenergy_input_registers_populated() {
        let mut state = PlantState::new(test_ts());
        state.solar.generation_w = 4000.0;
        state.solar.pv1_w = 4000.0;
        state.solar.pv2_w = 0.0;
        state.grid.power_w = -500.0;
        state.batteries[0].soc_percent = 65.0;
        state.sync_battery_from_vec();

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        // Read via input register space
        assert_eq!(store.read_by_space(0, RegisterSpace::Input), Some(1)); // status
        assert_eq!(store.read_by_space(1, RegisterSpace::Input), Some(3500)); // PV1 voltage ×0.1
        assert_eq!(store.read_by_space(18, RegisterSpace::Input), Some(4000)); // PV1 power (4000W on array 1)
        assert_eq!(store.read_by_space(59, RegisterSpace::Input), Some(65)); // SOC
    }

    #[test]
    fn givenergy_holding_registers_populated() {
        let mut state = PlantState::new(test_ts());
        state
            .inverter
            .mode_state
            .set_user(sim_models::InverterMode::Eco);

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        // HR 27: battery power mode = 1 (eco)
        assert_eq!(store.read_by_space(27, RegisterSpace::Holding), Some(1));
        // HR 0: device type
        assert_eq!(store.read_by_space(0, RegisterSpace::Holding), Some(0x2001));
    }

    #[test]
    fn input_and_holding_dont_collide() {
        let state = PlantState::new(test_ts());
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        // IR 0 = status (1), HR 0 = device type (0x2001) — must differ
        let ir0 = store.read_by_space(0, RegisterSpace::Input);
        let hr0 = store.read_by_space(0, RegisterSpace::Holding);
        assert_eq!(ir0, Some(1));
        assert_eq!(hr0, Some(0x2001));
        assert_ne!(ir0, hr0, "IR 0 and HR 0 must not collide");
    }

    #[test]
    fn ge_input_reference_energy_registers_project() {
        let mut state = PlantState::new(test_ts());
        state.energy_totals.solar_generation_kwh = 12.5;
        state.energy_totals.grid_export_kwh = 3.0;
        state.energy_totals.grid_import_kwh = 1.5;
        state.energy_totals.ac_charge_kwh = 0.7;
        state.batteries[0].throughput_kwh = 8.4;
        state.sync_battery_from_vec();

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        assert_eq!(store.read_by_space(6, RegisterSpace::Input), Some(0));
        assert_eq!(store.read_by_space(7, RegisterSpace::Input), Some(84));
        assert_eq!(store.read_by_space(11, RegisterSpace::Input), Some(0));
        assert_eq!(store.read_by_space(12, RegisterSpace::Input), Some(125));
        assert_eq!(store.read_by_space(21, RegisterSpace::Input), Some(0));
        assert_eq!(store.read_by_space(22, RegisterSpace::Input), Some(30));
        assert_eq!(store.read_by_space(32, RegisterSpace::Input), Some(0));
        assert_eq!(store.read_by_space(33, RegisterSpace::Input), Some(15));
        assert_eq!(store.read_by_space(35, RegisterSpace::Input), Some(7));
        assert_eq!(store.read_by_space(44, RegisterSpace::Input), Some(125));
        assert_eq!(store.read_by_space(45, RegisterSpace::Input), Some(0));
        assert_eq!(store.read_by_space(46, RegisterSpace::Input), Some(125));
    }

    #[test]
    fn zero_energy_state_projects_fixture_energy_registers_for_all_inverter_types() {
        let inverter_types = [
            "Gen1Hybrid",
            "Gen2Hybrid",
            "Gen3Hybrid",
            "Gen3Hybrid8kW",
            "Gen3Hybrid10kW",
            "Gen3Plus6kW",
            "Gen3Plus4600",
            "Gen3Plus3600",
            "Gen3Plus6kW2",
            "ACCoupled",
            "ACCoupled2",
            "ThreePhase",
            "ThreePhase8kW",
            "ThreePhase10kW",
            "ThreePhase11kW",
            "ACThreePhase",
            "AllInOne6",
            "AllInOne",
            "AllInOne5",
            "AIO8kW",
            "AIO10kW",
            "AIOHybrid6kW",
            "AIOHybrid8kW",
            "AIOHybrid10kW",
        ];

        for inv_type in inverter_types {
            let mut state = PlantState::new(test_ts());
            state.config.inverter_type = inv_type.to_string();
            let mut store = RegisterStore::new(default_register_catalogue());
            store.project_from_state(&state);

            assert!(
                store.read_by_space(17, RegisterSpace::Input).unwrap_or(0) > 0,
                "{inv_type}: PV today should be non-zero"
            );
            assert!(
                store.read_by_space(25, RegisterSpace::Input).unwrap_or(0) > 0,
                "{inv_type}: grid export today should be non-zero"
            );
            assert!(
                store.read_by_space(26, RegisterSpace::Input).unwrap_or(0) > 0,
                "{inv_type}: grid import today should be non-zero"
            );
            assert!(
                store.read_by_space(36, RegisterSpace::Input).unwrap_or(0) > 0,
                "{inv_type}: battery charge today should be non-zero"
            );
            assert!(
                store.read_by_space(37, RegisterSpace::Input).unwrap_or(0) > 0,
                "{inv_type}: battery discharge today should be non-zero"
            );
            assert!(
                store.read_by_space(44, RegisterSpace::Input).unwrap_or(0) > 0,
                "{inv_type}: PV generation today should be non-zero"
            );

            if inv_type.starts_with("ThreePhase") || inv_type == "ACThreePhase" {
                assert!(
                    read_u32_ir(&store, 1366, 1367) > 0,
                    "{inv_type}: 3-phase PV1 today should be non-zero"
                );
                assert!(
                    read_u32_ir(&store, 1380, 1381) > 0,
                    "{inv_type}: 3-phase grid import today should be non-zero"
                );
                assert!(
                    read_u32_ir(&store, 1384, 1385) > 0,
                    "{inv_type}: 3-phase grid export today should be non-zero"
                );
                assert!(
                    read_u32_ir(&store, 1388, 1389) > 0,
                    "{inv_type}: 3-phase battery discharge today should be non-zero"
                );
                assert!(
                    read_u32_ir(&store, 1392, 1393) > 0,
                    "{inv_type}: 3-phase battery charge today should be non-zero"
                );
                assert!(
                    read_u32_ir(&store, 1396, 1397) > 0,
                    "{inv_type}: 3-phase load today should be non-zero"
                );
            }
        }
    }

    #[test]
    fn gen1_hybrid_uses_real_hybrid_dtc_with_gen1_firmware_century() {
        let now = test_ts();
        let mut s = PlantState::new(now);
        s.config.inverter_type = "Gen1Hybrid".to_string();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        // 0x2001 is the family code for ALL Gen1/Gen2/Gen3 hybrids.
        // HR(21) century 2 (arm_fw 252) disambiguates as Gen1Hybrid.
        assert_eq!(store.read_by_space(0, RegisterSpace::Holding), Some(0x2001));
        assert_eq!(store.read_by_space(21, RegisterSpace::Holding), Some(252));
    }

    #[test]
    fn gen2_hybrid_shares_family_dtc_but_reports_century_8_firmware() {
        let now = test_ts();
        let mut s = PlantState::new(now);
        s.config.inverter_type = "Gen2Hybrid".to_string();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        // Same DTC as Gen1/Gen3, but HR(21) century 8 → Gen2.
        assert_eq!(store.read_by_space(0, RegisterSpace::Holding), Some(0x2001));
        assert_eq!(store.read_by_space(21, RegisterSpace::Holding), Some(852));
    }

    #[test]
    fn dsp_firmware_projects_from_inverter_state() {
        let now = test_ts();
        let mut s = PlantState::new(now);
        s.inverter.dsp_firmware_version = 1234;
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(19, RegisterSpace::Holding), Some(1234));
    }

    #[test]
    fn arm_firmware_override_takes_precedence_over_type_default() {
        let now = test_ts();
        let mut s = PlantState::new(now);
        // Default Gen3Hybrid arm_fw is 318 (century 3)
        s.config.inverter_type = "Gen3Hybrid".to_string();
        // Override to simulate Gen2 identification
        s.inverter.arm_firmware_version = 852;
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(21, RegisterSpace::Holding), Some(852));
    }

    #[test]
    fn arm_firmware_falls_back_to_type_default_when_zero() {
        let now = test_ts();
        let mut s = PlantState::new(now);
        s.config.inverter_type = "Gen3Hybrid".to_string();
        s.inverter.arm_firmware_version = 0; // sentinel: use type default
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(21, RegisterSpace::Holding), Some(318));
    }

    #[test]
    fn extended_givtcp_schedule_registers_are_catalogued_and_projected() {
        let mut store = RegisterStore::new(default_register_catalogue());
        let sched = sim_models::Schedule {
            charge_start: 1.5,
            charge_end: 2.5,
            charge_start_2: 3.0,
            charge_end_2: 4.0,
            charge_target_soc: 80.0,
            charge_target_soc_2: 90.0,
            discharge_start: 18.0,
            discharge_end: 19.0,
            discharge_start_2: 20.0,
            discharge_end_2: 21.0,
            discharge_target_soc: 20.0,
            discharge_target_soc_2: 30.0,
            enable_charge: true,
            enable_discharge: true,
            ..Default::default()
        };

        store.project_schedule(&sched);

        for addr in [
            242, 243, 244, 245, 272, 275, 1109, 1111, 1112, 1113, 1114, 1115, 1116, 1118, 1119,
            1120, 1121, 1122, 1123,
        ] {
            assert!(
                store.write(addr, 42),
                "HR {addr} should be catalogued ReadWrite"
            );
        }
        store.project_schedule(&sched);
        assert_eq!(store.read_by_space(242, RegisterSpace::Holding), Some(80));
        assert_eq!(store.read_by_space(243, RegisterSpace::Holding), Some(300));
        assert_eq!(store.read_by_space(244, RegisterSpace::Holding), Some(400));
        assert_eq!(store.read_by_space(245, RegisterSpace::Holding), Some(90));
        assert_eq!(store.read_by_space(272, RegisterSpace::Holding), Some(20));
        assert_eq!(store.read_by_space(275, RegisterSpace::Holding), Some(30));
        assert_eq!(store.read_by_space(1109, RegisterSpace::Holding), Some(10));
        assert_eq!(store.read_by_space(1111, RegisterSpace::Holding), Some(80));
        assert_eq!(store.read_by_space(1113, RegisterSpace::Holding), Some(130));
        assert_eq!(store.read_by_space(1115, RegisterSpace::Holding), Some(300));
        // HR 1122/1123 are force charge/discharge registers projected from
        // inverter mode, not schedule — they keep their last-written value.
        assert_eq!(store.read_by_space(1122, RegisterSpace::Holding), Some(42));
        assert_eq!(store.read_by_space(1123, RegisterSpace::Holding), Some(42));
    }

    // ===================================================================
    // GivEnergy Holding Register Tests
    // ===================================================================

    fn make_state() -> PlantState {
        let mut s = PlantState::new(test_ts());
        s.solar.generation_w = 4000.0;
        s.grid.power_w = -800.0;
        s.batteries[0].soc_percent = 75.0;
        s.sync_battery_from_vec();
        s
    }

    #[test]
    fn ge_holding_system_time_registers() {
        let state = make_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        assert_eq!(store.read_by_space(35, RegisterSpace::Holding), Some(2025));
        assert_eq!(store.read_by_space(36, RegisterSpace::Holding), Some(6));
        assert_eq!(store.read_by_space(37, RegisterSpace::Holding), Some(1));
        assert_eq!(store.read_by_space(38, RegisterSpace::Holding), Some(12));
        assert_eq!(store.read_by_space(39, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(40, RegisterSpace::Holding), Some(0));
    }

    #[test]
    fn ge_holding_schedule_slots_disabled_by_default() {
        let state = make_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        for addr in [31, 32, 44, 45, 56, 57, 94, 95] {
            assert_eq!(
                store.read_by_space(addr, RegisterSpace::Holding),
                Some(60),
                "HR {addr} should be 60 (disabled)"
            );
        }
    }

    #[test]
    fn ge_holding_pause_mode_disabled() {
        let state = make_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        assert_eq!(store.read_by_space(318, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(319, RegisterSpace::Holding), Some(60));
        assert_eq!(store.read_by_space(320, RegisterSpace::Holding), Some(60));
    }

    #[test]
    fn ge_holding_calibration_and_reboot() {
        let state = make_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        assert_eq!(store.read_by_space(29, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(163, RegisterSpace::Holding), Some(0));
    }

    #[test]
    fn power_limit_registers_default_to_100_percent() {
        let state = PlantState::new(test_ts());
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        assert_eq!(store.read_by_space(111, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(112, RegisterSpace::Holding), Some(100));
        // AC-coupled and 3-phase mirrors of the same fields
        assert_eq!(store.read_by_space(313, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(314, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(1108, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(1110, RegisterSpace::Holding), Some(100));
    }

    #[test]
    fn new_reference_input_registers_project_from_state() {
        let mut state = make_state();
        state.inverter.ac_power_w = 3200.0;
        state.load.demand_w = 1250.0;
        state.inverter.work_time_hours = 1234.0;
        state.solar.generation_w = 4321.0;
        state.energy_totals.battery_charge_kwh = 5.5;
        state.energy_totals.battery_discharge_kwh = 4.4;
        state.active_faults.push("grid_loss".to_string());

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        assert_eq!(store.read_by_space(24, RegisterSpace::Input), Some(3200));
        assert_eq!(store.read_by_space(42, RegisterSpace::Input), Some(1250));
        assert_eq!(store.read_by_space(43, RegisterSpace::Input), Some(3200));
        assert_eq!(store.read_by_space(47, RegisterSpace::Input), Some(0));
        assert_eq!(store.read_by_space(48, RegisterSpace::Input), Some(1234));
        assert_eq!(store.read_by_space(180, RegisterSpace::Input), Some(44));
        assert_eq!(store.read_by_space(181, RegisterSpace::Input), Some(55));
        assert_eq!(store.read_by_space(182, RegisterSpace::Input), Some(44));
        assert_eq!(store.read_by_space(183, RegisterSpace::Input), Some(55));
        assert_eq!(store.read_by_space(247, RegisterSpace::Input), Some(0));
        assert_eq!(store.read_by_space(248, RegisterSpace::Input), Some(4321));

        let fault_code = ((store.read_by_space(39, RegisterSpace::Input).unwrap() as u32) << 16)
            | store.read_by_space(40, RegisterSpace::Input).unwrap() as u32;
        assert_ne!(fault_code, 0, "active faults should project into IR 39-40");
    }

    #[test]
    fn inverter_parallel_mode_register_is_projected_and_writable() {
        let mut state = make_state();
        state.enable_inverter_parallel_mode = true;
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        assert_eq!(store.read_by_space(199, RegisterSpace::Holding), Some(1));
        assert!(store.write(199, 0));
    }

    #[test]
    fn high_energy_alt_holding_registers_project() {
        let mut state = make_state();
        state.config.solar_peak_watts = 6000.0;
        state.energy_totals.battery_charge_kwh = 2.5;
        state.energy_totals.battery_discharge_kwh = 3.5;
        state.energy_totals.grid_export_kwh = 4.5;
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        assert_eq!(store.read_by_space(4107, RegisterSpace::Holding), Some(0));
        assert_eq!(
            store.read_by_space(4108, RegisterSpace::Holding),
            Some(6000)
        );
        assert_eq!(store.read_by_space(4109, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(4110, RegisterSpace::Holding), Some(35));
        assert_eq!(store.read_by_space(4111, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(4112, RegisterSpace::Holding), Some(25));
        assert_eq!(store.read_by_space(4113, RegisterSpace::Holding), Some(35));
        assert_eq!(store.read_by_space(4114, RegisterSpace::Holding), Some(25));
        assert_eq!(store.read_by_space(4141, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(4142, RegisterSpace::Holding), Some(45));
    }

    #[test]
    fn ge_holding_all_safe_write_regs_accept_writes() {
        let mut store = RegisterStore::new(default_register_catalogue());
        let addrs: &[u16] = &[
            20, 27, 29, 31, 32, 35, 36, 37, 38, 39, 40, 44, 45, 50, 56, 57, 59, 94, 95, 96, 110,
            111, 112, 116, 163, 318, 319, 320,
        ];
        for &addr in addrs {
            assert!(store.write(addr, 42), "HR {addr} should accept writes");
            assert_eq!(store.read(addr), Some(42), "HR {addr} should read back");
        }
    }

    #[test]
    fn ge_input_battery_power_negative_when_charging() {
        let mut state = make_state();
        state.batteries[0].power_kw = 2.0;
        state.sync_battery_from_vec();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        let raw = store.read_by_space(52, RegisterSpace::Input).unwrap();
        assert!((raw as i16) < 0, "IR 52 should be negative when charging");
    }

    #[test]
    fn ge_input_battery_current_negative_when_charging() {
        let mut state = make_state();
        state.batteries[0].power_kw = 2.0;
        state.sync_battery_from_vec();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        let raw = store.read_by_space(51, RegisterSpace::Input).unwrap();
        assert!((raw as i16) < 0, "IR 51 should be negative when charging");
    }

    #[test]
    fn ge_input_battery_power_positive_when_discharging() {
        let mut state = make_state();
        state.batteries[0].power_kw = -2.0;
        state.sync_battery_from_vec();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        let raw = store.read_by_space(52, RegisterSpace::Input).unwrap();
        assert!(
            (raw as i16) > 0,
            "IR 52 should be positive when discharging"
        );
    }

    #[test]
    fn catalogue_contains_all_safe_write_registers() {
        let cat = default_register_catalogue();
        let addrs: &[u16] = &[
            20, 27, 29, 31, 32, 35, 36, 37, 38, 39, 40, 44, 45, 50, 56, 57, 59, 94, 95, 96, 110,
            111, 112, 116, 163, 318, 319, 320,
        ];
        for &addr in addrs {
            let found = cat
                .iter()
                .any(|d| d.address == addr && d.access == Access::ReadWrite);
            assert!(found, "HR {addr} must be in catalogue as ReadWrite");
        }
    }

    // ===================================================================
    // BMS Register Tests
    // ===================================================================

    #[test]
    fn battery_bms_projects_valid_data() {
        let state = make_state();
        let bms = RegisterStore::new(default_register_catalogue());
        let result = bms.project_battery_bms(&state.batteries[0], 0);
        // Check IR 100 = SOC
        assert_eq!(result[40], state.batteries[0].soc_percent.round() as u16);
        // Check IR 97 = cell count
        assert_eq!(result[37], 16);
        // Check IR 82-83 = voltage (uint32 mV)
        let v = ((result[22] as u32) << 16) | result[23] as u32;
        assert!(
            v > 40000 && v < 60000,
            "Voltage should be 40-60V, got {v} mV"
        );
        // Check IR 96 = cycles
        assert_eq!(result[36], 0);
        // Check serial is non-empty (IR 110-114)
        assert!(
            result[50] > 0 || result[51] > 0,
            "Serial should be non-empty"
        );
    }

    #[test]
    fn battery_bms_projects_status_cap_design2_and_usb() {
        let mut state = make_state();
        state.batteries[0].power_kw = 1.2;
        state.batteries[0].nominal_capacity_kwh = 9.5;
        state.sync_battery_from_vec();
        let bms = RegisterStore::new(default_register_catalogue());
        let result = bms.project_battery_bms(&state.batteries[0], 0);

        assert_eq!(
            result[30] >> 8,
            1,
            "IR 90 status_1 should indicate charging"
        );
        assert_eq!(result[34], 0, "IR 94 warnings default healthy");
        let cap_design = ((result[26] as u32) << 16) | result[27] as u32;
        let cap_design2 = ((result[41] as u32) << 16) | result[42] as u32;
        assert_eq!(
            cap_design2, cap_design,
            "IR 101-102 mirrors design capacity"
        );
        assert_eq!(
            result[55], 1,
            "IR 115 usb_device_inserted defaults to WiFi/raw 1"
        );
    }

    #[test]
    fn battery_bms_projects_multi_module() {
        let mut state = make_state();
        // Add second battery
        let mut b2 = state.batteries[0].clone();
        b2.soc_percent = 30.0;
        b2.voltage_v = 46.0;
        state.batteries.push(b2);
        state.sync_battery_from_vec();

        let bms = RegisterStore::new(default_register_catalogue());
        let bms0 = bms.project_battery_bms(&state.batteries[0], 0);
        let bms1 = bms.project_battery_bms(&state.batteries[1], 1);
        assert_eq!(bms0[40], 75); // module 0 SOC
        assert_eq!(bms1[40], 30); // module 1 SOC
        // Different serials
        assert_ne!(bms0[53], bms1[53], "Serials should differ");
    }

    // ===================================================================
    // Inverter Type / DTC Tests
    // ===================================================================

    #[test]
    fn inverter_dtc_projects_correctly() {
        let now = test_ts();

        // Gen3 Hybrid → 0x2001
        let mut s = PlantState::new(now);
        s.config.inverter_type = "Gen3Hybrid".to_string();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(0, RegisterSpace::Holding), Some(0x2001));

        // All-in-One → 0x8002
        s.config.inverter_type = "AllInOne".to_string();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(0, RegisterSpace::Holding), Some(0x8002));

        // Three Phase → 0x4001
        s.config.inverter_type = "ThreePhase".to_string();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(0, RegisterSpace::Holding), Some(0x4001));
    }

    // ===================================================================
    // Battery Capacity Register (HR 55) Tests
    // ===================================================================

    #[test]
    fn battery_capacity_ah_reflects_total_kwh() {
        let now = test_ts();
        let mut s = PlantState::new(now);
        s.batteries[0].capacity_kwh = 9.5;
        s.config.inverter_type = "Gen3Hybrid".to_string(); // nomV = 51.2
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        let ah = store.read_by_space(55, RegisterSpace::Holding).unwrap_or(0);
        // 9.5 kWh / 51.2V * 1000 ≈ 185 Ah
        assert!(
            ah > 150 && ah < 220,
            "Expected ~185 Ah for 9.5kWh @ 51.2V, got {ah}"
        );
    }

    // ===================================================================
    // Serial Number / Firmware Tests
    // ===================================================================

    #[test]
    fn serial_number_registers_populated() {
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&PlantState::new(test_ts()));
        for addr in 13..=17 {
            let val = store.read_by_space(addr, RegisterSpace::Holding);
            assert!(
                val.is_some() && val.unwrap() > 0,
                "HR {addr} should have serial data"
            );
        }
    }

    #[test]
    fn firmware_version_register_populated() {
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&PlantState::new(test_ts()));
        let fw = store.read_by_space(21, RegisterSpace::Holding).unwrap_or(0);
        assert!(fw > 0, "HR 21 should have firmware version");
    }

    // ===================================================================
    // Three-Phase 11kW (0x4004) Regression Tests
    // ===================================================================
    //
    // GivEnergy three-phase clients (giv_tcp / givenergy-modbus /
    // givenergy-local) read HR 1108/1110 (battery limits), HR 1113-1121
    // (charge/discharge slots 1-2), HR 246-268 / 276-298 (slots 3-10),
    // HR 55 (battery capacity Ah @ 76.8V nominal), HR 1109 (SOC reserve),
    // and the mppt_phase_count byte to detect a 3-phase unit. A real
    // 3-phase 11kW inverter reports DTC 0x4004. These checks pin down
    // every register a 3-phase client depends on so the 8/10/11kW
    // variants don't silently fall through to single-phase defaults.

    fn three_phase_11kw_state() -> PlantState {
        let mut s = PlantState::new(test_ts());
        s.config.inverter_type = "ThreePhase11kW".to_string();
        s.config.max_ac_watts = 11000.0;
        s.inverter.dsp_firmware_version = 11043;
        s.batteries[0].capacity_kwh = 9.5;
        s.batteries[0].soc_percent = 50.0;
        s.sync_battery_from_vec();
        s
    }

    #[test]
    fn threephase_11kw_projects_correct_dtc() {
        let s = three_phase_11kw_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(0, RegisterSpace::Holding), Some(0x4004));
    }

    #[test]
    fn threephase_11kw_reports_three_phases_byte() {
        // HR 3 packs MPPT count (high byte) and phase count (low byte).
        // 3-phase clients detect '3' in the low byte.
        let s = three_phase_11kw_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        let v = store.read_by_space(3, RegisterSpace::Holding).unwrap_or(0);
        assert_eq!(
            v & 0xFF,
            3,
            "low byte should be 3 (three phase), got {v:#x}"
        );
    }

    #[test]
    fn threephase_11kw_battery_capacity_uses_76v_nominal() {
        // HR 55 = (kWh * 1000) / nominal_voltage. At 9.5kWh / 76.8V → ~124 Ah.
        // A buggy projection using the 51.2V single-phase default would
        // return ~186 Ah and trigger alarms in 3-phase clients.
        let s = three_phase_11kw_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        let ah = store.read_by_space(55, RegisterSpace::Holding).unwrap_or(0);
        assert!(
            (120..=130).contains(&ah),
            "Expected ~124 Ah @ 76.8V, got {ah}"
        );
    }

    #[test]
    fn threephase_11kw_battery_limit_mirrors_default_to_100_percent() {
        // HR 1108 (discharge limit) and HR 1110 (charge limit) are the
        // 3-phase mirrors of HR 314/313. Default must be 100% so a
        // freshly-booted client doesn't refuse to charge/discharge.
        let s = three_phase_11kw_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(1108, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(1110, RegisterSpace::Holding), Some(100));
    }

    #[test]
    fn threephase_11kw_schedule_mirrors_slots_1_and_2() {
        // HR 1113/1114 (charge slot 1) and 1115/1116 (charge slot 2)
        // must mirror HR 94/95 and HR 31/32. Same for discharge slots
        // at HR 1118-1121 mirroring HR 56/57 and HR 44/45.
        let mut store = RegisterStore::new(default_register_catalogue());
        let sched = sim_models::Schedule {
            charge_start: 1.5,
            charge_end: 5.0,
            charge_start_2: 10.0,
            charge_end_2: 12.0,
            discharge_start: 16.0,
            discharge_end: 19.0,
            discharge_start_2: 21.0,
            discharge_end_2: 23.0,
            ..Default::default()
        };
        store.project_schedule_for(&sched, "ThreePhase11kW");

        assert_eq!(store.read_by_space(94, RegisterSpace::Holding), Some(130));
        assert_eq!(store.read_by_space(95, RegisterSpace::Holding), Some(500));
        assert_eq!(store.read_by_space(1113, RegisterSpace::Holding), Some(130));
        assert_eq!(store.read_by_space(1114, RegisterSpace::Holding), Some(500));

        assert_eq!(store.read_by_space(31, RegisterSpace::Holding), Some(1000));
        assert_eq!(store.read_by_space(32, RegisterSpace::Holding), Some(1200));
        assert_eq!(
            store.read_by_space(1115, RegisterSpace::Holding),
            Some(1000)
        );
        assert_eq!(
            store.read_by_space(1116, RegisterSpace::Holding),
            Some(1200)
        );

        assert_eq!(store.read_by_space(56, RegisterSpace::Holding), Some(1600));
        assert_eq!(store.read_by_space(57, RegisterSpace::Holding), Some(1900));
        assert_eq!(
            store.read_by_space(1118, RegisterSpace::Holding),
            Some(1600)
        );
        assert_eq!(
            store.read_by_space(1119, RegisterSpace::Holding),
            Some(1900)
        );

        assert_eq!(store.read_by_space(44, RegisterSpace::Holding), Some(2100));
        assert_eq!(store.read_by_space(45, RegisterSpace::Holding), Some(2300));
        assert_eq!(
            store.read_by_space(1120, RegisterSpace::Holding),
            Some(2100)
        );
        assert_eq!(
            store.read_by_space(1121, RegisterSpace::Holding),
            Some(2300)
        );
    }

    #[test]
    fn threephase_11kw_charge_target_soc_mirrored_at_hr_1111() {
        let mut store = RegisterStore::new(default_register_catalogue());
        let sched = sim_models::Schedule {
            charge_target_soc: 87.0,
            ..Default::default()
        };
        store.project_schedule_for(&sched, "ThreePhase11kW");
        assert_eq!(store.read_by_space(116, RegisterSpace::Holding), Some(87));
        assert_eq!(store.read_by_space(1111, RegisterSpace::Holding), Some(87));
    }

    fn read_u32_ir(store: &RegisterStore, hi: u16, lo: u16) -> u32 {
        ((store.read_by_space(hi, RegisterSpace::Input).unwrap_or(0) as u32) << 16)
            | store.read_by_space(lo, RegisterSpace::Input).unwrap_or(0) as u32
    }

    #[test]
    fn threephase_11kw_publishes_live_data_on_tph_input_registers() {
        let mut s = three_phase_11kw_state();
        s.config.pv2_peak_watts = 3000.0;
        s.solar.pv1_w = 2100.0;
        s.solar.pv2_w = 1900.0;
        s.solar.generation_w = 4000.0;
        s.inverter.ac_power_w = 3300.0;
        s.grid.power_w = 1800.0; // internal positive = importing
        s.load.demand_w = 2700.0;
        s.batteries[0].power_kw = 1.5;
        s.batteries[0].soc_percent = 62.0;
        s.sync_battery_from_vec();
        s.inverter.dsp_firmware_version = 612;
        s.inverter.arm_firmware_version = 318;
        s.energy_totals.solar_generation_kwh = 12.0;
        s.energy_totals.grid_import_kwh = 3.0;
        s.energy_totals.grid_export_kwh = 4.0;
        s.energy_totals.battery_charge_kwh = 5.0;
        s.energy_totals.battery_discharge_kwh = 6.0;
        s.energy_totals.load_consumption_kwh = 7.0;

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        // PV block: real 3-phase clients read IR 1001/1002 and 1017-1020.
        assert_eq!(store.read_by_space(1001, RegisterSpace::Input), Some(3500));
        assert_eq!(store.read_by_space(1002, RegisterSpace::Input), Some(3500));
        assert_eq!(read_u32_ir(&store, 1017, 1018), 21000); // 2100W at ×0.1W
        assert_eq!(read_u32_ir(&store, 1019, 1020), 19000);

        // Grid/load block: voltage, per-phase current/load, total load.
        assert_eq!(store.read_by_space(1061, RegisterSpace::Input), Some(2400));
        assert_eq!(store.read_by_space(1062, RegisterSpace::Input), Some(2400));
        assert_eq!(store.read_by_space(1063, RegisterSpace::Input), Some(2400));
        assert_eq!(store.read_by_space(1064, RegisterSpace::Input), Some(46));
        assert_eq!(store.read_by_space(1083, RegisterSpace::Input), Some(9000));
        assert_eq!(read_u32_ir(&store, 1089, 1090), 27000);
        assert_eq!(read_u32_ir(&store, 1079, 1080), 1234); // CT/meter import — hardcoded lifetime
        assert_eq!(read_u32_ir(&store, 1081, 1082), 4321); // CT/meter export — hardcoded lifetime
        assert_eq!(read_u32_ir(&store, 1240, 1241), 4321); // export mirror — hardcoded lifetime
        assert_eq!(read_u32_ir(&store, 1244, 1245), 0); // second meter absent

        // Battery block: temperatures, voltages, SoC, charge/discharge split, current and firmware IDs.
        assert_eq!(store.read_by_space(1128, RegisterSpace::Input), Some(350)); // t_inverter 35.0°C
        assert_eq!(store.read_by_space(1129, RegisterSpace::Input), Some(330)); // t_boost 33.0°C
        assert_eq!(store.read_by_space(1130, RegisterSpace::Input), Some(380)); // t_buck_boost 38.0°C
        assert_eq!(store.read_by_space(1131, RegisterSpace::Input), Some(768)); // v_battery_bms 76.8V
        assert_eq!(store.read_by_space(1132, RegisterSpace::Input), Some(62));
        assert_eq!(read_u32_ir(&store, 1138, 1139), 15000); // charging 1.5kW at ×0.1W
        assert_eq!(read_u32_ir(&store, 1136, 1137), 0);
        assert!((store.read_by_space(1140, RegisterSpace::Input).unwrap() as i16) < 0);
        assert_eq!(store.read_by_space(1133, RegisterSpace::Input), Some(768)); // v_battery_pcs 76.8V
        assert_eq!(store.read_by_space(1134, RegisterSpace::Input), Some(3800)); // v_dc_bus 380.0V
        assert_eq!(store.read_by_space(1135, RegisterSpace::Input), Some(3800)); // v_inv_bus 380.0V
        assert_eq!(store.read_by_space(1325, RegisterSpace::Input), Some(612));
        assert_eq!(store.read_by_space(1326, RegisterSpace::Input), Some(612));
        assert_eq!(store.read_by_space(1327, RegisterSpace::Input), Some(318)); // overridden in test
        assert_eq!(store.read_by_space(19, RegisterSpace::Holding), Some(612));
        assert_eq!(store.read_by_space(21, RegisterSpace::Holding), Some(318)); // override propagates

        // Energy block: 3-phase energy totals live at IR 1366+.
        assert_eq!(read_u32_ir(&store, 1366, 1367), 60); // PV1 half of 12.0kWh, ×0.1kWh
        assert_eq!(read_u32_ir(&store, 1370, 1371), 60); // PV2 half
        assert_eq!(read_u32_ir(&store, 1380, 1381), 30);
        assert_eq!(read_u32_ir(&store, 1384, 1385), 40);
        assert_eq!(read_u32_ir(&store, 1388, 1389), 60);
        assert_eq!(read_u32_ir(&store, 1392, 1393), 50);
        assert_eq!(read_u32_ir(&store, 1396, 1397), 70);
        // Total/lifetime counterparts
        assert_eq!(read_u32_ir(&store, 1360, 1361), 120); // e_inverter_out_today = solar(12.0)
        assert_eq!(read_u32_ir(&store, 1362, 1363), 120); // e_inverter_out_total
        assert_eq!(read_u32_ir(&store, 1368, 1369), 60); // e_pv1_total = half
        assert_eq!(read_u32_ir(&store, 1372, 1373), 60); // e_pv2_total = half
        assert_eq!(read_u32_ir(&store, 1374, 1375), 120); // e_pv_total
        assert_eq!(read_u32_ir(&store, 1382, 1383), 1234); // e_import_total — hardcoded CT lifetime
        assert_eq!(read_u32_ir(&store, 1386, 1387), 4321); // e_export_total — hardcoded CT lifetime
        assert_eq!(read_u32_ir(&store, 1390, 1391), 60); // e_battery_discharge_total
        assert_eq!(read_u32_ir(&store, 1394, 1395), 50); // e_battery_charge_total
        assert_eq!(read_u32_ir(&store, 1398, 1399), 70); // e_load_total
        assert_eq!(read_u32_ir(&store, 1400, 1401), 40); // e_export2_today
        assert_eq!(read_u32_ir(&store, 1402, 1403), 40); // e_export2_total
        assert_eq!(read_u32_ir(&store, 1412, 1413), 120); // e_pv_today (combined)
    }

    #[test]
    fn threephase_11kw_firmware_registers_have_correct_defaults() {
        let s = three_phase_11kw_state();
        // No override — should use type defaults
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        assert_eq!(store.read_by_space(19, RegisterSpace::Holding), Some(11043));
        assert_eq!(store.read_by_space(21, RegisterSpace::Holding), Some(612));
        assert_eq!(store.read_by_space(1325, RegisterSpace::Input), Some(11043));
        assert_eq!(store.read_by_space(1326, RegisterSpace::Input), Some(11043));
        assert_eq!(store.read_by_space(1327, RegisterSpace::Input), Some(612));
    }

    #[test]
    fn threephase_11kw_ct_meter_total_registers_are_hardcoded() {
        // CT meter lifetime totals are hardcoded test values, not derived
        // from instant grid power. The live grid CT reading is at IR 1076
        // (status) via tph_ir_p_inverter_out and the signed grid power at
        // single-phase IR 30.
        let s = three_phase_11kw_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        assert_eq!(read_u32_ir(&store, 1079, 1080), 1234);
        assert_eq!(read_u32_ir(&store, 1081, 1082), 4321);
        assert_eq!(read_u32_ir(&store, 1240, 1241), 4321);
    }
}
