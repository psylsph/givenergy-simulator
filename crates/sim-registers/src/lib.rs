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

use serde::{Deserialize, Serialize};
use chrono::{Datelike, Timelike};

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
                "ge_ir_pv1_voltage" => {
                    Some(if state.solar.pv1_w > 0.0 { 350.0 } else { 0.0 })
                }
                // IR 2: PV2 voltage (×0.1 V)
                // Show voltage whenever PV2 is configured (even not generating)
                "ge_ir_pv2_voltage" => {
                    Some(if state.solar.pv2_w > 0.0 || state.config.pv2_peak_watts > 0.0 { 350.0 } else { 0.0 })
                }
                // IR 5: Grid voltage (×0.1 V)
                "ge_ir_grid_voltage" => Some(240.0),
                // IR 8: PV1 current (×0.1 A) — per-string: pv1_w / 350V
                "ge_ir_pv1_current" => Some(state.solar.pv1_w / 350.0),
                // IR 9: PV2 current (×0.1 A)
                "ge_ir_pv2_current" => Some(state.solar.pv2_w / 350.0),
                // IR 13: Grid frequency (×0.01 Hz)
                "ge_ir_grid_frequency" => Some(50.0),
                // IR 17: PV1 energy today (×0.1 kWh)
                "ge_ir_pv1_energy_today" => Some(state.energy_totals.solar_generation_kwh / 2.0),
                // IR 18: PV1 power (W)
                "ge_ir_pv1_power" => Some(state.solar.pv1_w),
                // IR 19: PV2 energy today (×0.1 kWh)
                "ge_ir_pv2_energy_today" => Some(state.energy_totals.solar_generation_kwh / 2.0),
                // IR 20: PV2 power (W)
                "ge_ir_pv2_power" => Some(state.solar.pv2_w),
                // IR 25: Export energy today (×0.1 kWh)
                "ge_ir_today_export_energy" => Some(state.energy_totals.grid_export_kwh),
                // IR 26: Import energy today (×0.1 kWh)
                "ge_ir_today_import_energy" => Some(state.energy_totals.grid_import_kwh),
                // IR 30: Grid power (signed, +exporting/-importing per GE convention)
                "ge_ir_grid_power" => {
                    // Negate: our internal has positive=import, GE wire has positive=export
                    self.values.insert(key, (-state.grid.power_w) as i16 as u16);
                    continue;
                }
                // IR 35: Consumption today (×0.1 kWh)
                "ge_ir_today_consumption" => Some(state.energy_totals.load_consumption_kwh),
                // IR 36: Battery charge today (×0.1 kWh)
                "ge_ir_today_charge_energy" => Some(state.energy_totals.battery_charge_kwh),
                // IR 37: Battery discharge today (×0.1 kWh)
                "ge_ir_today_discharge_energy" => Some(state.energy_totals.battery_discharge_kwh),
                // IR 41: Inverter temperature (×0.1 °C)
                "ge_ir_inverter_temperature" => Some(state.inverter.temperature_celsius),
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
                // IR 56: Battery temperature (×0.1 °C)
                "ge_ir_battery_temperature" => Some(state.battery_temperature_celsius()),
                // IR 59: Battery SOC (%)
                "ge_ir_battery_soc" => Some(state.aggregate_soc()),

                // ================================================================
                // GivEnergy-native Holding Registers (HR 0-119)
                // ================================================================

                // HR 0: Device type
                "ge_hr_device_type" => {
                    // Encode inverter type as DTC hex code
                    let dtc: u16 = match state.config.inverter_type.as_str() {
                        "Gen1Hybrid" => 0x1001,
                        "Gen3Hybrid" => 0x2001,
                        "Gen3Hybrid8kW" => 0x2101,
                        "Gen3Hybrid10kW" => 0x2102,
                        "ACCoupled" => 0x3001,
                        "ACCoupled2" => 0x3002,
                        "AllInOne6" => 0x8001,
                        "AllInOne" => 0x8002,
                        "AllInOne5" => 0x8003,
                        "AIO8kW" => 0x8102,
                        "AIO10kW" => 0x8103,
                        "ThreePhase" => 0x4001,
                        _ => 0x2001,
                    };
                    self.values.insert(key, dtc);
                    continue;
                }
                // HR 13-17: Serial number (simulated)
                "ge_hr_serial_0" => { self.values.insert(key, (b'S' as u16) << 8 | b'I' as u16); continue; }
                "ge_hr_serial_1" => { self.values.insert(key, (b'M' as u16) << 8 | b'0' as u16); continue; }
                "ge_hr_serial_2" => { self.values.insert(key, (b'0' as u16) << 8 | b'1' as u16); continue; }
                "ge_hr_serial_3" => { self.values.insert(key, (b'2' as u16) << 8 | b'3' as u16); continue; }
                "ge_hr_serial_4" => { self.values.insert(key, (b'4' as u16) << 8 | b'5' as u16); continue; }
                // HR 21: ARM firmware version
                "ge_hr_arm_firmware" => {
                    self.values.insert(key, 300); // FW 3.xx = Gen3
                    continue;
                }
                // HR 29: Battery calibration stage (0=off)
                "ge_hr_battery_calibration_stage" => {
                    self.values.insert(key, 0);
                    continue;
                }
                // HR 20: Enable charge target
                "ge_hr_enable_charge_target" => {
                    self.values.insert(key, 0);
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
                // HR 50: Active power rate (%)
                "ge_hr_active_power_rate" => Some(100.0),
                // HR 55: Battery capacity in Ah (total system)
                // kWh = Ah * nominal_voltage / 1000 → Ah = kWh * 1000 / V
                "ge_hr_battery_capacity_ah" => {
                    let nom_v = match state.config.inverter_type.as_str() {
                        "AllInOne6" | "AllInOne" | "AllInOne5" | "AIO8kW" | "AIO10kW" => 307.0,
                        "ThreePhase" => 76.8,
                        _ => 51.2,
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
                // HR 59: Enable discharge
                "ge_hr_enable_discharge" => {
                    let v = match state.inverter.mode_state.effective {
                        sim_models::InverterMode::ForceDischarge => 1,
                        _ => 0,
                    };
                    self.values.insert(key, v);
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
                // HR 110: Battery SOC reserve (%)
                "ge_hr_battery_soc_reserve" => Some(state.min_aggregate_soc()),
                // HR 111: Battery charge limit (%)
                "ge_hr_battery_charge_limit" => Some(100.0),
                // HR 112: Battery discharge limit (%)
                "ge_hr_battery_discharge_limit" => Some(100.0),
                // HR 116: Charge target SOC (%)
                "ge_hr_charge_target_soc" => Some(100.0),
                // HR 163: Inverter reboot (write-only, always reads 0)
                "ge_hr_inverter_reboot" => {
                    self.values.insert(key, 0);
                    continue;
                }
                // HR 318: Battery pause mode
                "ge_hr_battery_pause_mode" => {
                    self.values.insert(key, 0); // not paused
                    continue;
                }
                // HR 319: Battery pause slot 1 start (HHMM)
                "ge_hr_battery_pause_slot_1_start" => {
                    self.values.insert(key, 60); // disabled sentinel
                    continue;
                }
                // HR 320: Battery pause slot 1 end (HHMM)
                "ge_hr_battery_pause_slot_1_end" => {
                    self.values.insert(key, 60); // disabled sentinel
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
                "inverter_ac_power" => Some(state.inverter.ac_power_w),
                "inverter_export_limit_w" => Some(state.inverter.export_limit_w),
                "inverter_temperature" => Some(state.inverter.temperature_celsius),
                "inverter_firmware_state" => {
                    self.values.insert(key, 1);
                    continue;
                }
                "battery_soc" => Some(state.aggregate_soc()),
                "battery_soc_2" => Some(state.batteries.get(1).map(|b| b.soc_percent).unwrap_or(0.0)),
                "battery_soc_3" => Some(state.batteries.get(2).map(|b| b.soc_percent).unwrap_or(0.0)),
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
                "battery_charge_efficiency" => {
                    Some(state.batteries.first().map(|b| b.charge_efficiency).unwrap_or(0.95))
                }
                "battery_discharge_efficiency" => {
                    Some(state.batteries.first().map(|b| b.discharge_efficiency).unwrap_or(0.95))
                }
                "battery_module_count" => {
                    self.values.insert(key, state.batteries.len() as u16);
                    continue;
                }
                "pv_generation" => Some(state.solar.generation_w),
                "pv_voltage" => Some(if state.solar.generation_w > 0.0 { 350.0 } else { 0.0 }),
                "pv_current" => Some(state.solar.generation_w / 350.0),
                "pv_energy_today" => Some(state.energy_totals.solar_generation_kwh),
                "pv_peak_capacity" => Some(state.config.solar_peak_watts),
                "grid_power" => {
                    self.values.insert(key, state.grid.power_w as i16 as u16);
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
                    self.values.insert(key, state.config.tick_interval_secs as u16);
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
                "schedule_charge_start" | "schedule_charge_end" |
                "schedule_discharge_start" | "schedule_discharge_end" => {
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

                _ => continue,
            };

            if let Some(eng) = engineering {
                let raw = if def.scaling_factor > 0.0 {
                    (eng / def.scaling_factor) as u16
                } else {
                    eng as u16
                };
                self.values.insert(key, raw);
            }
        }
    }

    /// Iterator over all definitions.
    /// Project battery BMS data for a specific battery module (IR 60-119).
    ///
    /// Returns 60 u16 values for Input Registers 60-119.
    /// The reference client reads this on slave 0x32 for battery #1,
    /// and slaves 0x33-0x37 for additional batteries.
    pub fn project_battery_bms(&self, battery: &sim_models::BatteryState, module_index: usize) -> [u16; 60] {
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
        let cap_remaining_ah = ((battery.capacity_kwh * battery.soc_percent / 100.0) * 1000.0 / 51.2 * 100.0).round() as u32;

        // cap_calibrated: IR 84-85
        regs[24] = (cap_calibrated_ah >> 16) as u16;
        regs[25] = (cap_calibrated_ah & 0xFFFF) as u16;
        // cap_design: IR 86-87
        regs[26] = (cap_design_ah >> 16) as u16;
        regs[27] = (cap_design_ah & 0xFFFF) as u16;
        // cap_remaining: IR 88-89
        regs[28] = (cap_remaining_ah >> 16) as u16;
        regs[29] = (cap_remaining_ah & 0xFFFF) as u16;

        // num_cycles: IR 96
        regs[36] = battery.cycle_count.round() as u16;
        // num_cells: IR 97
        regs[37] = cell_count as u16;
        // bms_firmware_version: IR 98
        regs[38] = 100; // v1.00

        // SOC: IR 100
        regs[40] = battery.soc_percent.round() as u16;

        // t_max: IR 103 (0.1 °C)
        regs[43] = temp_deci;
        // t_min: IR 104 (0.1 °C)
        regs[44] = (temp_deci as f64 * 0.98).round() as u16;

        // Serial number: IR 110-114 (5 regs = 10 Latin-1 chars)
        // Generate a simulated serial
        let serial_str = format!("BAT{:04X}{:02X}  ", module_index + 1, (battery.soc_percent as u16) & 0xFF);
        for i in 0..5 {
            let c1 = serial_str.as_bytes().get(i * 2).copied().unwrap_or(b' ');
            let c2 = serial_str.as_bytes().get(i * 2 + 1).copied().unwrap_or(b' ');
            regs[50 + i] = (c1 as u16) << 8 | c2 as u16;
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
    use RegisterCategory as C;
    use RegisterType as T;
    use Access::*;
    use RegisterSpace::*;

    vec![
        // ================================================================
        // GivEnergy-native Input Registers (IR 0-59)
        // Read via fn 0x04 (Read Input Registers), slave 0x32
        // ================================================================
        RegisterDef { address: 0,   name: "ge_ir_status".into(),              category: C::Inverter, typ: T::U16, scaling_factor: 1.0,  access: ReadOnly, space: Input },
        RegisterDef { address: 1,   name: "ge_ir_pv1_voltage".into(),         category: C::PV,       typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 2,   name: "ge_ir_pv2_voltage".into(),         category: C::PV,       typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 5,   name: "ge_ir_grid_voltage".into(),        category: C::Grid,     typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 8,   name: "ge_ir_pv1_current".into(),         category: C::PV,       typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 9,   name: "ge_ir_pv2_current".into(),         category: C::PV,       typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 13,  name: "ge_ir_grid_frequency".into(),      category: C::Grid,     typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 17,  name: "ge_ir_pv1_energy_today".into(),    category: C::PV,       typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 18,  name: "ge_ir_pv1_power".into(),           category: C::PV,       typ: T::U16, scaling_factor: 1.0,  access: ReadOnly, space: Input },
        RegisterDef { address: 19,  name: "ge_ir_pv2_energy_today".into(),    category: C::PV,       typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 20,  name: "ge_ir_pv2_power".into(),           category: C::PV,       typ: T::U16, scaling_factor: 1.0,  access: ReadOnly, space: Input },
        RegisterDef { address: 25,  name: "ge_ir_today_export_energy".into(), category: C::Grid,     typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 26,  name: "ge_ir_today_import_energy".into(), category: C::Grid,     typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 30,  name: "ge_ir_grid_power".into(),          category: C::Grid,     typ: T::S16, scaling_factor: 1.0,  access: ReadOnly, space: Input },
        RegisterDef { address: 35,  name: "ge_ir_today_consumption".into(),   category: C::Grid,     typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 36,  name: "ge_ir_today_charge_energy".into(), category: C::Battery,  typ: T::U16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 37,  name: "ge_ir_today_discharge_energy".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Input },
        RegisterDef { address: 41,  name: "ge_ir_inverter_temperature".into(),category: C::Inverter, typ: T::S16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 50,  name: "ge_ir_battery_voltage".into(),     category: C::Battery,  typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 51,  name: "ge_ir_battery_current".into(),     category: C::Battery,  typ: T::S16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 52,  name: "ge_ir_battery_power".into(),       category: C::Battery,  typ: T::S16, scaling_factor: 1.0,  access: ReadOnly, space: Input },
        RegisterDef { address: 56,  name: "ge_ir_battery_temperature".into(), category: C::Battery,  typ: T::S16, scaling_factor: 0.1,  access: ReadOnly, space: Input },
        RegisterDef { address: 59,  name: "ge_ir_battery_soc".into(),         category: C::Battery,  typ: T::U16, scaling_factor: 1.0,  access: ReadOnly, space: Input },

        // ================================================================
        // GivEnergy-native Holding Registers (HR 0-119)
        // Read via fn 0x03 (Read Holding Registers), slave 0x32
        // ================================================================
        RegisterDef { address: 0,   name: "ge_hr_device_type".into(),         category: C::Inverter,     typ: T::U16, scaling_factor: 1.0, access: ReadOnly,   space: Holding },
        // HR 13-17: Serial number (5 regs, Latin-1, read-only)
        RegisterDef { address: 13,  name: "ge_hr_serial_0".into(),           category: C::Inverter,     typ: T::U16, scaling_factor: 1.0, access: ReadOnly,   space: Holding },
        RegisterDef { address: 14,  name: "ge_hr_serial_1".into(),           category: C::Inverter,     typ: T::U16, scaling_factor: 1.0, access: ReadOnly,   space: Holding },
        RegisterDef { address: 15,  name: "ge_hr_serial_2".into(),           category: C::Inverter,     typ: T::U16, scaling_factor: 1.0, access: ReadOnly,   space: Holding },
        RegisterDef { address: 16,  name: "ge_hr_serial_3".into(),           category: C::Inverter,     typ: T::U16, scaling_factor: 1.0, access: ReadOnly,   space: Holding },
        RegisterDef { address: 17,  name: "ge_hr_serial_4".into(),           category: C::Inverter,     typ: T::U16, scaling_factor: 1.0, access: ReadOnly,   space: Holding },
        RegisterDef { address: 20,  name: "ge_hr_enable_charge_target".into(),category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 21,  name: "ge_hr_arm_firmware".into(),        category: C::Inverter,     typ: T::U16, scaling_factor: 1.0, access: ReadOnly,   space: Holding },
        RegisterDef { address: 27,  name: "ge_hr_battery_power_mode".into(),  category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 29,  name: "ge_hr_battery_calibration_stage".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 31,  name: "ge_hr_charge_slot_2_start".into(), category: C::Schedules,     typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 32,  name: "ge_hr_charge_slot_2_end".into(),   category: C::Schedules,     typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 35,  name: "ge_hr_system_time_year".into(),    category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 36,  name: "ge_hr_system_time_month".into(),   category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 37,  name: "ge_hr_system_time_day".into(),     category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 38,  name: "ge_hr_system_time_hour".into(),    category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 39,  name: "ge_hr_system_time_minute".into(),  category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 40,  name: "ge_hr_system_time_second".into(),  category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 44,  name: "ge_hr_discharge_slot_2_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 45,  name: "ge_hr_discharge_slot_2_end".into(),   category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 50,  name: "ge_hr_active_power_rate".into(),   category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 55,  name: "ge_hr_battery_capacity_ah".into(),  category: C::Battery,      typ: T::U16, scaling_factor: 1.0, access: ReadOnly,   space: Holding },
        RegisterDef { address: 56,  name: "ge_hr_discharge_slot_1_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 57,  name: "ge_hr_discharge_slot_1_end".into(),   category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 59,  name: "ge_hr_enable_discharge".into(),    category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 94,  name: "ge_hr_charge_slot_1_start".into(), category: C::Schedules,     typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 95,  name: "ge_hr_charge_slot_1_end".into(),   category: C::Schedules,     typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 96,  name: "ge_hr_enable_charge".into(),       category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 110, name: "ge_hr_battery_soc_reserve".into(), category: C::Battery,      typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 111, name: "ge_hr_battery_charge_limit".into(),category: C::Battery,      typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 112, name: "ge_hr_battery_discharge_limit".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 116, name: "ge_hr_charge_target_soc".into(),   category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 163, name: "ge_hr_inverter_reboot".into(),     category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 318, name: "ge_hr_battery_pause_mode".into(),   category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite,  space: Holding },
        RegisterDef { address: 319, name: "ge_hr_battery_pause_slot_1_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 320, name: "ge_hr_battery_pause_slot_1_end".into(),   category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },

        // ================================================================
        // Simulator-internal registers (100+, 200+, etc.)
        // All in holding register space.
        // ================================================================

        // ---- Inverter (100–119) ----
        RegisterDef { address: 100, name: "inverter_mode".into(), category: C::Inverter, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 101, name: "inverter_ac_power".into(), category: C::Inverter, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 102, name: "inverter_export_limit_w".into(), category: C::Inverter, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 103, name: "inverter_temperature".into(), category: C::Inverter, typ: T::S16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 104, name: "inverter_firmware_state".into(), category: C::Inverter, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },

        // ---- Battery (200–239) ----
        RegisterDef { address: 200, name: "battery_soc".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 201, name: "battery_soc_2".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 202, name: "battery_soc_3".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 203, name: "battery_power".into(), category: C::Battery, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 204, name: "battery_voltage".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Holding },
        RegisterDef { address: 205, name: "battery_current".into(), category: C::Battery, typ: T::S16, scaling_factor: 0.01, access: ReadOnly, space: Holding },
        RegisterDef { address: 206, name: "battery_temperature".into(), category: C::Battery, typ: T::S16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 207, name: "battery_capacity_kwh".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 208, name: "battery_max_charge_kw".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.1, access: ReadWrite, space: Holding },
        RegisterDef { address: 209, name: "battery_max_discharge_kw".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.1, access: ReadWrite, space: Holding },
        RegisterDef { address: 210, name: "battery_min_soc".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 211, name: "battery_max_soc".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 212, name: "battery_charge_efficiency".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Holding },
        RegisterDef { address: 213, name: "battery_discharge_efficiency".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Holding },
        RegisterDef { address: 214, name: "battery_module_count".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },

        // ---- PV / Solar (300–319) ----
        RegisterDef { address: 300, name: "pv_generation".into(), category: C::PV, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 301, name: "pv_voltage".into(), category: C::PV, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 302, name: "pv_current".into(), category: C::PV, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Holding },
        RegisterDef { address: 303, name: "pv_energy_today".into(), category: C::PV, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 304, name: "pv_peak_capacity".into(), category: C::PV, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },

        // ---- Grid (400–419) ----
        RegisterDef { address: 400, name: "grid_power".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 401, name: "grid_voltage".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 402, name: "grid_frequency".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Holding },
        RegisterDef { address: 403, name: "grid_connected".into(), category: C::Grid, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 404, name: "load_power".into(), category: C::Grid, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },

        // ---- Energy Totals (500–519) ----
        RegisterDef { address: 500, name: "grid_import_kwh".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 501, name: "grid_export_kwh".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 502, name: "battery_charge_kwh".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 503, name: "battery_discharge_kwh".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 504, name: "solar_generation_kwh".into(), category: C::PV, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 505, name: "load_consumption_kwh".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },

        // ---- Configuration (600–619) ----
        RegisterDef { address: 600, name: "config_battery_count".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 601, name: "config_tick_interval".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 602, name: "config_weather".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },

        // ---- Schedules (700–719) ----
        RegisterDef { address: 700, name: "schedule_charge_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 701, name: "schedule_charge_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 702, name: "schedule_discharge_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 703, name: "schedule_discharge_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 704, name: "schedule_charge_target_soc".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 705, name: "schedule_discharge_target_soc".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
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
        state.batteries[0].soc_percent = 75.0;
        state.sync_battery_from_vec();

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        assert_eq!(store.read(300), Some(3500)); // pv_generation
        assert_eq!(store.read(200), Some(75));   // battery_soc (aggregate)
    }

    #[test]
    fn write_respects_access_control() {
        let mut store = RegisterStore::new(default_register_catalogue());
        assert!(store.write(100, 2));  // inverter_mode = ReadWrite
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
        state.inverter.mode_state.set_user(sim_models::InverterMode::ForceCharge);

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
        state.inverter.mode_state.set_user(sim_models::InverterMode::Eco);

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
            assert_eq!(store.read_by_space(addr, RegisterSpace::Holding), Some(60),
                "HR {addr} should be 60 (disabled)");
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
    fn ge_holding_all_safe_write_regs_accept_writes() {
        let mut store = RegisterStore::new(default_register_catalogue());
        let addrs: &[u16] = &[20, 27, 29, 31, 32, 35, 36, 37, 38, 39, 40,
            44, 45, 50, 56, 57, 59, 94, 95, 96, 110, 111, 112, 116, 163, 318, 319, 320];
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
        assert!((raw as i16) > 0, "IR 52 should be positive when discharging");
    }

    #[test]
    fn catalogue_contains_all_safe_write_registers() {
        let cat = default_register_catalogue();
        let addrs: &[u16] = &[20, 27, 29, 31, 32, 35, 36, 37, 38, 39, 40,
            44, 45, 50, 56, 57, 59, 94, 95, 96, 110, 111, 112, 116, 163, 318, 319, 320];
        for &addr in addrs {
            let found = cat.iter().any(|d| d.address == addr && d.access == Access::ReadWrite);
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
        assert!(v > 40000 && v < 60000, "Voltage should be 40-60V, got {v} mV");
        // Check IR 96 = cycles
        assert_eq!(result[36], 0);
        // Check serial is non-empty (IR 110-114)
        assert!(result[50] > 0 || result[51] > 0, "Serial should be non-empty");
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
        assert!(ah > 150 && ah < 220, "Expected ~185 Ah for 9.5kWh @ 51.2V, got {ah}");
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
            assert!(val.is_some() && val.unwrap() > 0, "HR {addr} should have serial data");
        }
    }

    #[test]
    fn firmware_version_register_populated() {
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&PlantState::new(test_ts()));
        let fw = store.read_by_space(21, RegisterSpace::Holding).unwrap_or(0);
        assert!(fw > 0, "HR 21 should have firmware version");
    }
}
