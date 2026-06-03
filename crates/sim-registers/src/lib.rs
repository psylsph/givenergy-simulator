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
        let u32_words = |engineering: f64, scaling: f64| -> (u16, u16) {
            let raw = if scaling > 0.0 {
                (engineering / scaling).max(0.0).round() as u32
            } else {
                engineering.max(0.0).round() as u32
            };
            ((raw >> 16) as u16, (raw & 0xFFFF) as u16)
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
                // IR 11-12: PV total lifetime (uint32, ×0.1 kWh)
                "ge_ir_pv_total_high" | "ge_ir_pv_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 13: Grid frequency (×0.01 Hz)
                "ge_ir_grid_frequency" => Some(50.0),
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
                // IR 25: Export energy today (×0.1 kWh)
                "ge_ir_today_export_energy" => Some(state.energy_totals.grid_export_kwh),
                // IR 26: Import energy today (×0.1 kWh)
                "ge_ir_today_import_energy" => Some(state.energy_totals.grid_import_kwh),
                // IR 30: Grid power (signed, +exporting/-importing per GE convention)
                "ge_ir_grid_power" => {
                    // Negate: our internal has positive=import, GE wire has positive=export
                    // Clamp to i16 range to avoid panic on saturating cast
                    let negated = -state.grid.power_w;
                    let clamped = negated.clamp(-32768.0, 32767.0);
                    self.values.insert(key, clamped as i16 as u16);
                    continue;
                }
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
                // IR 41: Inverter temperature (×0.1 °C)
                "ge_ir_inverter_temperature" => Some(state.inverter.temperature_celsius),
                // IR 44: PV generation today (×0.1 kWh)
                "ge_ir_pv_generation_today" => Some(state.energy_totals.solar_generation_kwh),
                // IR 45-46: PV generation total (uint32, ×0.1 kWh)
                "ge_ir_pv_generation_total_high" | "ge_ir_pv_generation_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_generation_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
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
                "meter_i_phase_2" => { self.values.insert(key, 0); continue; }
                "meter_i_phase_3" => { self.values.insert(key, 0); continue; }
                "meter_i_ln" => { self.values.insert(key, 0); continue; }
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
                "meter_p_active_phase_2" => { self.values.insert(key, 0); continue; }
                "meter_p_active_phase_3" => { self.values.insert(key, 0); continue; }
                "meter_p_active_total" => {
                    let clamped = state.grid.power_w.clamp(-32768.0, 32767.0);
                    self.values.insert(key, clamped as i16 as u16);
                    continue;
                }
                "meter_p_reactive_phase_1" | "meter_p_reactive_phase_2" | "meter_p_reactive_phase_3" | "meter_p_reactive_total" => {
                    self.values.insert(key, 0); continue;
                }
                "meter_p_apparent_phase_1" => {
                    self.values.insert(key, state.grid.power_w.abs() as u16);
                    continue;
                }
                "meter_p_apparent_phase_2" | "meter_p_apparent_phase_3" => {
                    self.values.insert(key, 0); continue;
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
                "meter_pf_phase_2" | "meter_pf_phase_3" => { self.values.insert(key, 0); continue; }
                "meter_pf_total" => {
                    if state.grid.power_w.abs() > 1.0 {
                        self.values.insert(key, 1000i16 as u16);
                    } else {
                        self.values.insert(key, 0);
                    }
                    continue;
                }
                "meter_frequency" => Some(50.0),
                "meter_e_import_active" => Some(state.energy_totals.grid_import_kwh),
                "meter_e_import_reactive" => Some(0.0),
                "meter_e_export_active" => Some(state.energy_totals.grid_export_kwh),
                "meter_e_export_reactive" => Some(0.0),
                "meter_reserved" => Some(0.0),

                // ================================================================
                // GivEnergy-native Holding Registers (HR 0-119)
                // ================================================================

                // HR 0: Device type
                "ge_hr_device_type" => {
                    // Encode inverter type as DTC hex code
                    let dtc: u16 = match state.config.inverter_type.as_str() {
                        "Gen1Hybrid" => 0x2001,
                        "Gen3Hybrid" => 0x2001,
                        "Gen3Hybrid8kW" => 0x2101,
                        "Gen3Hybrid10kW" => 0x2102,
                        "Gen3Plus6kW" => 0x2201,
                        "Gen3Plus4600" => 0x2202,
                        "Gen3Plus3600" => 0x2203,
                        "Gen3Plus6kW2" => 0x2204,
                        "ACCoupled" => 0x3001,
                        "ACCoupled2" => 0x3002,
                        "ThreePhase8kW" => 0x4002,
                        "ThreePhase10kW" => 0x4003,
                        "AllInOne6" => 0x8001,
                        "AllInOne" => 0x8002,
                        "AllInOne5" => 0x8003,
                        "AIO8kW" => 0x8102,
                        "AIO10kW" => 0x8103,
                        "ThreePhase" => 0x4001,
                        "AIOHybrid6kW" => 0x8201,
                        "AIOHybrid8kW" => 0x8202,
                        "AIOHybrid10kW" => 0x8203,
                        _ => 0x2001,
                    };
                    self.values.insert(key, dtc);
                    continue;
                }
                // HR 13-17: Serial number (simulated)
                "ge_hr_serial_0" => {
                    self.values.insert(key, (b'S' as u16) << 8 | b'I' as u16);
                    continue;
                }
                "ge_hr_serial_1" => {
                    self.values.insert(key, (b'M' as u16) << 8 | b'0' as u16);
                    continue;
                }
                "ge_hr_serial_2" => {
                    self.values.insert(key, (b'0' as u16) << 8 | b'1' as u16);
                    continue;
                }
                "ge_hr_serial_3" => {
                    self.values.insert(key, (b'2' as u16) << 8 | b'3' as u16);
                    continue;
                }
                "ge_hr_serial_4" => {
                    self.values.insert(key, (b'4' as u16) << 8 | b'5' as u16);
                    continue;
                }
                // HR 21: ARM firmware version
                "ge_hr_arm_firmware" => {
                    let fw = match state.config.inverter_type.as_str() {
                        "Gen1Hybrid" => 100,
                        "Gen3Hybrid" => 300,
                        "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 400,
                        _ => 300,
                    };
                    self.values.insert(key, fw);
                    continue;
                }
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
                "ge_hr_active_power_rate" => Some(state.active_power_rate_percent),
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
                "ge_hr_battery_charge_limit" => Some(state.battery_charge_limit_percent),
                // HR 112: Battery discharge limit (%)
                "ge_hr_battery_discharge_limit" => Some(state.battery_discharge_limit_percent),
                // HR 114: Battery discharge min power reserve (%)
                "ge_hr_battery_discharge_min_power_reserve" => {
                    Some(state.battery_discharge_min_power_reserve)
                }
                // HR 116: Charge target SOC (%)
                "ge_hr_charge_target_soc" => Some(100.0),
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
        let hrs_to_hhmm = |h: f64| -> u16 {
            if h <= 0.0 {
                return 60; /* disabled */
            }
            let hours = h.floor() as u16;
            let mins = ((h - hours as f64) * 60.0).round() as u16;
            if mins > 59 {
                return 60; /* invalid = disabled */
            }
            hours * 100 + mins
        };

        // Charge slot 1 (HR 94-95)
        let cs1_start = hrs_to_hhmm(schedule.charge_start);
        let cs1_end = hrs_to_hhmm(schedule.charge_end);
        self.write(94, cs1_start); self.write(95, cs1_end);
        let cs2_start = hrs_to_hhmm(schedule.charge_start_2);
        let cs2_end = hrs_to_hhmm(schedule.charge_end_2);
        self.write(31, cs2_start); self.write(32, cs2_end);
        self.write(243, cs2_start); self.write(244, cs2_end);
        let cs3_s = hrs_to_hhmm(schedule.charge_start_3); let cs3_e = hrs_to_hhmm(schedule.charge_end_3);
        self.write(246, cs3_s); self.write(247, cs3_e);
        let cs4_s = hrs_to_hhmm(schedule.charge_start_4); let cs4_e = hrs_to_hhmm(schedule.charge_end_4);
        self.write(249, cs4_s); self.write(250, cs4_e);
        let cs5_s = hrs_to_hhmm(schedule.charge_start_5); let cs5_e = hrs_to_hhmm(schedule.charge_end_5);
        self.write(252, cs5_s); self.write(253, cs5_e);
        let cs6_s = hrs_to_hhmm(schedule.charge_start_6); let cs6_e = hrs_to_hhmm(schedule.charge_end_6);
        self.write(255, cs6_s); self.write(256, cs6_e);
        let cs7_s = hrs_to_hhmm(schedule.charge_start_7); let cs7_e = hrs_to_hhmm(schedule.charge_end_7);
        self.write(258, cs7_s); self.write(259, cs7_e);
        let cs8_s = hrs_to_hhmm(schedule.charge_start_8); let cs8_e = hrs_to_hhmm(schedule.charge_end_8);
        self.write(261, cs8_s); self.write(262, cs8_e);
        let cs9_s = hrs_to_hhmm(schedule.charge_start_9); let cs9_e = hrs_to_hhmm(schedule.charge_end_9);
        self.write(264, cs9_s); self.write(265, cs9_e);
        let cs10_s = hrs_to_hhmm(schedule.charge_start_10); let cs10_e = hrs_to_hhmm(schedule.charge_end_10);
        self.write(267, cs10_s); self.write(268, cs10_e);

        let ds1_start = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start) };
        let ds1_end = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end) };
        self.write(56, ds1_start); self.write(57, ds1_end);
        let ds2_start = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_2) };
        let ds2_end = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_2) };
        self.write(44, ds2_start); self.write(45, ds2_end);
        let ds3_s = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_3) }; let ds3_e = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_3) };
        self.write(276, ds3_s); self.write(277, ds3_e);
        let ds4_s = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_4) }; let ds4_e = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_4) };
        self.write(279, ds4_s); self.write(280, ds4_e);
        let ds5_s = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_5) }; let ds5_e = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_5) };
        self.write(282, ds5_s); self.write(283, ds5_e);
        let ds6_s = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_6) }; let ds6_e = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_6) };
        self.write(285, ds6_s); self.write(286, ds6_e);
        let ds7_s = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_7) }; let ds7_e = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_7) };
        self.write(288, ds7_s); self.write(289, ds7_e);
        let ds8_s = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_8) }; let ds8_e = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_8) };
        self.write(291, ds8_s); self.write(292, ds8_e);
        let ds9_s = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_9) }; let ds9_e = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_9) };
        self.write(294, ds9_s); self.write(295, ds9_e);
        let ds10_s = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_start_10) }; let ds10_e = if is_ac_coupled { 60 } else { hrs_to_hhmm(schedule.discharge_end_10) };
        self.write(297, ds10_s); self.write(298, ds10_e);

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

        let discharge_enabled = !is_ac_coupled && (schedule.enable_discharge
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
        self.write(1122, if discharge_enabled { 1 } else { 0 });
        self.write(1123, if charge_enabled { 1 } else { 0 });
        self.write(272, if is_ac_coupled { 0 } else { schedule.discharge_target_soc as u16 });
        self.write(275, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_2 as u16 });
        self.write(278, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_3 as u16 });
        self.write(281, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_4 as u16 });
        self.write(284, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_5 as u16 });
        self.write(287, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_6 as u16 });
        self.write(290, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_7 as u16 });
        self.write(293, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_8 as u16 });
        self.write(296, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_9 as u16 });
        self.write(299, if is_ac_coupled { 0 } else { schedule.discharge_target_soc_10 as u16 });

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
        let es1_s = hrs_to_hhmm(schedule.export_start_1); let es1_e = hrs_to_hhmm(schedule.export_end_1);
        self.write(2062, es1_s); self.write(2063, es1_e); self.write(2064, schedule.export_target_soc_1 as u16);
        let es2_s = hrs_to_hhmm(schedule.export_start_2); let es2_e = hrs_to_hhmm(schedule.export_end_2);
        self.write(2065, es2_s); self.write(2066, es2_e); self.write(2067, schedule.export_target_soc_2 as u16);
        let es3_s = hrs_to_hhmm(schedule.export_start_3); let es3_e = hrs_to_hhmm(schedule.export_end_3);
        self.write(2068, es3_s); self.write(2069, es3_e); self.write(2070, schedule.export_target_soc_3 as u16);
        self.write(2071, schedule.export_power_limit_w as u16);

        // Internal schedule registers (HR 700-729)
        self.write(700, cs1_start); self.write(701, cs1_end);
        self.write(702, ds1_start); self.write(703, ds1_end);
        self.write(704, schedule.charge_target_soc as u16);
        self.write(705, schedule.discharge_target_soc as u16);
        self.write(706, cs2_start); self.write(707, cs2_end);
        self.write(708, ds2_start); self.write(709, ds2_end);
        self.write(710, schedule.charge_target_soc_2 as u16);
        self.write(711, schedule.discharge_target_soc_2 as u16);
        self.write(712, cs3_s); self.write(713, cs3_e);
        self.write(714, ds3_s); self.write(715, ds3_e);
        self.write(716, schedule.charge_target_soc_3 as u16);
        self.write(717, schedule.discharge_target_soc_3 as u16);
        self.write(718, cs4_s); self.write(719, cs4_e);
        self.write(720, ds4_s); self.write(721, ds4_e);
        self.write(722, schedule.charge_target_soc_4 as u16);
        self.write(723, schedule.discharge_target_soc_4 as u16);
        self.write(724, cs5_s); self.write(725, cs5_e);
        self.write(726, ds5_s); self.write(727, ds5_e);
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
        RegisterDef { address: 60, name: "meter_v_phase_1".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Input },
        RegisterDef { address: 61, name: "meter_v_phase_2".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Input },
        RegisterDef { address: 62, name: "meter_v_phase_3".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Input },
        RegisterDef { address: 63, name: "meter_i_phase_1".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 64, name: "meter_i_phase_2".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 65, name: "meter_i_phase_3".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 66, name: "meter_i_ln".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 67, name: "meter_i_total".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 68, name: "meter_p_active_phase_1".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 69, name: "meter_p_active_phase_2".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 70, name: "meter_p_active_phase_3".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 71, name: "meter_p_active_total".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 72, name: "meter_p_reactive_phase_1".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 73, name: "meter_p_reactive_phase_2".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 74, name: "meter_p_reactive_phase_3".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 75, name: "meter_p_reactive_total".into(), category: C::Grid, typ: T::S16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 76, name: "meter_p_apparent_phase_1".into(), category: C::Grid, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 77, name: "meter_p_apparent_phase_2".into(), category: C::Grid, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 78, name: "meter_p_apparent_phase_3".into(), category: C::Grid, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 79, name: "meter_p_apparent_total".into(), category: C::Grid, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Input },
        RegisterDef { address: 80, name: "meter_pf_phase_1".into(), category: C::Grid, typ: T::S16, scaling_factor: 0.001, access: ReadOnly, space: Input },
        RegisterDef { address: 81, name: "meter_pf_phase_2".into(), category: C::Grid, typ: T::S16, scaling_factor: 0.001, access: ReadOnly, space: Input },
        RegisterDef { address: 82, name: "meter_pf_phase_3".into(), category: C::Grid, typ: T::S16, scaling_factor: 0.001, access: ReadOnly, space: Input },
        RegisterDef { address: 83, name: "meter_pf_total".into(), category: C::Grid, typ: T::S16, scaling_factor: 0.001, access: ReadOnly, space: Input },
        RegisterDef { address: 84, name: "meter_frequency".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.01, access: ReadOnly, space: Input },
        RegisterDef { address: 85, name: "meter_e_import_active".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Input },
        RegisterDef { address: 86, name: "meter_e_import_reactive".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Input },
        RegisterDef { address: 87, name: "meter_e_export_active".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Input },
        RegisterDef { address: 88, name: "meter_e_export_reactive".into(), category: C::Grid, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Input },
        RegisterDef { address: 89, name: "meter_reserved".into(), category: C::Grid, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Input },

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
            address: 27,
            name: "ge_hr_battery_power_mode".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
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
            address: 50,
            name: "ge_hr_active_power_rate".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
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
            address: 59,
            name: "ge_hr_enable_discharge".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
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
            address: 114,
            name: "ge_hr_battery_discharge_min_power_reserve".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
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
        RegisterDef { address: 246, name: "ge_hr_charge_slot_3_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 247, name: "ge_hr_charge_slot_3_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 248, name: "ge_hr_charge_target_soc_3".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 249, name: "ge_hr_charge_slot_4_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 250, name: "ge_hr_charge_slot_4_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 251, name: "ge_hr_charge_target_soc_4".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 252, name: "ge_hr_charge_slot_5_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 253, name: "ge_hr_charge_slot_5_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 254, name: "ge_hr_charge_target_soc_5".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 255, name: "ge_hr_charge_slot_6_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 256, name: "ge_hr_charge_slot_6_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 257, name: "ge_hr_charge_target_soc_6".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 258, name: "ge_hr_charge_slot_7_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 259, name: "ge_hr_charge_slot_7_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 260, name: "ge_hr_charge_target_soc_7".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 261, name: "ge_hr_charge_slot_8_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 262, name: "ge_hr_charge_slot_8_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 263, name: "ge_hr_charge_target_soc_8".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 264, name: "ge_hr_charge_slot_9_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 265, name: "ge_hr_charge_slot_9_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 266, name: "ge_hr_charge_target_soc_9".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 267, name: "ge_hr_charge_slot_10_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 268, name: "ge_hr_charge_slot_10_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 269, name: "ge_hr_charge_target_soc_10".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 276, name: "ge_hr_discharge_slot_3_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 277, name: "ge_hr_discharge_slot_3_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 278, name: "ge_hr_discharge_target_soc_3".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 279, name: "ge_hr_discharge_slot_4_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 280, name: "ge_hr_discharge_slot_4_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 281, name: "ge_hr_discharge_target_soc_4".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 282, name: "ge_hr_discharge_slot_5_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 283, name: "ge_hr_discharge_slot_5_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 284, name: "ge_hr_discharge_target_soc_5".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 285, name: "ge_hr_discharge_slot_6_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 286, name: "ge_hr_discharge_slot_6_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 287, name: "ge_hr_discharge_target_soc_6".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 288, name: "ge_hr_discharge_slot_7_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 289, name: "ge_hr_discharge_slot_7_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 290, name: "ge_hr_discharge_target_soc_7".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 291, name: "ge_hr_discharge_slot_8_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 292, name: "ge_hr_discharge_slot_8_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 293, name: "ge_hr_discharge_target_soc_8".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 294, name: "ge_hr_discharge_slot_9_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 295, name: "ge_hr_discharge_slot_9_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 296, name: "ge_hr_discharge_target_soc_9".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 297, name: "ge_hr_discharge_slot_10_start".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 298, name: "ge_hr_discharge_slot_10_end".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 299, name: "ge_hr_discharge_target_soc_10".into(), category: C::Schedules, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },

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
        RegisterDef {
            address: 2040,
            name: "ems_export_limit".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2044,
            name: "ems_register_2044".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2045,
            name: "ems_register_2045".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2046,
            name: "ems_register_2046".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2047,
            name: "ems_register_2047".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2048,
            name: "ems_register_2048".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2049,
            name: "ems_register_2049".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2050,
            name: "ems_register_2050".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2051,
            name: "ems_register_2051".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2052,
            name: "ems_register_2052".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2053,
            name: "ems_register_2053".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2054,
            name: "ems_register_2054".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2055,
            name: "ems_register_2055".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2056,
            name: "ems_register_2056".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2057,
            name: "ems_register_2057".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2058,
            name: "ems_register_2058".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2059,
            name: "ems_register_2059".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2060,
            name: "ems_register_2060".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2061,
            name: "ems_register_2061".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2062,
            name: "ems_register_2062".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2063,
            name: "ems_register_2063".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2064,
            name: "ems_register_2064".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2065,
            name: "ems_register_2065".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2066,
            name: "ems_register_2066".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2067,
            name: "ems_register_2067".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2068,
            name: "ems_register_2068".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2069,
            name: "ems_register_2069".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2070,
            name: "ems_register_2070".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2071,
            name: "ems_register_2071".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2072,
            name: "ems_register_2072".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        RegisterDef {
            address: 2073,
            name: "ems_register_2073".into(),
            category: C::Configuration,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadWrite,
            space: Holding,
        },
        // ================================================================
        // Smart Load slots (HR 554-573) — 10 start/end pairs
        // ================================================================
        RegisterDef { address: 554, name: "ge_hr_smart_load_slot_1_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 555, name: "ge_hr_smart_load_slot_1_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 556, name: "ge_hr_smart_load_slot_2_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 557, name: "ge_hr_smart_load_slot_2_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 558, name: "ge_hr_smart_load_slot_3_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 559, name: "ge_hr_smart_load_slot_3_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 560, name: "ge_hr_smart_load_slot_4_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 561, name: "ge_hr_smart_load_slot_4_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 562, name: "ge_hr_smart_load_slot_5_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 563, name: "ge_hr_smart_load_slot_5_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 564, name: "ge_hr_smart_load_slot_6_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 565, name: "ge_hr_smart_load_slot_6_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 566, name: "ge_hr_smart_load_slot_7_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 567, name: "ge_hr_smart_load_slot_7_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 568, name: "ge_hr_smart_load_slot_8_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 569, name: "ge_hr_smart_load_slot_8_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 570, name: "ge_hr_smart_load_slot_9_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 571, name: "ge_hr_smart_load_slot_9_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 572, name: "ge_hr_smart_load_slot_10_start".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 573, name: "ge_hr_smart_load_slot_10_end".into(), category: C::Configuration, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },

        // ================================================================
        // High registers (HR 4107-4114) — PV power setting, battery energy alt sources
        // ================================================================
        RegisterDef { address: 4107, name: "ge_hr_pv_power_setting_high".into(), category: C::PV, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 4108, name: "ge_hr_pv_power_setting_low".into(), category: C::PV, typ: T::U16, scaling_factor: 1.0, access: ReadWrite, space: Holding },
        RegisterDef { address: 4109, name: "ge_hr_battery_discharge_total_alt2_high".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 4110, name: "ge_hr_battery_discharge_total_alt2_low".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 4111, name: "ge_hr_battery_charge_total_alt2_high".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 4112, name: "ge_hr_battery_charge_total_alt2_low".into(), category: C::Battery, typ: T::U16, scaling_factor: 1.0, access: ReadOnly, space: Holding },
        RegisterDef { address: 4113, name: "ge_hr_battery_discharge_today_alt3".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },
        RegisterDef { address: 4114, name: "ge_hr_battery_charge_today_alt3".into(), category: C::Battery, typ: T::U16, scaling_factor: 0.1, access: ReadOnly, space: Holding },

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
    fn gen1_hybrid_uses_real_hybrid_dtc_with_gen1_firmware_century() {
        let now = test_ts();
        let mut s = PlantState::new(now);
        s.config.inverter_type = "Gen1Hybrid".to_string();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(store.read_by_space(0, RegisterSpace::Holding), Some(0x2001));
        assert_eq!(store.read_by_space(21, RegisterSpace::Holding), Some(100));
    }

    #[test]
    fn extended_givtcp_schedule_registers_are_catalogued_and_projected() {
        let mut store = RegisterStore::new(default_register_catalogue());
        let mut sched = sim_models::Schedule::default();
        sched.charge_start = 1.5;
        sched.charge_end = 2.5;
        sched.charge_start_2 = 3.0;
        sched.charge_end_2 = 4.0;
        sched.charge_target_soc = 80.0;
        sched.charge_target_soc_2 = 90.0;
        sched.discharge_start = 18.0;
        sched.discharge_end = 19.0;
        sched.discharge_start_2 = 20.0;
        sched.discharge_end_2 = 21.0;
        sched.discharge_target_soc = 20.0;
        sched.discharge_target_soc_2 = 30.0;
        sched.enable_charge = true;
        sched.enable_discharge = true;

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
        assert_eq!(store.read_by_space(1122, RegisterSpace::Holding), Some(1));
        assert_eq!(store.read_by_space(1123, RegisterSpace::Holding), Some(1));
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
}
