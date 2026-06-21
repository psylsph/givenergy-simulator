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
    /// GivEnergy Gateway aggregation bank (IR 1600-1859).
    Gateway,
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

/// Deterministic per-cell voltage multiplier in [0.99, 1.01] (≤1% noise).
///
/// Real packs have slight cell-to-cell capacity/impedance variation. The factor
/// is derived from a splitmix64-style hash of (module, cell) so each cell of each
/// module gets a unique, stable offset — values don't jitter between poll cycles.
fn cell_voltage_factor(module_index: usize, cell_index: usize) -> f64 {
    let mut h = (module_index as u64).wrapping_mul(0x9E3779B97F4A7C15)
        ^ (cell_index as u64).wrapping_mul(0x517CC1B727220A95);
    h ^= h >> 33;
    h = h.wrapping_mul(0xFF51AFD7ED558CCD);
    h ^= h >> 33;
    let frac = (h >> 11) as f64 / (1u64 << 53) as f64; // [0, 1)
    1.0 + (frac * 2.0 - 1.0) * 0.01 // [0.99, 1.01]
}

/// Returns true for inverter types that use Gen3 extended register mapping for
/// charge slot 2 (HR 243-244 instead of classic HR 31-32).
///
/// Gen3/AIO/HV-Gen3/Gen4 models: charge slot 2 lives at HR 243-244.
/// Gen1 hybrids: charge slot 2 lives at classic HR 31-32.
/// Three-phase models: slots 1-2 mirror to HR 1113-1116 (and HR 243-244 in the
/// extended block), but never use HR 31-32 for scheduling.
/// AC-coupled models: only 1 basic charge/discharge slot (HR 94-95 / HR 56-57), no slot 2 at all.
fn uses_gen3_extended_slots(inverter_type: &str) -> bool {
    match inverter_type {
        // Gen1 hybrids: 2-slot max, classic HR 31-32
        "Gen1Hybrid" => false,
        // Gen2 hybrids: 1 charge/discharge slot only (HR 94-95 / HR 56-57)
        "Gen2Hybrid" => false,
        // AC-coupled: only 1 basic charge/discharge slot
        "ACCoupled" | "ACCoupled2" => false,
        // Everything else is Gen3-era or later → HR 243-244
        _ => true,
    }
}

/// Returns true for inverter types that support a second charge/discharge slot.
/// Gen2 hybrids and AC-coupled models only have 1 programmable charge/discharge slot.
fn has_slot_2(inverter_type: &str) -> bool {
    !matches!(inverter_type, "Gen2Hybrid" | "ACCoupled" | "ACCoupled2")
}

/// Returns true for three-phase inverter types that report line-to-line
/// grid voltage (~415V) at TPH registers IR 1061-1063 instead of
/// phase-to-neutral voltage (240V) used by single-phase inverters.
fn is_three_phase_inverter(inverter_type: &str) -> bool {
    inverter_type.starts_with("ThreePhase") || inverter_type == "ACThreePhase"
}

/// Returns true for GivEnergy Gateway device types.
///
/// A Gateway is an AC aggregation/transfer hub (not an inverter) that sits in
/// front of one or more All-in-One (AIO) units and exposes a gateway-specific
/// Input Register bank at IR 1600-1859. When true, `project_gateway_bank`
/// additionally serves the aggregation bank and a `GW`-prefixed serial.
///
/// Detection on the wire keys off the `GW` serial-number prefix; the DTC
/// family is `0x7xxx` (`Gateway12kW` = `0x7001`).
fn is_gateway_inverter(inverter_type: &str) -> bool {
    inverter_type.starts_with("Gateway")
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

    /// Check whether any RegisterDef exists in the given address range for
    /// the given register space. Used to distinguish "register exists but is
    /// zero" from "register does not exist at all" — the Modbus server can
    /// return an error for entirely unmapped ranges instead of silent zeros.
    pub fn has_any_def_in_range(&self, start: u16, count: u16, space: RegisterSpace) -> bool {
        self.defs
            .iter()
            .any(|d| d.space == space && (start..start + count).contains(&d.address))
    }

    /// Update all register values from plant state.
    pub fn project_from_state(&mut self, state: &sim_models::PlantState) {
        // Project energy registers directly from the live plant totals. Energy
        // totals are a true power-integral (accumulated by EnergyTracker and
        // reset at midnight), so a fresh / early-morning plant legitimately
        // reads zero — do NOT inject a synthetic fixture here, otherwise daily
        // energy registers (e.g. PV energy today) jump to a non-zero value the
        // instant a client polls instead of climbing smoothly with power.
        let is_three_phase = is_three_phase_inverter(&state.config.inverter_type);
        let grid_v_ll = (240.0_f64 * 3.0_f64.sqrt()).round(); // 416V line-to-line

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
        // Single-phase inverter fault bitmask for HR(223)-HR(224)
        // (inverter_errors / inverter_fault_messages) — the authoritative
        // named-fault register — and its raw hex mirror on IR(39)-IR(40).
        // Bit positions match the GivEnergy `_inverter_fault_code` decoder used
        // by BOTH givenergy-modbus (model/inverter.py) and giv_tcp
        // (baseinverter.py): the uint32 is decoded MSB-first, so list-index `i`
        // corresponds to bit `31-i`.
        //   grid_loss         → bit 7  ("No Utility",         idx 24)
        //   inverter_trip     → bit 23 ("Consistent Fault",   idx 8)
        //   battery_over_temp → bit 0  ("Inverter NTC Fault", idx 31)
        //     — the only thermal bit on the single-phase inverter fault word; real
        //       battery thermal status lives in the BMS, so this is the closest
        //       client-decodable thermal signal (also reflected on IR 0 status and
        //       IR 57 charger_warning_code as auxiliary signals).
        //   comm_timeout      → bit 24 ("ARM Comms Fault",    idx 7)
        //   sensor_drift      → bit 30 (reserved idx 1 — non-zero word, no name)
        let single_phase_fault_word = || -> u32 {
            let mut code = 0u32;
            for fault in &state.active_faults {
                match fault.as_str() {
                    "grid_loss" => code |= 1 << 7,
                    "inverter_trip" => code |= 1 << 23,
                    "battery_over_temp" => code |= 1 << 0,
                    "comm_timeout" => code |= 1 << 24,
                    "sensor_drift" => code |= 1 << 30,
                    _ => {}
                }
            }
            code
        };
        // Three-phase inverter fault words for IR(1300)-IR(1307). Each word is a
        // 16-bit MSB-first bitmask decoded by `_inverter_fault_code2`
        // (givenergy-modbus model/inverter_threephase.py) / `inverter_fault_code2`
        // (giv_tcp model/threephase.py): list-index `i` ↔ bit `15-i`.
        //   grid_loss         → IR1301 bit 0  ("No Grid connection",       idx 15)
        //   inverter_trip     → IR1305 bit 4  ("Relay fault",              idx 11)
        //   battery_over_temp → IR1307 bit 9  ("Battery over temperature", idx 6)
        //   comm_timeout      → IR1301 bit 15 ("Gateway Comm fault",       idx 0)
        //   sensor_drift      → IR1305 bit 13 ("NTC open",                 idx 2)
        let threephase_fault_words = || -> [u16; 8] {
            let mut w = [0u16; 8];
            for fault in &state.active_faults {
                match fault.as_str() {
                    "grid_loss" => w[1] |= 1 << 0,
                    "inverter_trip" => w[5] |= 1 << 4,
                    "battery_over_temp" => w[7] |= 1 << 9,
                    "comm_timeout" => w[1] |= 1 << 15,
                    "sensor_drift" => w[5] |= 1 << 13,
                    _ => {}
                }
            }
            w
        };
        // Three-phase / HV battery voltage: sum of per-module voltages from BatteryEngine.
        // BatteryEngine scales the LFP curve by 1.5× for ThreePhase (24S, 76.8V nominal).
        let hv_battery_v = state.batteries.iter().map(|b| b.voltage_v).sum::<f64>();

        for def in &self.defs {
            let key = def.store_key();

            // CT clamp meter registers (IR 60-89 on slave 0x01) return all zeros
            // when ct_meter_installed is false. The inverter's built-in grid
            // measurement (IR 30, IR 42-43 on slave 0x32) is independent of this.
            if def.name.starts_with("meter_") && !state.config.ct_meter_installed {
                self.values.insert(key, 0);
                continue;
            }

            let engineering: Option<f64> = match def.name.as_str() {
                // ================================================================
                // GivEnergy-native Input Registers (IR 0-59)
                // ================================================================

                // IR 0: Inverter status (givenergy-modbus `Status` enum:
                // 0=Waiting, 1=Normal, 2=Warning, 3=Fault). HEM reads FAULT as
                // its authoritative inverter-trip signal, so reflect it here.
                "ge_ir_status" => {
                    self.values.insert(
                        key,
                        if state.active_faults.iter().any(|f| f == "inverter_trip") {
                            3
                        } else {
                            1
                        },
                    );
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
                // IR 5: Grid voltage (×0.1 V). Zero when grid disconnected.
                "ge_ir_grid_voltage" => {
                    if state.grid.connected {
                        Some(240.0)
                    } else {
                        Some(0.0)
                    }
                }
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
                // Projected from the lifetime bucket (never reset at midnight)
                // so clients see a stable "lifetime to date" figure.
                "ge_ir_pv_total_high" | "ge_ir_pv_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.solar_lifetime_kwh, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                // IR 13: Grid frequency (×0.01 Hz). Zero when grid disconnected.
                "ge_ir_grid_frequency" => {
                    if state.grid.connected {
                        Some(50.0)
                    } else {
                        Some(0.0)
                    }
                }
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
                // IR 31: EPS backup power (W). Reported when EPS is engaged —
                // either by user HR(317)=1, or actively running during grid-loss
                // island mode. When grid is healthy but EPS is just armed, EPS
                // is not flowing any power so this reads 0. During grid-loss
                // the inverter island-mode supplies the house from the battery;
                // report that discharge power in GE-wire convention
                // (positive = power supplied to the load).
                "ge_ir_eps_backup_power" => Some(if state.enable_eps {
                    if state.active_faults.iter().any(|f| f == "grid_loss") {
                        // internal positive = charging → flip to GE wire
                        // (positive = discharging into the backup load).
                        (-state.total_battery_power_kw() * 1000.0).max(0.0)
                    } else {
                        0.0
                    }
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
                // IR 39-40: raw fault_code (displayed as hex, no named decoder —
                // giv_tcp does not decode it at all). Mirrors the HR(223-224)
                // inverter fault word for single-phase units. Three-phase units
                // surface named faults on IR(1300-1307) instead.
                "ge_ir_fault_code_high" | "ge_ir_fault_code_low" => {
                    let code = if is_three_phase {
                        0
                    } else {
                        single_phase_fault_word()
                    };
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
                "ge_ir_grid_apparent" => {
                    if state.grid.connected {
                        Some(state.inverter.ac_power_w.abs())
                    } else {
                        Some(0.0)
                    }
                }
                // IR 44: Inverter AC output energy today (×0.1 kWh).
                // GivTCP baseinverter.py: IR(44) = e_inverter_out_day.
                // For hybrid inverters this differs from solar by battery-discharge contribution.
                "ge_ir_pv_generation_today" => Some(state.energy_totals.inverter_output_kwh),
                // IR 45-46: Inverter AC output energy total (uint32, ×0.1 kWh)
                "ge_ir_pv_generation_total_high" | "ge_ir_pv_generation_total_low" => {
                    let (hi, lo) = u32_words(state.energy_totals.inverter_output_kwh, 0.1);
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
                // IR 49: system/work mode (2=on-grid, 1=off-grid, 3=fault).
                // Off-grid takes precedence: a grid_loss fault disconnects the
                // grid (see sim-faults), so report OFF_GRID rather than a
                // generic FAULT — this is the inverter's authoritative grid-loss
                // signal consumed by HEM's decoder.
                "ge_ir_system_mode" => Some(if !state.grid.connected {
                    1.0
                } else if !state.active_faults.is_empty() {
                    3.0
                } else {
                    2.0
                }),
                // IR 50: Battery voltage (×0.01 V)
                "ge_ir_battery_voltage" => {
                    if is_gateway_inverter(&state.config.inverter_type) {
                        Some(0.0)
                    } else {
                        let soc = state.aggregate_soc();
                        Some(44.0 + soc * 0.08)
                    }
                }
                // IR 51: Battery current (signed, ×0.01 A)
                "ge_ir_battery_current" => {
                    if is_gateway_inverter(&state.config.inverter_type) {
                        self.values.insert(key, 0);
                        continue;
                    }
                    // Same convention as battery power: negate so positive = discharging
                    let amps = -(state.total_battery_power_kw() * 1000.0 / 48.0);
                    self.values.insert(key, amps as i16 as u16);
                    continue;
                }
                // IR 52: Battery power (signed, +charging)
                "ge_ir_battery_power" => {
                    if is_gateway_inverter(&state.config.inverter_type) {
                        self.values.insert(key, 0);
                        continue;
                    }
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
                "ge_ir_battery_temperature" => {
                    if is_gateway_inverter(&state.config.inverter_type) {
                        Some(0.0)
                    } else {
                        Some(state.battery_temperature_celsius())
                    }
                }
                // IR 57-58: warning and inverter AC-terminal current
                "ge_ir_charger_warning_code" => Some(
                    if state.active_faults.iter().any(|f| f == "battery_over_temp") {
                        1.0
                    } else {
                        0.0
                    },
                ),
                "ge_ir_grid_port_current" => {
                    if state.grid.connected {
                        Some(state.inverter.ac_power_w.abs() / 240.0)
                    } else {
                        Some(0.0)
                    }
                }
                // IR 59: Battery SOC (%) — zero for Gateway (no direct batteries)
                "ge_ir_battery_soc" => {
                    if is_gateway_inverter(&state.config.inverter_type) {
                        Some(0.0)
                    } else {
                        Some(state.aggregate_soc())
                    }
                }
                // IR 180-183: model-dependent battery energy alt sources
                // alt1 counters use lifetime throughput to match IR 6-7
                "ge_ir_battery_discharge_total_alt1" => {
                    // For alt1, use half of throughput (approximating discharge portion)
                    // In real systems this would track discharge separately, but for sim
                    // we split throughput evenly between charge and discharge
                    let total_throughput: f64 =
                        state.batteries.iter().map(|b| b.throughput_kwh).sum();
                    Some(total_throughput / 2.0)
                }
                "ge_ir_battery_charge_total_alt1" => {
                    // For alt1, use half of throughput (approximating charge portion)
                    let total_throughput: f64 =
                        state.batteries.iter().map(|b| b.throughput_kwh).sum();
                    Some(total_throughput / 2.0)
                }
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
                // ================================================================
                // CT Clamp Meter Input Registers (IR 60-89)
                // Single-phase: all power on phase 1; phases 2/3 zero.
                // Three-phase: balanced split across all 3 phases.
                // ================================================================
                "meter_v_phase_1" | "meter_v_phase_2" | "meter_v_phase_3" => {
                    if !state.grid.connected {
                        Some(0.0)
                    } else if is_three_phase || def.name.ends_with('1') {
                        Some(240.0)
                    } else {
                        Some(0.0)
                    }
                }
                "meter_i_phase_1" | "meter_i_phase_2" | "meter_i_phase_3" => {
                    let i = if is_three_phase {
                        state.grid.power_w.abs() / 3.0 / 240.0
                    } else if def.name.ends_with('1') {
                        state.grid.power_w.abs() / 240.0
                    } else {
                        0.0
                    };
                    self.values.insert(key, (i * 100.0) as u16);
                    continue;
                }
                "meter_i_ln" => {
                    self.values.insert(key, 0);
                    continue;
                }
                "meter_i_total" => {
                    let i = if is_three_phase {
                        state.grid.power_w.abs() / 3.0 / 240.0 * 3.0 // sum of 3 phases
                    } else {
                        state.grid.power_w.abs() / 240.0
                    };
                    self.values.insert(key, (i * 100.0) as u16);
                    continue;
                }
                "meter_p_active_phase_1" | "meter_p_active_phase_2" | "meter_p_active_phase_3" => {
                    // GivEnergy convention: +W = import, −W = export
                    let p = if is_three_phase {
                        state.grid.power_w / 3.0
                    } else if def.name.ends_with('1') {
                        state.grid.power_w
                    } else {
                        0.0
                    };
                    let clamped = p.clamp(-32768.0, 32767.0);
                    self.values.insert(key, clamped as i16 as u16);
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
                "meter_p_apparent_phase_1"
                | "meter_p_apparent_phase_2"
                | "meter_p_apparent_phase_3" => {
                    let p = if is_three_phase {
                        state.grid.power_w.abs() / 3.0
                    } else if def.name.ends_with('1') {
                        state.grid.power_w.abs()
                    } else {
                        0.0
                    };
                    self.values.insert(key, p as u16);
                    continue;
                }
                "meter_p_apparent_total" => {
                    self.values.insert(key, state.grid.power_w.abs() as u16);
                    continue;
                }
                "meter_pf_phase_1" | "meter_pf_phase_2" | "meter_pf_phase_3" => {
                    let has_power = if is_three_phase {
                        state.grid.power_w.abs() > 1.0
                    } else {
                        def.name.ends_with('1') && state.grid.power_w.abs() > 1.0
                    };
                    self.values
                        .insert(key, if has_power { 1000i16 as u16 } else { 0 });
                    continue;
                }
                "meter_pf_total" => {
                    self.values.insert(
                        key,
                        if state.grid.power_w.abs() > 1.0 {
                            1000i16 as u16
                        } else {
                            0
                        },
                    );
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
                        // GivTCP inverter_max_power LUT: 0x2106=8000W.
                        // No single-phase 10kW DTC exists in GivTCP's database;
                        // 0x2102 is recognised as 4600W by GivTCP — closest
                        // available, but the client will under-size this unit.
                        "Gen3Hybrid8kW" => 0x2106,
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
                // Gateway has no direct batteries — leave as zeros.
                "ge_hr_first_battery_serial_0"
                | "ge_hr_first_battery_serial_1"
                | "ge_hr_first_battery_serial_2"
                | "ge_hr_first_battery_serial_3"
                | "ge_hr_first_battery_serial_4" => {
                    if is_gateway_inverter(&state.config.inverter_type) {
                        self.values.insert(key, 0);
                        continue;
                    }
                    let v = match def.name.as_str() {
                        "ge_hr_first_battery_serial_0" => (b'B' as u16) << 8 | b'A' as u16,
                        "ge_hr_first_battery_serial_1" => (b'T' as u16) << 8 | b'0' as u16,
                        "ge_hr_first_battery_serial_2" => (b'0' as u16) << 8 | b'0' as u16,
                        "ge_hr_first_battery_serial_3" => (b'0' as u16) << 8 | b'1' as u16,
                        "ge_hr_first_battery_serial_4" => (b' ' as u16) << 8 | b' ' as u16,
                        _ => 0,
                    };
                    self.values.insert(key, v);
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
                // HR 18: First battery BMS firmware version — zero for Gateway (no direct batteries)
                "ge_hr_first_battery_bms_firmware" => {
                    if is_gateway_inverter(&state.config.inverter_type) {
                        self.values.insert(key, 0);
                    } else {
                        self.values.insert(key, 100);
                    }
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
                    // HR 43: GivTCP packs charge_target_soc (high byte) and
                    // discharge_target_soc (low byte) — configuration setpoints.
                    // If no schedule is set, use sensible defaults (100% charge, 4% discharge).
                    self.values.insert(key, 100u16 << 8 | 4u16);
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
                "ge_hr_enable_standard_self_consumption_logic" => {
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
                    // Use lifetime throughput to match IR 6-7 (split evenly for discharge)
                    let total_throughput: f64 =
                        state.batteries.iter().map(|b| b.throughput_kwh).sum();
                    let (hi, lo) = u32_words(total_throughput / 2.0, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "ge_hr_battery_charge_total_alt2_high" | "ge_hr_battery_charge_total_alt2_low" => {
                    // Use lifetime throughput to match IR 6-7 (split evenly for charge)
                    let total_throughput: f64 =
                        state.batteries.iter().map(|b| b.throughput_kwh).sum();
                    let (hi, lo) = u32_words(total_throughput / 2.0, 0.1);
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
                "ge_hr_enable_battery_self_heating" => {
                    self.values
                        .insert(key, state.inverter.battery_self_heating as u16);
                    continue;
                }
                "ge_hr_enable_manual_battery_heater" => {
                    self.values
                        .insert(key, state.inverter.manual_battery_heater as u16);
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
                "battery_max_charge_kw" => Some(state.effective_max_charge_w() / 1000.0),
                "battery_max_discharge_kw" => Some(state.effective_max_discharge_w() / 1000.0),
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
                    // Gateway has no direct batteries — report 0 (batteries live in child AIO).
                    if is_gateway_inverter(&state.config.inverter_type) {
                        self.values.insert(key, 0);
                    } else {
                        self.values.insert(key, state.batteries.len() as u16);
                    }
                    continue;
                }
                // HR 223-224: inverter_errors / inverter_fault_messages (uint32,
                // MSB-first) — THE authoritative named-fault register, decoded by
                // `_inverter_fault_code` (givenergy-modbus) / `inverter_fault_code`
                // (giv_tcp) into human-readable fault names. Three-phase units do
                // NOT use this register for faults — they use IR(1300-1307).
                "ge_hr_inverter_errors_high" | "ge_hr_inverter_errors_low" => {
                    let code = if is_three_phase {
                        0
                    } else {
                        single_phase_fault_word()
                    };
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
                // IR 1300-1307: three-phase inverter fault words (16-bit each,
                // MSB-first). Decoded by `_inverter_fault_code2` (givenergy-modbus
                // threephase) / `inverter_fault_code2` (giv_tcp threephase). Zero
                // for single-phase units, which use HR(223-224) instead.
                "ge_ir_threephase_fault_0"
                | "ge_ir_threephase_fault_1"
                | "ge_ir_threephase_fault_2"
                | "ge_ir_threephase_fault_3"
                | "ge_ir_threephase_fault_4"
                | "ge_ir_threephase_fault_5"
                | "ge_ir_threephase_fault_6"
                | "ge_ir_threephase_fault_7" => {
                    let idx = (def.address - 1300) as usize;
                    let words = if is_three_phase {
                        threephase_fault_words()
                    } else {
                        [0u16; 8]
                    };
                    self.values.insert(key, words[idx]);
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
                "tph_ir_v_ac1" | "tph_ir_v_ac2" | "tph_ir_v_ac3" => {
                    if !state.grid.connected {
                        Some(0.0)
                    } else if is_three_phase {
                        Some(grid_v_ll) // line-to-line
                    } else {
                        Some(240.0)
                    }
                }
                "tph_ir_i_ac1" | "tph_ir_i_ac2" | "tph_ir_i_ac3" => {
                    if !state.grid.connected {
                        Some(0.0)
                    } else {
                        // Per-phase current.
                        // Single-phase: inverter throughput / V_PN / 3.
                        // Three-phase (CT display): grid current per phase = grid_power / 3 / V_PN.
                        let i = if is_three_phase {
                            state.grid.power_w.abs() / 3.0 / 240.0
                        } else {
                            state.inverter.ac_power_w.abs() / 240.0 / 3.0
                        };
                        Some(i)
                    }
                }
                "tph_ir_f_ac1" => {
                    if state.grid.connected {
                        Some(50.0)
                    } else {
                        Some(0.0)
                    }
                }
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
                    // Real-time import power (0 when exporting)
                    let import_w = state.grid.power_w.max(0.0);
                    let (hi, lo) = u32_words(import_w, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_p_meter_export_high" | "tph_ir_p_meter_export_low" => {
                    // Real-time export power (0 when importing)
                    let export_w = (-state.grid.power_w).max(0.0);
                    let (hi, lo) = u32_words(export_w, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_p_export_high" | "tph_ir_p_export_low" => {
                    // Mirror of p_meter_export at IR 1240-1241 (alternative address)
                    let export_w = (-state.grid.power_w).max(0.0);
                    let (hi, lo) = u32_words(export_w, 0.1);
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
                    // Per-phase house load. Client sets p_active_phase to 0
                    // for the synthetic CT meter; these are load display only.
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
                "tph_ir_v_battery_bms" => Some(hv_battery_v),
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
                "tph_ir_v_battery_pcs" => Some(hv_battery_v),
                "tph_ir_v_dc_bus" => Some(380.0),
                "tph_ir_v_inv_bus" => Some(380.0),
                "tph_ir_i_battery" => {
                    let amps = -(state.total_battery_power_kw() * 1000.0 / hv_battery_v);
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
                    let (hi, lo) = u32_words(state.energy_totals.solar_lifetime_kwh, 0.1);
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
                    // Use lifetime throughput to match IR 6-7 (split evenly for discharge)
                    let total_throughput: f64 =
                        state.batteries.iter().map(|b| b.throughput_kwh).sum();
                    let (hi, lo) = u32_words(total_throughput / 2.0, 0.1);
                    self.values
                        .insert(key, if def.name.ends_with("_high") { hi } else { lo });
                    continue;
                }
                "tph_ir_e_battery_charge_total_high" | "tph_ir_e_battery_charge_total_low" => {
                    // Use lifetime throughput to match IR 6-7 (split evenly for charge)
                    let total_throughput: f64 =
                        state.batteries.iter().map(|b| b.throughput_kwh).sum();
                    let (hi, lo) = u32_words(total_throughput / 2.0, 0.1);
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

        // Gateway aggregation bank (IR 1600-1859) + GW serial prefix.
        // No-op for non-gateway inverters (leaves the seeded zeros in place,
        // which makes the version-string validity check fail so clients
        // correctly conclude "no gateway present").
        self.project_gateway_bank(state);
    }

    /// Project the GivEnergy Gateway aggregation bank (Input Registers
    /// IR 1600-1859) plus the `GW`-prefixed serial number.
    ///
    /// Models a single-AIO Gateway topology: the `PlantState` represents the
    /// child AIO's physics (battery / inverter / solar / load / grid) and this
    /// method derives the gateway's measured/aggregated view of it. See
    /// `docs/gateway-register-reference.md` for the authoritative register map.
    ///
    /// Firmware variant selection:
    /// - V1 (`GA000009`, IR(1603) < 10): high-register-first uint32 totals,
    ///   aio1 serial @ 1831-1835, aio2 @ 1838-1842, aio3 @ 1845-1849.
    /// - V2 (`GA000010`, IR(1603) >= 10): low-register-first uint32 totals,
    ///   aio1 serial @ 1841-1845, aio2 @ 1848-1852, aio3 @ 1855-1859.
    ///   Controlled by `state.config.gateway_fw_version` (default 9 = V1).
    fn project_gateway_bank(&mut self, state: &sim_models::PlantState) {
        if !is_gateway_inverter(&state.config.inverter_type) {
            return;
        }

        // Helpers --------------------------------------------------------
        // Values are written as raw u16 directly into the Input space (key =
        // address), so scaling must be applied here rather than by the caller.
        let deci = |eng: f64| -> u16 { (eng * 10.0).round().clamp(0.0, 65535.0) as u16 };
        let i16_word = |watts: f64| -> u16 { watts.clamp(-32768.0, 32767.0) as i16 as u16 };
        let u16_word = |w: f64| -> u16 { w.round().clamp(0.0, 65535.0) as u16 };
        let is_v2 = state.config.gateway_fw_version >= 10;
        // uint32: byte order depends on variant.
        // V1: high-register-first (hi, lo). V2: low-register-first (lo, hi).
        let u32_words = |engineering: f64, scaling: f64| -> (u16, u16) {
            let raw = (engineering / scaling).max(0.0).round() as u32;
            let hi = (raw >> 16) as u16;
            let lo = (raw & 0xFFFF) as u16;
            if is_v2 { (lo, hi) } else { (hi, lo) }
        };
        let mut set = |addr: u16, v: u16| {
            self.values.insert(addr as u32, v);
        };

        let connected = state.grid.connected;
        let line_v = if connected { 240.0 } else { 0.0 };

        // Gateway wire convention for p_ac1 (1616), p_aio_total (1702), and
        // the per-AIO p_aioN_inverter (1816–1818) is the OPPOSITE of the
        // standard inverter's p_battery (IR 52): raw + = charging/in,
        // − = discharging/out. Confirmed by GivTCP read.py:1556, which
        // negates GEInv.p_aio_total to recover Battery_Power in its internal
        // + = discharging convention. Since total_battery_power_kw() is
        // already + = charging internally, we emit it VERBATIM here — do
        // not negate (negating would produce the inverted wire convention
        // that real GivEnergy gateway hardware does not use).
        let aio_power_w = state.total_battery_power_kw() * 1000.0;
        let pv_w = state.solar.generation_w;
        // Gateway house load correctly excludes the EV charger (key property):
        // `load.demand_w` is household-only; `evc` draw is tracked separately.
        let load_w = state.load.demand_w;
        let grid_w = state.grid.power_w; // + import / - export

        let et = &state.energy_totals;

        // 4.1 System state & version — IR 1600-1631 ----------------------
        // software_version = "GA000009" (V1) or "GA0000010" (V2).
        // gateway_version converter:
        //   prefix = latin1(r0|r1) = "GA00", digits = decimal of each byte of
        //   r2|r3 → e.g. "00"+"09" → "GA000009" (V1) or "00"+"10" → "GA0000010" (V2).
        //   IR(1603) < 10 selects V1, >= 10 selects V2.
        let fw_byte = state.config.gateway_fw_version;
        set(1600, (b'G' as u16) << 8 | b'A' as u16); // 0x4741
        set(1601, (b'0' as u16) << 8 | b'0' as u16); // 0x3030
        set(1602, 0x0000);
        set(1603, fw_byte); // < 10 = V1, >= 10 = V2
        // work_mode = On Grid (2)
        set(1604, 2);
        set(1608, deci(line_v)); // v_grid
        set(
            1609,
            (grid_w / 240.0 * 10.0).clamp(-32768.0, 32767.0) as i16 as u16,
        ); // i_grid signed (deci A)
        set(1610, deci(line_v)); // v_load
        set(1611, deci(load_w / 240.0)); // i_load (deci A)
        set(1612, deci(pv_w / 240.0)); // i_pv (deci A)
        set(1616, i16_word(aio_power_w)); // p_ac1 (signed)
        set(1617, u16_word(pv_w)); // p_pv
        set(1618, u16_word(load_w)); // p_load (excludes EV)
        set(1619, i16_word(0.0)); // p_liberty (smart load, unmodelled)
        set(1620, 0); // fault_protection hi
        set(1621, 0); // fault_protection lo
        set(1622, 0); // gateway_fault_codes hi
        set(1623, 0); // gateway_fault_codes lo
        set(1624, deci(line_v)); // v_grid_relay
        set(1625, deci(line_v)); // v_inverter_relay
        // first_inverter_serial_number (IR 1627-1631) = AIO1 serial
        let aio1_serial = make_aio_serial(0);
        for i in 0..5usize {
            let hi = aio1_serial[2 * i];
            let lo = aio1_serial[2 * i + 1];
            set(1627 + i as u16, (hi as u16) << 8 | lo as u16);
        }

        // 4.2 / 4.3 Daily + lifetime energy — IR 1640-1657 ----------------
        // Daily counters and lifetime totals both derive from the cumulative
        // energy_totals (the sim does not reset daily counters — consistent
        // with the standard IR 25/35 "today" registers).
        // uint32 byte order depends on variant (V1 = hi-first, V2 = lo-first).
        set(1640, deci(et.grid_import_kwh)); // e_grid_import_today
        let (h, l) = u32_words(et.grid_import_kwh, 0.1);
        set(1641, h);
        set(1642, l); // e_grid_import_total
        set(1643, deci(et.solar_generation_kwh)); // e_pv_today
        let (h, l) = u32_words(et.solar_generation_kwh, 0.1);
        set(1644, h);
        set(1645, l); // e_pv_total
        set(1646, deci(et.grid_export_kwh)); // e_grid_export_today
        let (h, l) = u32_words(et.grid_export_kwh, 0.1);
        set(1647, h);
        set(1648, l); // e_grid_export_total
        set(1649, deci(et.battery_charge_kwh)); // e_aio_charge_today
        let (h, l) = u32_words(et.battery_charge_kwh, 0.1);
        set(1650, h);
        set(1651, l); // e_aio_charge_total
        set(1652, deci(et.battery_discharge_kwh)); // e_aio_discharge_today
        let (h, l) = u32_words(et.battery_discharge_kwh, 0.1);
        set(1653, h);
        set(1654, l); // e_aio_discharge_total
        set(1655, deci(et.load_consumption_kwh)); // e_load_today
        let (h, l) = u32_words(et.load_consumption_kwh, 0.1);
        set(1656, h);
        set(1657, l); // e_load_total

        // 4.4 AIO summary — IR 1700-1704 ----------------------------------
        let num_aio = state.config.parallel_aio_num.clamp(1, 3) as usize;
        set(1700, num_aio as u16); // parallel_aio_num
        set(1701, num_aio as u16); // parallel_aio_online_num (all online)
        set(1702, i16_word(aio_power_w)); // p_aio_total (signed)
        // aio_state: 1 = charging, 2 = discharging, 0 = idle
        let batt_kw = state.total_battery_power_kw();
        let aio_state = if batt_kw > 0.01 {
            1u16
        } else if batt_kw < -0.01 {
            2u16
        } else {
            0u16
        };
        set(1703, aio_state);
        set(1704, 100); // battery_firmware_version

        // Per-AIO data: distribute battery data evenly across N AIO units.
        // Since the PlantState models one shared battery stack, each AIO
        // gets an equal fraction of the aggregate SOC / power / energy.
        let per_aio_power_w = aio_power_w / num_aio as f64;
        let per_aio_charge_kwh = et.battery_charge_kwh / num_aio as f64;
        let per_aio_discharge_kwh = et.battery_discharge_kwh / num_aio as f64;
        let battery_charge_total = et.battery_charge_kwh; // aggregate
        let battery_discharge_total = et.battery_discharge_kwh; // aggregate
        let per_aio_charge_total = battery_charge_total / num_aio as f64;
        let per_aio_discharge_total = battery_discharge_total / num_aio as f64;
        let soc = state.aggregate_soc(); // all AIOs share the same SoC

        // V1/V2 AIO serial address bases.
        fn aio_serial_addrs(aio_idx: usize, is_v2: bool) -> u16 {
            if is_v2 {
                match aio_idx {
                    0 => 1841,
                    1 => 1848,
                    2 => 1855,
                    _ => 1841,
                }
            } else {
                match aio_idx {
                    0 => 1831,
                    1 => 1838,
                    2 => 1845,
                    _ => 1831,
                }
            }
        }

        // Generate a unique 10-char Latin-1 serial for each AIO.
        // "SA24230001", "SA24230002", "SA24230003" mirror real GivEnergy
        // serial shapes (AAYYWW + sequential suffix).
        fn make_aio_serial(aio_idx: usize) -> [u8; 10] {
            let suffix = aio_idx + 1;
            let s = format!("SA2423{suffix:04}");
            let mut buf = [b' '; 10];
            let bytes = s.as_bytes();
            for (i, &b) in bytes.iter().enumerate().take(10) {
                buf[i] = b;
            }
            buf
        }

        // Shared SOC for all AIOs (single stack model)
        for aio_i in 0..3 {
            let reg = [1801, 1802, 1803][aio_i];
            let v = if aio_i < num_aio { u16_word(soc) } else { 0 };
            set(reg, v);
        }

        // Per-AIO charge (1705-1713): today + total for each active AIO
        // Layout: AIO1 @ 1705-1707, AIO2 @ 1708-1710, AIO3 @ 1711-1713
        for aio_i in 0..3 {
            let base = 1705u16 + aio_i as u16 * 3;
            if aio_i < num_aio {
                set(base, deci(per_aio_charge_kwh)); // today
                let (h, l) = u32_words(per_aio_charge_total, 0.1);
                set(base + 1, h);
                set(base + 2, l);
            } else {
                set(base, 0);
                set(base + 1, 0);
                set(base + 2, 0);
            }
        }

        // Per-AIO discharge (1750-1758): today + total for each active AIO
        // Layout: AIO1 @ 1750-1752, AIO2 @ 1753-1755, AIO3 @ 1756-1758
        for aio_i in 0..3 {
            let base = 1750u16 + aio_i as u16 * 3;
            if aio_i < num_aio {
                set(base, deci(per_aio_discharge_kwh)); // today
                let (h, l) = u32_words(per_aio_discharge_total, 0.1);
                set(base + 1, h);
                set(base + 2, l);
            } else {
                set(base, 0);
                set(base + 1, 0);
                set(base + 2, 0);
            }
        }

        // 4.9 Battery aggregate energy + per-AIO SOC — IR 1795-1800 -------
        set(1795, deci(battery_charge_total)); // e_battery_charge_today
        let (h, l) = u32_words(battery_charge_total, 0.1);
        set(1796, h);
        set(1797, l); // e_battery_charge_total
        set(1798, deci(battery_discharge_total)); // e_battery_discharge_today
        let (h, l) = u32_words(battery_discharge_total, 0.1);
        set(1799, h);
        set(1800, l); // e_battery_discharge_total

        // 4.10 Per-AIO inverter power — IR 1816-1818 ---------------------
        for aio_i in 0..3 {
            let reg = 1816u16 + aio_i as u16;
            let v = if aio_i < num_aio {
                i16_word(per_aio_power_w)
            } else {
                0
            };
            set(reg, v);
        }

        // 4.11 Per-AIO serial numbers — IR 1831-1859 (variant-dependent) ---
        // Clear the full range 1831-1859, then write active AIO serials.
        for addr in 1831u16..=1859 {
            set(addr, 0);
        }
        for aio_i in 0..num_aio {
            let base = aio_serial_addrs(aio_i, is_v2);
            let serial = make_aio_serial(aio_i);
            for i in 0..5usize {
                let addr = base + i as u16;
                let hi = serial[2 * i];
                let lo = serial[2 * i + 1];
                set(addr, (hi as u16) << 8 | lo as u16);
            }
        }

        // Serial number (HR 13-17): emit a `GW` prefix so prefix-based
        // detection classifies the device as a Gateway. "GW2423G192" mirrors
        // the GivTCP reference example.
        let gw_serial: [u8; 10] = *b"GW2423G192";
        for i in 0..5usize {
            let hi = gw_serial[2 * i];
            let lo = gw_serial[2 * i + 1];
            self.values
                .insert(HOLDING_OFFSET + 13 + i as u32, (hi as u16) << 8 | lo as u16);
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
    /// AC-coupled inverters are basic single-phase slot devices: one charge slot
    /// (HR 94-95) and one discharge slot (HR 56-57), no slot 2/extended slots.
    /// Gen3/AIO/HV-Gen3/Gen4 inverters write charge slot 2 to HR 243-244
    /// (not HR 31-32, which contains stale/garbage data on real Gen3 firmware).
    pub fn project_schedule_for(&mut self, schedule: &sim_models::Schedule, inverter_type: &str) {
        // Gen3+ uses HR 243-244 for charge slot 2; Gen1 uses HR 31-32.
        // Gen2 and AC-coupled do not support slot 2.
        let gen3_ext = uses_gen3_extended_slots(inverter_type);
        let slot2 = has_slot_2(inverter_type);
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
        if gen3_ext && slot2 {
            // Gen3+ firmware stores charge slot 2 at HR 243-244.
            // HR 31-32 is stale/garbage on real Gen3 — leave at disabled sentinel.
            self.write(243, cs2_start);
            self.write(244, cs2_end);
        } else if slot2 {
            // Gen1: classic HR 31-32 for charge slot 2.
            self.write(31, cs2_start);
            self.write(32, cs2_end);
        }
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

        let (ds1_start, ds1_end) = slot_pair(schedule.discharge_start, schedule.discharge_end);
        self.write(56, ds1_start);
        self.write(57, ds1_end);
        let (ds2_start, ds2_end) = if !slot2 {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_2, schedule.discharge_end_2)
        };
        self.write(44, ds2_start);
        self.write(45, ds2_end);
        let no_extended = !slot2;
        let (ds3_s, ds3_e) = if no_extended {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_3, schedule.discharge_end_3)
        };
        self.write(276, ds3_s);
        self.write(277, ds3_e);
        let (ds4_s, ds4_e) = if no_extended {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_4, schedule.discharge_end_4)
        };
        self.write(279, ds4_s);
        self.write(280, ds4_e);
        let (ds5_s, ds5_e) = if no_extended {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_5, schedule.discharge_end_5)
        };
        self.write(282, ds5_s);
        self.write(283, ds5_e);
        let (ds6_s, ds6_e) = if no_extended {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_6, schedule.discharge_end_6)
        };
        self.write(285, ds6_s);
        self.write(286, ds6_e);
        let (ds7_s, ds7_e) = if no_extended {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_7, schedule.discharge_end_7)
        };
        self.write(288, ds7_s);
        self.write(289, ds7_e);
        let (ds8_s, ds8_e) = if no_extended {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_8, schedule.discharge_end_8)
        };
        self.write(291, ds8_s);
        self.write(292, ds8_e);
        let (ds9_s, ds9_e) = if no_extended {
            (60, 60)
        } else {
            slot_pair(schedule.discharge_start_9, schedule.discharge_end_9)
        };
        self.write(294, ds9_s);
        self.write(295, ds9_e);
        let (ds10_s, ds10_e) = if no_extended {
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

        let discharge_enabled = schedule.enable_discharge
            || schedule.discharge_start != schedule.discharge_end
            || schedule.discharge_start_2 != schedule.discharge_end_2
            || schedule.discharge_start_3 != schedule.discharge_end_3
            || schedule.discharge_start_4 != schedule.discharge_end_4
            || schedule.discharge_start_5 != schedule.discharge_end_5
            || schedule.discharge_start_6 != schedule.discharge_end_6
            || schedule.discharge_start_7 != schedule.discharge_end_7
            || schedule.discharge_start_8 != schedule.discharge_end_8
            || schedule.discharge_start_9 != schedule.discharge_end_9
            || schedule.discharge_start_10 != schedule.discharge_end_10;
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
        self.write(1109, 4);
        self.write(1112, if charge_enabled { 1 } else { 0 });
        self.write(272, schedule.discharge_target_soc as u16);
        self.write(
            275,
            if slot2 {
                schedule.discharge_target_soc_2 as u16
            } else {
                0
            },
        );
        self.write(
            278,
            if no_extended {
                0
            } else {
                schedule.discharge_target_soc_3 as u16
            },
        );
        self.write(
            281,
            if no_extended {
                0
            } else {
                schedule.discharge_target_soc_4 as u16
            },
        );
        self.write(
            284,
            if no_extended {
                0
            } else {
                schedule.discharge_target_soc_5 as u16
            },
        );
        self.write(
            287,
            if no_extended {
                0
            } else {
                schedule.discharge_target_soc_6 as u16
            },
        );
        self.write(
            290,
            if no_extended {
                0
            } else {
                schedule.discharge_target_soc_7 as u16
            },
        );
        self.write(
            293,
            if no_extended {
                0
            } else {
                schedule.discharge_target_soc_8 as u16
            },
        );
        self.write(
            296,
            if no_extended {
                0
            } else {
                schedule.discharge_target_soc_9 as u16
            },
        );
        self.write(
            299,
            if no_extended {
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
            .unwrap_or(4);
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

        // Cell voltages: IR 60-75 (mV). Simulate 16 cells from total voltage,
        // with ≤1% deterministic per-cell variation (see cell_voltage_factor).
        let cell_count = 16usize;
        let base_mv = battery.voltage_v * 1000.0 / cell_count as f64;
        for (i, reg) in regs.iter_mut().take(cell_count).enumerate() {
            *reg = (base_mv * cell_voltage_factor(module_index, i)).round() as u16; // IR 60+i
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
        regs[31] = 0x0E10; // IR 91: status_3/status_4
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

        // IR 105: e_battery_discharge_total (uint16, ×0.1 kWh)
        // IR 106: e_battery_charge_total (uint16, ×0.1 kWh)
        // GivTCP battery.py reads these. Use per-module throughput as a proxy.
        let throughput_deci = (battery.throughput_kwh * 10.0).round() as u16;
        regs[45] = throughput_deci; // IR 105: discharge total
        regs[46] = throughput_deci; // IR 106: charge total

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

        // IR 67: cluster_cell_voltage (milli V), IR 68: cluster_cell_temperature (deci °C),
        // IR 70: status. Set after aggregate values are computed below.

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

        // IR 67: cluster_cell_voltage (milli V) — average cell voltage across stack
        let avg_cell_v = stack_voltage / (n as f64 * 24.0);
        regs[7] = (avg_cell_v * 1000.0).round() as u16;
        // IR 68: cluster_cell_temperature (deci °C) — average across all modules
        let avg_temp = batteries.iter().map(|b| b.temperature_celsius).sum::<f64>() / n as f64;
        regs[8] = (avg_temp * 10.0).round() as u16;
        // IR 70: status — 0x01 = normal operation
        regs[10] = 0x01;

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

        // IR 98: nominal_capacity (deci Ah). Calculated from the user-configured
        // per-module capacity and the three-phase stack nominal voltage (76.8 V).
        // Previously hardcoded to 510 (51.0 Ah × 76.8 V = 3.9 kWh per module)
        // which didn't reflect user-chosen battery size.
        let module_capacity_kwh = batteries
            .first()
            .map(|b| b.nominal_capacity_kwh)
            .unwrap_or(3.4);
        let module_nominal_ah = (module_capacity_kwh * 1000.0 / 76.8 * 10.0).round() as u16;
        regs[38] = module_nominal_ah;
        // IR 99: remaining_battery_capacity (deci Ah), scaled by SOC
        let remaining_ah = module_nominal_ah as f64 * avg_soc / 100.0;
        regs[39] = remaining_ah.round() as u16;
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
        // nudged by SOC so the cluster looks healthy and varying, plus ≤1%
        // deterministic per-cell noise (see cell_voltage_factor).
        let cell_v = 3.3 + (battery.soc_percent - 50.0) * 0.003;
        let base_mv = cell_v * 1000.0;
        for (i, reg) in regs.iter_mut().take(cells).enumerate() {
            *reg = (base_mv * cell_voltage_factor(module_index, i)).round() as u16; // IR 60+i
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
        // HR 199: GivTCP names this "enable_standard_self_consumption_logic".
        // Internally tracked as enable_inverter_parallel_mode.
        RegisterDef {
            address: 199,
            name: "ge_hr_enable_standard_self_consumption_logic".into(),
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
            name: "ge_hr_enable_battery_self_heating".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 172,
            name: "ge_hr_enable_manual_battery_heater".into(),
            category: C::Battery,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        // ---- Battery (200–239) ----
        // Note: HR 200 overlaps GivTCP's cmd_bms_flash_update, but no client
        // actually reads this register — kept for backward compat.
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
        // HR 223-224: Inverter fault codes (uint32).
        // GivTCP reads these as inverter_errors. Non-zero = fault active.
        RegisterDef {
            address: 223,
            name: "ge_hr_inverter_errors_high".into(),
            category: C::Inverter,
            typ: T::U16,
            scaling_factor: 1.0,
            access: ReadOnly,
            space: Holding,
        },
        RegisterDef {
            address: 224,
            name: "ge_hr_inverter_errors_low".into(),
            category: C::Inverter,
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
        // ================================================================
        // Three-phase inverter fault bank (Input Registers, IR 1300-1307)
        // Populated only for three-phase inverters (ThreePhase*, ACThreePhase);
        // single-phase units keep these at zero and use HR(223-224) instead.
        // Each register is one 16-bit MSB-first fault word, decoded by the
        // GivEnergy `_inverter_fault_code2` / `inverter_fault_code2` table.
        // ================================================================
        tph_fault_def(1300, "ge_ir_threephase_fault_0"),
        tph_fault_def(1301, "ge_ir_threephase_fault_1"),
        tph_fault_def(1302, "ge_ir_threephase_fault_2"),
        tph_fault_def(1303, "ge_ir_threephase_fault_3"),
        tph_fault_def(1304, "ge_ir_threephase_fault_4"),
        tph_fault_def(1305, "ge_ir_threephase_fault_5"),
        tph_fault_def(1306, "ge_ir_threephase_fault_6"),
        tph_fault_def(1307, "ge_ir_threephase_fault_7"),
        // ================================================================
        // GivEnergy Gateway aggregation bank (Input Registers, IR 1600-1859)
        // Read via fn 0x04 at the gateway slave. Populated only when the
        // inverter type is a Gateway (see project_gateway_bank). Values are
        // written as raw u16 directly, so scaling_factor here is informational.
        // See docs/gateway-register-reference.md for the authoritative map.
        // ================================================================
        gw_ir_def(1600, "gw_sw_version_0", 1.0),
        gw_ir_def(1601, "gw_sw_version_1", 1.0),
        gw_ir_def(1602, "gw_sw_version_2", 1.0),
        gw_ir_def(1603, "gw_sw_version_3", 1.0),
        gw_ir_def(1604, "gw_work_mode", 1.0),
        gw_ir_def(1608, "gw_v_grid", 0.1),
        gw_ir_def(1609, "gw_i_grid", 0.1),
        gw_ir_def(1610, "gw_v_load", 0.1),
        gw_ir_def(1611, "gw_i_load", 0.1),
        gw_ir_def(1612, "gw_i_pv", 0.1),
        gw_ir_def(1616, "gw_p_ac1", 1.0),
        gw_ir_def(1617, "gw_p_pv", 1.0),
        gw_ir_def(1618, "gw_p_load", 1.0),
        gw_ir_def(1619, "gw_p_liberty", 1.0),
        gw_ir_def(1620, "gw_fault_protection_hi", 1.0),
        gw_ir_def(1621, "gw_fault_protection_lo", 1.0),
        gw_ir_def(1622, "gw_fault_codes_hi", 1.0),
        gw_ir_def(1623, "gw_fault_codes_lo", 1.0),
        gw_ir_def(1624, "gw_v_grid_relay", 0.1),
        gw_ir_def(1625, "gw_v_inverter_relay", 0.1),
        gw_ir_def(1627, "gw_first_inv_serial_0", 1.0),
        gw_ir_def(1628, "gw_first_inv_serial_1", 1.0),
        gw_ir_def(1629, "gw_first_inv_serial_2", 1.0),
        gw_ir_def(1630, "gw_first_inv_serial_3", 1.0),
        gw_ir_def(1631, "gw_first_inv_serial_4", 1.0),
        gw_ir_def(1640, "gw_e_grid_import_today", 0.1),
        gw_ir_def(1641, "gw_e_grid_import_total_hi", 0.1),
        gw_ir_def(1642, "gw_e_grid_import_total_lo", 0.1),
        gw_ir_def(1643, "gw_e_pv_today", 0.1),
        gw_ir_def(1644, "gw_e_pv_total_hi", 0.1),
        gw_ir_def(1645, "gw_e_pv_total_lo", 0.1),
        gw_ir_def(1646, "gw_e_grid_export_today", 0.1),
        gw_ir_def(1647, "gw_e_grid_export_total_hi", 0.1),
        gw_ir_def(1648, "gw_e_grid_export_total_lo", 0.1),
        gw_ir_def(1649, "gw_e_aio_charge_today", 0.1),
        gw_ir_def(1650, "gw_e_aio_charge_total_hi", 0.1),
        gw_ir_def(1651, "gw_e_aio_charge_total_lo", 0.1),
        gw_ir_def(1652, "gw_e_aio_discharge_today", 0.1),
        gw_ir_def(1653, "gw_e_aio_discharge_total_hi", 0.1),
        gw_ir_def(1654, "gw_e_aio_discharge_total_lo", 0.1),
        gw_ir_def(1655, "gw_e_load_today", 0.1),
        gw_ir_def(1656, "gw_e_load_total_hi", 0.1),
        gw_ir_def(1657, "gw_e_load_total_lo", 0.1),
        gw_ir_def(1700, "gw_parallel_aio_num", 1.0),
        gw_ir_def(1701, "gw_parallel_aio_online_num", 1.0),
        gw_ir_def(1702, "gw_p_aio_total", 1.0),
        gw_ir_def(1703, "gw_aio_state", 1.0),
        gw_ir_def(1704, "gw_battery_firmware", 1.0),
        gw_ir_def(1705, "gw_e_aio1_charge_today", 0.1),
        gw_ir_def(1706, "gw_e_aio1_charge_total_hi", 0.1),
        gw_ir_def(1707, "gw_e_aio1_charge_total_lo", 0.1),
        gw_ir_def(1708, "gw_e_aio2_charge_today", 0.1),
        gw_ir_def(1709, "gw_e_aio2_charge_total_hi", 0.1),
        gw_ir_def(1710, "gw_e_aio2_charge_total_lo", 0.1),
        gw_ir_def(1711, "gw_e_aio3_charge_today", 0.1),
        gw_ir_def(1712, "gw_e_aio3_charge_total_hi", 0.1),
        gw_ir_def(1713, "gw_e_aio3_charge_total_lo", 0.1),
        gw_ir_def(1750, "gw_e_aio1_discharge_today", 0.1),
        gw_ir_def(1751, "gw_e_aio1_discharge_total_hi", 0.1),
        gw_ir_def(1752, "gw_e_aio1_discharge_total_lo", 0.1),
        gw_ir_def(1753, "gw_e_aio2_discharge_today", 0.1),
        gw_ir_def(1754, "gw_e_aio2_discharge_total_hi", 0.1),
        gw_ir_def(1755, "gw_e_aio2_discharge_total_lo", 0.1),
        gw_ir_def(1756, "gw_e_aio3_discharge_today", 0.1),
        gw_ir_def(1757, "gw_e_aio3_discharge_total_hi", 0.1),
        gw_ir_def(1758, "gw_e_aio3_discharge_total_lo", 0.1),
        gw_ir_def(1795, "gw_e_battery_charge_today", 0.1),
        gw_ir_def(1796, "gw_e_battery_charge_total_hi", 0.1),
        gw_ir_def(1797, "gw_e_battery_charge_total_lo", 0.1),
        gw_ir_def(1798, "gw_e_battery_discharge_today", 0.1),
        gw_ir_def(1799, "gw_e_battery_discharge_total_hi", 0.1),
        gw_ir_def(1800, "gw_e_battery_discharge_total_lo", 0.1),
        gw_ir_def(1801, "gw_aio1_soc", 1.0),
        gw_ir_def(1802, "gw_aio2_soc", 1.0),
        gw_ir_def(1803, "gw_aio3_soc", 1.0),
        gw_ir_def(1816, "gw_p_aio1_inverter", 1.0),
        gw_ir_def(1817, "gw_p_aio2_inverter", 1.0),
        gw_ir_def(1818, "gw_p_aio3_inverter", 1.0),
        // Serial address range 1831-1859 — used by both V1 and V2 at
        // different offsets (V1: aio1@1831, aio2@1838, aio3@1845;
        // V2: aio1@1841, aio2@1848, aio3@1855). Define all so the
        // snapshot always has matching catalogue entries.
        gw_ir_def(1831, "gw_serial_1831", 1.0),
        gw_ir_def(1832, "gw_serial_1832", 1.0),
        gw_ir_def(1833, "gw_serial_1833", 1.0),
        gw_ir_def(1834, "gw_serial_1834", 1.0),
        gw_ir_def(1835, "gw_serial_1835", 1.0),
        gw_ir_def(1836, "gw_serial_1836", 1.0),
        gw_ir_def(1837, "gw_serial_1837", 1.0),
        gw_ir_def(1838, "gw_serial_1838", 1.0),
        gw_ir_def(1839, "gw_serial_1839", 1.0),
        gw_ir_def(1840, "gw_serial_1840", 1.0),
        gw_ir_def(1841, "gw_serial_1841", 1.0),
        gw_ir_def(1842, "gw_serial_1842", 1.0),
        gw_ir_def(1843, "gw_serial_1843", 1.0),
        gw_ir_def(1844, "gw_serial_1844", 1.0),
        gw_ir_def(1845, "gw_serial_1845", 1.0),
        gw_ir_def(1846, "gw_serial_1846", 1.0),
        gw_ir_def(1847, "gw_serial_1847", 1.0),
        gw_ir_def(1848, "gw_serial_1848", 1.0),
        gw_ir_def(1849, "gw_serial_1849", 1.0),
        gw_ir_def(1850, "gw_serial_1850", 1.0),
        gw_ir_def(1851, "gw_serial_1851", 1.0),
        gw_ir_def(1852, "gw_serial_1852", 1.0),
        gw_ir_def(1853, "gw_serial_1853", 1.0),
        gw_ir_def(1854, "gw_serial_1854", 1.0),
        gw_ir_def(1855, "gw_serial_1855", 1.0),
        gw_ir_def(1856, "gw_serial_1856", 1.0),
        gw_ir_def(1857, "gw_serial_1857", 1.0),
        gw_ir_def(1858, "gw_serial_1858", 1.0),
        gw_ir_def(1859, "gw_serial_1859", 1.0),
    ]
}

/// Build a read-only Input Register definition in the Gateway aggregation bank.
fn gw_ir_def(address: u16, name: &'static str, scaling: f64) -> RegisterDef {
    RegisterDef {
        address,
        name: name.into(),
        category: RegisterCategory::Gateway,
        typ: RegisterType::U16,
        scaling_factor: scaling,
        access: Access::ReadOnly,
        space: RegisterSpace::Input,
    }
}

/// A three-phase inverter fault-word register (IR 1300-1307). Each register is
/// one 16-bit MSB-first word from the `_inverter_fault_code2` /
/// `inverter_fault_code2` decoder. See `project_from_state` for the bit map.
fn tph_fault_def(address: u16, name: &'static str) -> RegisterDef {
    RegisterDef {
        address,
        name: name.into(),
        category: RegisterCategory::Inverter,
        typ: RegisterType::U16,
        scaling_factor: 1.0,
        access: Access::ReadOnly,
        space: RegisterSpace::Input,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sim_models::INVERTER_HOURS_PER_YEAR;
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
            // Use GIV-BAT-3.4-HV sizing: 3.4 kWh nominal per module
            b.soc_percent = 60.0;
            b.voltage_v = 51.2;
            b.soh = 0.95;
            b.nominal_capacity_kwh = 3.4;
            b.capacity_kwh = b.nominal_capacity_kwh * b.soh;
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
        // IR 98 nominal_capacity = (3.4 kWh * 1000 / 76.8 V * 10) rounded
        // = (3400 / 76.8 * 10).round() = 442.7 -> 443 deci Ah
        assert_eq!(bcu[38], 443, "IR(98) per-module nominal Ah from 3.4 kWh");
        // IR 99 remaining = 443 deci Ah * 60% = 265.8 -> 266
        assert_eq!(bcu[39], 266, "IR(99) remaining capacity scales with SOC");
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
        // Lifetime starts equal to today's solar so the IR 11-12 "total"
        // assertion below remains consistent with the daily-bucket projection
        // it replaced.
        state.energy_totals.solar_lifetime_kwh = 12.5;
        state.energy_totals.inverter_output_kwh = 12.5;
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
    fn ir_11_12_pv_total_reads_lifetime_not_daily() {
        // The GE "PV total lifetime" pair (IR 11 high / IR 12 low) must read
        // from `solar_lifetime_kwh` (cumulative, never reset at midnight), not
        // from the daily `solar_generation_kwh` bucket. After a day rollover
        // the daily bucket is zeroed but the lifetime bucket keeps climbing.
        let mut state = PlantState::new(test_ts());
        state.energy_totals.solar_generation_kwh = 0.0; // today is empty
        state.energy_totals.solar_lifetime_kwh = 12345.0; // baseline + prior days

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        let raw = (12345.0_f64 * 10.0).round() as u32; // ×0.1 kWh scaling
        assert_eq!(
            store.read_by_space(11, RegisterSpace::Input),
            Some((raw >> 16) as u16)
        );
        assert_eq!(
            store.read_by_space(12, RegisterSpace::Input),
            Some((raw & 0xFFFF) as u16)
        );
    }

    #[test]
    fn ir_1374_1375_tph_pv_total_reads_lifetime_not_daily() {
        // Same projection contract for the ThreePhase register pair: lifetime
        // bucket, not daily bucket.
        let mut state = PlantState::new(test_ts());
        state.config.inverter_type = "ThreePhase11kW".to_string();
        state.energy_totals.solar_generation_kwh = 0.0;
        state.energy_totals.solar_lifetime_kwh = 12345.0;

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        let raw = (12345.0_f64 * 10.0).round() as u32;
        assert_eq!(
            store.read_by_space(1374, RegisterSpace::Input),
            Some((raw >> 16) as u16)
        );
        assert_eq!(
            store.read_by_space(1375, RegisterSpace::Input),
            Some((raw & 0xFFFF) as u16)
        );
    }

    #[test]
    fn zero_energy_state_projects_zero_energy_registers_for_all_inverter_types() {
        // Energy "today" registers must be a faithful power-integral: a plant
        // that has not generated/imported/exported/charged/discharged anything
        // must read exactly zero on every daily energy register. The register
        // projection must NOT inject a synthetic non-zero fixture, otherwise
        // clients see daily energy (e.g. PV energy today) jump to a fixed
        // value the instant they poll instead of climbing smoothly with power.
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

            assert_eq!(
                store.read_by_space(17, RegisterSpace::Input).unwrap_or(0),
                0,
                "{inv_type}: PV today should be zero when no energy has been generated"
            );
            assert_eq!(
                store.read_by_space(25, RegisterSpace::Input).unwrap_or(0),
                0,
                "{inv_type}: grid export today should be zero"
            );
            assert_eq!(
                store.read_by_space(26, RegisterSpace::Input).unwrap_or(0),
                0,
                "{inv_type}: grid import today should be zero"
            );
            assert_eq!(
                store.read_by_space(36, RegisterSpace::Input).unwrap_or(0),
                0,
                "{inv_type}: battery charge today should be zero"
            );
            assert_eq!(
                store.read_by_space(37, RegisterSpace::Input).unwrap_or(0),
                0,
                "{inv_type}: battery discharge today should be zero"
            );
            assert_eq!(
                store.read_by_space(44, RegisterSpace::Input).unwrap_or(0),
                0,
                "{inv_type}: PV generation today should be zero"
            );

            if inv_type.starts_with("ThreePhase") || inv_type == "ACThreePhase" {
                assert_eq!(
                    read_u32_ir(&store, 1366, 1367),
                    0,
                    "{inv_type}: 3-phase PV1 today should be zero"
                );
                assert_eq!(
                    read_u32_ir(&store, 1380, 1381),
                    0,
                    "{inv_type}: 3-phase grid import today should be zero"
                );
                assert_eq!(
                    read_u32_ir(&store, 1384, 1385),
                    0,
                    "{inv_type}: 3-phase grid export today should be zero"
                );
                assert_eq!(
                    read_u32_ir(&store, 1388, 1389),
                    0,
                    "{inv_type}: 3-phase battery discharge today should be zero"
                );
                assert_eq!(
                    read_u32_ir(&store, 1392, 1393),
                    0,
                    "{inv_type}: 3-phase battery charge today should be zero"
                );
                assert_eq!(
                    read_u32_ir(&store, 1396, 1397),
                    0,
                    "{inv_type}: 3-phase load today should be zero"
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
        assert_eq!(store.read_by_space(1109, RegisterSpace::Holding), Some(4));
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
        // IR 180-181 (alt1 lifetime battery energy totals) project from the
        // seeded per-module `throughput_kwh` (3 yr × 330 cycles/yr × 9.5 kWh
        // = 9405 kWh, halved per the alt1 split). The seed happens in
        // `PlantState::new` before this test mutates state, so we assert the
        // seeded lifetime value rather than the daily kWh bucket.
        let seeded_throughput_kwh = state.batteries[0].throughput_kwh;
        let alt1_raw = (seeded_throughput_kwh / 2.0 * 10.0).round() as u16;
        assert_eq!(
            store.read_by_space(180, RegisterSpace::Input),
            Some(alt1_raw)
        );
        assert_eq!(
            store.read_by_space(181, RegisterSpace::Input),
            Some(alt1_raw)
        );
        // IR 182-183 still mirror the daily energy totals (alt2 split).
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
        // HR 4109-4112 (alt2 lifetime battery energy totals) project from the
        // seeded per-module `throughput_kwh` (3 yr × 330 cycles/yr × 9.5 kWh
        // = 9405 kWh, halved per the alt2 split). The seed happens in
        // `PlantState::new` before this test mutates state, so we assert the
        // seeded lifetime value rather than the daily kWh bucket.
        let seeded_throughput_kwh = state.batteries[0].throughput_kwh;
        let alt2_raw = (seeded_throughput_kwh / 2.0 * 10.0).round() as u16;
        assert_eq!(store.read_by_space(4109, RegisterSpace::Holding), Some(0));
        assert_eq!(
            store.read_by_space(4110, RegisterSpace::Holding),
            Some(alt2_raw)
        );
        assert_eq!(store.read_by_space(4111, RegisterSpace::Holding), Some(0));
        assert_eq!(
            store.read_by_space(4112, RegisterSpace::Holding),
            Some(alt2_raw)
        );
        // HR 4113-4114 (alt3 daily) still mirror the daily kWh buckets.
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
        assert_eq!(result[31], 0x0E10, "IR 91 status_3/status_4 default");
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

        // Three-phase uses Gen3-extended mapping: slot 2 at HR 243-244, NOT HR 31-32.
        // HR 31-32 are NOT written by project_schedule_for for Gen3-extended inverters;
        // they retain whatever project_from_state set (disabled sentinel 60) or 0 if
        // project_from_state hasn't run.
        assert_eq!(store.read_by_space(243, RegisterSpace::Holding), Some(1000));
        assert_eq!(store.read_by_space(244, RegisterSpace::Holding), Some(1200));
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
        // BatteryEngine would compute 77.4V for ThreePhase 24S at 62% SOC
        s.batteries[0].voltage_v = 77.4;
        s.sync_battery_from_vec();
        s.inverter.dsp_firmware_version = 612;
        s.inverter.arm_firmware_version = 318;
        s.energy_totals.solar_generation_kwh = 12.0;
        // Lifetime bucket starts equal to today's solar so the "total" assertion
        // below remains consistent with the previous (daily-bucket) projection.
        s.energy_totals.solar_lifetime_kwh = 12.0;
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
        // Three-phase reports line-to-line voltage (~416V) at IR 1061-1063.
        assert_eq!(store.read_by_space(1061, RegisterSpace::Input), Some(4160));
        assert_eq!(store.read_by_space(1062, RegisterSpace::Input), Some(4160));
        assert_eq!(store.read_by_space(1063, RegisterSpace::Input), Some(4160));
        // Per-phase current now uses grid power for three-phase: |grid|/3/240 = 2.5A → raw 25
        assert_eq!(store.read_by_space(1064, RegisterSpace::Input), Some(25));
        // Per-phase house load: demand_w/3 = 900W
        assert_eq!(store.read_by_space(1083, RegisterSpace::Input), Some(9000));
        assert_eq!(read_u32_ir(&store, 1089, 1090), 27000);
        assert_eq!(read_u32_ir(&store, 1079, 1080), 18000); // CT/meter import — real-time 1800W
        assert_eq!(read_u32_ir(&store, 1081, 1082), 0); // CT/meter export — 0 (importing)
        assert_eq!(read_u32_ir(&store, 1240, 1241), 0); // export mirror — 0 (importing)
        assert_eq!(read_u32_ir(&store, 1244, 1245), 0); // second meter absent

        // Battery block: temperatures, voltages, SoC, charge/discharge split, current and firmware IDs.
        assert_eq!(store.read_by_space(1128, RegisterSpace::Input), Some(350)); // t_inverter 35.0°C
        assert_eq!(store.read_by_space(1129, RegisterSpace::Input), Some(330)); // t_boost 33.0°C
        assert_eq!(store.read_by_space(1130, RegisterSpace::Input), Some(380)); // t_buck_boost 38.0°C
        assert_eq!(store.read_by_space(1131, RegisterSpace::Input), Some(774)); // v_battery_bms 77.4V
        assert_eq!(store.read_by_space(1132, RegisterSpace::Input), Some(62));
        assert_eq!(read_u32_ir(&store, 1138, 1139), 15000); // charging 1.5kW at ×0.1W
        assert_eq!(read_u32_ir(&store, 1136, 1137), 0);
        assert!((store.read_by_space(1140, RegisterSpace::Input).unwrap() as i16) < 0);
        assert_eq!(store.read_by_space(1133, RegisterSpace::Input), Some(774)); // v_battery_pcs 77.4V
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
        // Lifetime counterparts: e_battery_*_total projects from the seeded
        // per-module `throughput_kwh` (3 yr × 330 cycles/yr × 9.5 kWh = 9405
        // kWh, halved per the alt1 split). The seed happens in
        // `PlantState::new` before this test mutates state.
        let seeded_throughput_kwh = s.batteries[0].throughput_kwh;
        let alt1_raw = ((seeded_throughput_kwh / 2.0) * 10.0).round() as u32;
        assert_eq!(read_u32_ir(&store, 1390, 1391), alt1_raw); // e_battery_discharge_total
        assert_eq!(read_u32_ir(&store, 1394, 1395), alt1_raw); // e_battery_charge_total
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
    fn threephase_11kw_ct_meter_power_registers_follow_grid() {
        // CT meter import/export power follows real-time grid.power_w.
        // Default state has grid.power_w = 0, so both import and export are 0.
        let s = three_phase_11kw_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        assert_eq!(read_u32_ir(&store, 1079, 1080), 0); // import: 0W (no grid flow)
        assert_eq!(read_u32_ir(&store, 1081, 1082), 0); // export: 0W (no grid flow)
        assert_eq!(read_u32_ir(&store, 1240, 1241), 0); // export mirror: 0W
    }

    #[test]
    fn ct_meter_input_registers_project_correctly() {
        // Verify the exact data a client would see when reading IR 60-89
        // on slave 0x01 (CT meter). The client decodes relative to base 60.
        let mut s = PlantState::new(test_ts());
        s.grid.power_w = 2400.0; // 2.4 kW import
        s.config.inverter_type = "Gen3Hybrid".to_string();
        s.config.ct_meter_installed = true;
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let space = RegisterSpace::Input;

        // IR 60: v_phase_1 = 240.0V → raw 2400 (×0.1)
        assert_eq!(store.read_by_space(60, space), Some(2400));
        // IR 63: i_phase_1 = 2400/240 = 10A → raw 1000 (×0.01)
        assert_eq!(store.read_by_space(63, space), Some(1000));
        // IR 67: i_total = same as i_phase_1 for single-phase
        assert_eq!(store.read_by_space(67, space), Some(1000));
        // IR 68: p_active_phase_1 = 2400 (signed W)
        assert_eq!(store.read_by_space(68, space), Some(2400));
        // IR 71: p_active_total = 2400 (signed W)
        assert_eq!(store.read_by_space(71, space), Some(2400));
        // IR 76: p_apparent_phase_1 = 2400 (VA)
        assert_eq!(store.read_by_space(76, space), Some(2400));
        // IR 79: p_apparent_total = 2400 (VA)
        assert_eq!(store.read_by_space(79, space), Some(2400));
        // IR 80: pf_phase_1 = 1000 (×0.001 = 1.000)
        assert_eq!(store.read_by_space(80, space), Some(1000));
        // IR 83: pf_total = 1000 (×0.001 = 1.000)
        assert_eq!(store.read_by_space(83, space), Some(1000));
        // IR 84: frequency = 5000 (×0.01 = 50.00 Hz)
        assert_eq!(store.read_by_space(84, space), Some(5000));
    }

    #[test]
    fn ct_meter_data_zero_when_grid_power_zero() {
        // When grid power is 0, current and power should be 0.
        // Power factor should also be 0 (below threshold).
        let mut s = PlantState::new(test_ts());
        s.grid.power_w = 0.0;
        s.config.ct_meter_installed = true;
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let space = RegisterSpace::Input;
        // IR 67: i_total = 0
        assert_eq!(store.read_by_space(67, space), Some(0));
        // IR 79: p_apparent_total = 0
        assert_eq!(store.read_by_space(79, space), Some(0));
        // IR 83: pf_total = 0 (power < 1W → PF = 0)
        assert_eq!(store.read_by_space(83, space), Some(0));
    }

    // ===================================================================
    // Gen3 Charge Slot Register Map Tests
    // ===================================================================

    #[test]
    fn gen3_hybrid_writes_charge_slot_2_to_extended_holding_registers() {
        // Gen3Hybrid (ARM FW century 3) MUST use HR 243-244 for charge slot 2.
        // HR 31-32 should NOT be written by project_schedule_for.
        let mut store = RegisterStore::new(default_register_catalogue());
        let sched = sim_models::Schedule {
            charge_start: 1.0,
            charge_end: 5.0,
            charge_start_2: 10.0,
            charge_end_2: 14.0,
            charge_target_soc: 80.0,
            enable_charge: true,
            ..Default::default()
        };
        store.project_schedule_for(&sched, "Gen3Hybrid");

        // Slot 1 at HR 94-95 (always)
        assert_eq!(store.read_by_space(94, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(95, RegisterSpace::Holding), Some(500));

        // Slot 2 at HR 243-244 (Gen3 extended)
        assert_eq!(store.read_by_space(243, RegisterSpace::Holding), Some(1000));
        assert_eq!(store.read_by_space(244, RegisterSpace::Holding), Some(1400));

        // HR 31-32 NOT written by Gen3 — stays at 0 (no project_from_state called)
        assert_eq!(store.read_by_space(31, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(32, RegisterSpace::Holding), Some(0));
    }

    #[test]
    fn gen3_hybrid_with_state_projection_leaves_hr_31_32_at_disabled_sentinel() {
        // When project_from_state runs first (setting HR 31-32 to 60),
        // project_schedule_for should NOT overwrite them for Gen3.
        let mut s = PlantState::new(test_ts());
        s.config.inverter_type = "Gen3Hybrid".to_string();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let sched = sim_models::Schedule {
            charge_start_2: 10.0,
            charge_end_2: 14.0,
            ..Default::default()
        };
        store.project_schedule_for(&sched, "Gen3Hybrid");

        // HR 31-32 stay at disabled sentinel from project_from_state
        assert_eq!(store.read_by_space(31, RegisterSpace::Holding), Some(60));
        assert_eq!(store.read_by_space(32, RegisterSpace::Holding), Some(60));
        // Slot 2 correctly at HR 243-244
        assert_eq!(store.read_by_space(243, RegisterSpace::Holding), Some(1000));
        assert_eq!(store.read_by_space(244, RegisterSpace::Holding), Some(1400));
    }

    #[test]
    fn gen1_hybrid_writes_charge_slot_2_to_classic_holding_registers() {
        // Gen1Hybrid (ARM FW century 2) uses classic HR 31-32 for charge slot 2.
        // HR 243-244 should NOT be written.
        let mut store = RegisterStore::new(default_register_catalogue());
        let sched = sim_models::Schedule {
            charge_start: 1.0,
            charge_end: 5.0,
            charge_start_2: 10.0,
            charge_end_2: 14.0,
            enable_charge: true,
            ..Default::default()
        };
        store.project_schedule_for(&sched, "Gen1Hybrid");

        // Slot 1 at HR 94-95 (always)
        assert_eq!(store.read_by_space(94, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(95, RegisterSpace::Holding), Some(500));

        // Slot 2 at HR 31-32 (classic Gen1)
        assert_eq!(store.read_by_space(31, RegisterSpace::Holding), Some(1000));
        assert_eq!(store.read_by_space(32, RegisterSpace::Holding), Some(1400));

        // HR 243-244 NOT written by Gen1 — stays at 0
        assert_eq!(store.read_by_space(243, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(244, RegisterSpace::Holding), Some(0));
    }

    #[test]
    fn gen2_hybrid_has_no_charge_slot_2() {
        // Gen2Hybrid only supports 1 charge slot (HR 94-95).
        // Neither HR 31-32 nor HR 243-244 should be written for slot 2.
        let mut store = RegisterStore::new(default_register_catalogue());
        let sched = sim_models::Schedule {
            charge_start: 1.0,
            charge_end: 5.0,
            charge_start_2: 22.0,
            charge_end_2: 6.0,
            ..Default::default()
        };
        store.project_schedule_for(&sched, "Gen2Hybrid");

        // Slot 1 works normally
        assert_eq!(store.read_by_space(94, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(95, RegisterSpace::Holding), Some(500));
        // No slot 2 at any address
        assert_eq!(store.read_by_space(31, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(32, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(243, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(244, RegisterSpace::Holding), Some(0));
    }

    #[test]
    fn gen2_hybrid_has_no_discharge_slot_2_or_extended_slots() {
        // Gen2Hybrid only supports 1 discharge slot (HR 56-57).
        // Discharge slot 2 (HR 44-45) and slots 3-10 should be disabled (60).
        let mut store = RegisterStore::new(default_register_catalogue());
        let sched = sim_models::Schedule {
            discharge_start: 17.0,
            discharge_end: 21.0,
            discharge_start_2: 10.0,
            discharge_end_2: 14.0,
            discharge_start_3: 8.0,
            discharge_end_3: 9.0,
            ..Default::default()
        };
        store.project_schedule_for(&sched, "Gen2Hybrid");

        // Slot 1 works normally
        assert_eq!(store.read_by_space(56, RegisterSpace::Holding), Some(1700));
        assert_eq!(store.read_by_space(57, RegisterSpace::Holding), Some(2100));
        // Slot 2 disabled
        assert_eq!(store.read_by_space(44, RegisterSpace::Holding), Some(60));
        assert_eq!(store.read_by_space(45, RegisterSpace::Holding), Some(60));
        // Extended slots also disabled
        assert_eq!(store.read_by_space(276, RegisterSpace::Holding), Some(60));
        assert_eq!(store.read_by_space(277, RegisterSpace::Holding), Some(60));
    }

    #[test]
    fn ac_coupled_has_basic_charge_and_discharge_slot_1_only() {
        // AC-coupled inverters use the basic single-phase slot map:
        // charge slot 1 at HR 94-95 and discharge slot 1 at HR 56-57.
        // Slot 2 / extended slots are not supported.
        let mut store = RegisterStore::new(default_register_catalogue());
        let sched = sim_models::Schedule {
            charge_start: 1.0,
            charge_end: 5.0,
            charge_start_2: 10.0,
            charge_end_2: 14.0,
            discharge_start: 17.0,
            discharge_end: 21.0,
            discharge_start_2: 10.0,
            discharge_end_2: 14.0,
            discharge_target_soc: 25.0,
            discharge_target_soc_2: 30.0,
            ..Default::default()
        };
        store.project_schedule_for(&sched, "ACCoupled");

        assert_eq!(store.read_by_space(94, RegisterSpace::Holding), Some(100));
        assert_eq!(store.read_by_space(95, RegisterSpace::Holding), Some(500));
        assert_eq!(store.read_by_space(56, RegisterSpace::Holding), Some(1700));
        assert_eq!(store.read_by_space(57, RegisterSpace::Holding), Some(2100));
        assert_eq!(store.read_by_space(59, RegisterSpace::Holding), Some(1));
        assert_eq!(store.read_by_space(272, RegisterSpace::Holding), Some(25));
        // No slot 2 at any address
        assert_eq!(store.read_by_space(31, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(32, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(243, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(244, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(44, RegisterSpace::Holding), Some(60));
        assert_eq!(store.read_by_space(45, RegisterSpace::Holding), Some(60));
        assert_eq!(store.read_by_space(275, RegisterSpace::Holding), Some(0));
        // Extended discharge slots disabled
        assert_eq!(store.read_by_space(276, RegisterSpace::Holding), Some(60));
        assert_eq!(store.read_by_space(277, RegisterSpace::Holding), Some(60));
    }

    #[test]
    fn all_in_one_uses_gen3_extended_charge_slot_2() {
        // AllInOne, AIO, AIOHybrid models all use Gen3-extended mapping.
        for inv_type in &[
            "AllInOne",
            "AllInOne5",
            "AllInOne6",
            "AIO6kW",
            "AIO8kW",
            "AIO10kW",
            "AIOHybrid6kW",
            "AIOHybrid8kW",
            "AIOHybrid10kW",
        ] {
            let mut store = RegisterStore::new(default_register_catalogue());
            let sched = sim_models::Schedule {
                charge_start_2: 11.0,
                charge_end_2: 13.0,
                ..Default::default()
            };
            store.project_schedule_for(&sched, inv_type);

            assert_eq!(
                store.read_by_space(243, RegisterSpace::Holding),
                Some(1100),
                "Slot 2 start should be at HR 243 for {inv_type}"
            );
            assert_eq!(
                store.read_by_space(244, RegisterSpace::Holding),
                Some(1300),
                "Slot 2 end should be at HR 244 for {inv_type}"
            );
        }
    }

    #[test]
    fn gen3_extended_models_skip_hr_31_32() {
        // Verify that all Gen3-era models leave HR 31-32 untouched.
        for inv_type in &[
            "Gen3Hybrid",
            "Gen3Hybrid8kW",
            "Gen3Hybrid10kW",
            "Gen3Plus6kW",
            "Gen3Plus8kW",
            "ThreePhase",
            "ThreePhase8kW",
            "ThreePhase10kW",
            "ThreePhase11kW",
            "AllInOne",
            "AIO8kW",
            "AIOHybrid8kW",
            "Polar5kW",
            "Polar8kW",
            "EMS",
            "Gateway12kW",
        ] {
            let mut store = RegisterStore::new(default_register_catalogue());
            let sched = sim_models::Schedule {
                charge_start_2: 10.0,
                charge_end_2: 14.0,
                ..Default::default()
            };
            store.project_schedule_for(&sched, inv_type);

            assert_eq!(
                store.read_by_space(31, RegisterSpace::Holding),
                Some(0),
                "HR 31 should NOT be written for {inv_type}"
            );
            assert_eq!(
                store.read_by_space(32, RegisterSpace::Holding),
                Some(0),
                "HR 32 should NOT be written for {inv_type}"
            );
            assert_eq!(
                store.read_by_space(243, RegisterSpace::Holding),
                Some(1000),
                "HR 243 should have slot 2 start for {inv_type}"
            );
            assert_eq!(
                store.read_by_space(244, RegisterSpace::Holding),
                Some(1400),
                "HR 244 should have slot 2 end for {inv_type}"
            );
        }
    }

    #[test]
    fn meter_registers_nonzero_after_realistic_tick() {
        let mut s = PlantState::new(test_ts());
        s.solar.generation_w = 4000.0;
        s.solar.pv1_w = 4000.0;
        s.load.demand_w = 2000.0;
        s.grid.power_w = -2000.0;
        s.batteries[0].soc_percent = 50.0;
        s.sync_battery_from_vec();
        s.config.inverter_type = "ACCoupled".to_string();
        s.config.ct_meter_installed = true;

        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let space = RegisterSpace::Input;
        let i_total = store.read_by_space(67, space);
        let p_apparent = store.read_by_space(79, space);
        let pf = store.read_by_space(83, space);

        assert_ne!(i_total, Some(0), "IR 67 i_total should be non-zero");
        assert_ne!(
            p_apparent,
            Some(0),
            "IR 79 p_apparent_total should be non-zero"
        );
        assert_ne!(pf, Some(0), "IR 83 pf_total should be non-zero");
        assert_eq!(i_total, Some(833), "IR 67: expected 833 (8.33A)");
        assert_eq!(p_apparent, Some(2000), "IR 79: expected 2000 VA");
        assert_eq!(pf, Some(1000), "IR 83: expected 1000 (1.000)");
    }

    // ==================================================================
    // Gateway aggregation bank (IR 1600-1859) tests
    // See docs/gateway-register-reference.md for the authoritative map.
    // ==================================================================

    fn gateway_state() -> PlantState {
        let mut s = PlantState::new(test_ts());
        s.config.inverter_type = "Gateway12kW".to_string();
        s.solar.generation_w = 3000.0;
        s.solar.pv1_w = 3000.0;
        s.load.demand_w = 800.0;
        s.grid.power_w = 500.0; // importing
        s.grid.connected = true;
        s.batteries[0].soc_percent = 65.0;
        s.batteries[0].power_kw = 2.0; // charging (+ = charging internal)
        s.sync_battery_from_vec();
        s.energy_totals.grid_import_kwh = 100.0;
        s.energy_totals.solar_generation_kwh = 200.0;
        s.energy_totals.battery_charge_kwh = 50.0;
        s.energy_totals.battery_discharge_kwh = 30.0;
        s.energy_totals.load_consumption_kwh = 80.0;
        s.energy_totals.grid_export_kwh = 10.0;
        s
    }

    #[test]
    fn gateway_bank_populated_for_gateway_inverter() {
        let s = gateway_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);

        // Version string decodes to non-empty → validity check passes.
        assert_eq!(ir(1600), (b'G' as u16) << 8 | b'A' as u16);
        // IR(1603) = 9 < 10 selects firmware variant V1.
        assert_eq!(ir(1603), 9, "IR(1603) must select V1 (< 10)");
        // Work mode = On Grid (2)
        assert_eq!(ir(1604), 2);
        // Single-AIO summary
        assert_eq!(ir(1700), 1, "parallel_aio_num = 1");
        assert_eq!(ir(1701), 1, "parallel_aio_online_num = 1");
        // PV power
        assert_eq!(ir(1617), 3000, "p_pv = solar generation");
        // Load excludes EV charger
        assert_eq!(ir(1618), 800, "p_load = household demand");
        // SOC projected onto AIO1
        assert_eq!(ir(1801), 65, "aio1_soc = aggregate SOC");
        assert_eq!(ir(1802), 0, "aio2_soc empty for single-AIO");
    }

    #[test]
    fn gateway_bank_zero_for_non_gateway_inverter() {
        // Non-gateway inverters must leave the bank at seeded zeros so the
        // version-string validity check fails and clients conclude "no gateway".
        let mut s = PlantState::new(test_ts());
        s.config.inverter_type = "Gen3Hybrid".to_string();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        assert_eq!(
            store.read_by_space(1600, RegisterSpace::Input),
            Some(0),
            "non-gateway must not populate gateway version"
        );
        assert_eq!(
            store.read_by_space(1603, RegisterSpace::Input),
            Some(0),
            "IR(1603) zero → variant selector sees no gateway"
        );
        assert_eq!(
            store.read_by_space(1700, RegisterSpace::Input),
            Some(0),
            "parallel_aio_num zero for non-gateway"
        );
    }

    #[test]
    fn gateway_serial_has_gw_prefix() {
        // Prefix-based detection keys off the GW serial on HR 13-17.
        let s = gateway_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let r0 = store.read_by_space(13, RegisterSpace::Holding).unwrap();
        let r1 = store.read_by_space(14, RegisterSpace::Holding).unwrap();
        let hi = (r0 >> 8) as u8;
        let lo = (r0 & 0xFF) as u8;
        assert_eq!(hi, b'G', "serial byte 0 must be 'G'");
        assert_eq!(lo, b'W', "serial byte 1 must be 'W'");
        // Full serial = "GW2423G192"
        let mut chars = Vec::new();
        for addr in 13..=17 {
            let v = store.read_by_space(addr, RegisterSpace::Holding).unwrap();
            chars.push((v >> 8) as u8);
            chars.push((v & 0xFF) as u8);
        }
        assert_eq!(&chars[..], b"GW2423G192");
        let _ = r1;
    }

    #[test]
    fn gateway_aio_power_sign_convention() {
        // GivEnergy Gateway wire convention for p_ac1 (1616), p_aio_total
        // (1702) and per-AIO p_aioN_inverter (1816–1818) is the OPPOSITE of
        // the standard inverter p_battery (IR 52): raw + = charging/in,
        // − = discharging/out. Confirmed by GivTCP read.py:1556 (negates
        // p_aio_total to recover Battery_Power with + = discharge internally).
        // aio_state (1703) independently corroborates: 1 = charging,
        // 2 = discharging, 0 = idle — a +raw value pairs with aio_state=1.
        let s = gateway_state(); // battery charging at 2kW → wire value POSITIVE
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0) as i16;
        assert!(
            ir(1616) > 0,
            "p_ac1 must be positive while charging (Gateway convention), got {}",
            ir(1616)
        );
        assert_eq!(ir(1616), ir(1702), "p_ac1 == p_aio_total for single-AIO");
        assert_eq!(
            ir(1702),
            ir(1816),
            "p_aio_total == p_aio1_inverter for single-AIO"
        );
        // aio_state = 1 (charging)
        assert_eq!(store.read_by_space(1703, RegisterSpace::Input), Some(1));
    }

    #[test]
    fn gateway_aio_power_discharge_positive() {
        let mut s = gateway_state();
        s.batteries[0].power_kw = -1.5; // discharging
        // total_battery_power_kw() sums batteries[], so no re-sync needed;
        // sync_vec_from_battery would overwrite this with the battery field.
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0) as i16;
        assert!(
            ir(1816) < 0,
            "p_aio1_inverter must be negative while discharging (Gateway convention), got {}",
            ir(1816)
        );
        assert_eq!(store.read_by_space(1703, RegisterSpace::Input), Some(2));
    }

    #[test]
    fn gateway_energy_totals_v1_byte_order() {
        // V1: uint32 energy totals are high-register-first.
        // grid_import_kwh = 100 → raw = 100/0.1 = 1000 = 0x03E8
        // hi = 0, lo = 1000.
        let s = gateway_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        assert_eq!(ir(1640), 1000, "e_grid_import_today = 100kWh * 10");
        // e_grid_import_total = (hi=0, lo=1000)
        assert_eq!(ir(1641), 0, "V1 high word first");
        assert_eq!(ir(1642), 1000, "V1 low word second");
        // AIO1 charge mirrors the aggregate charge total
        assert_eq!(ir(1705), 500, "e_aio1_charge_today = 50kWh * 10");
        assert_eq!(ir(1706), 0);
        assert_eq!(ir(1707), 500);
    }

    #[test]
    fn gateway_aio_serials_populated() {
        // AIO1 serial at IR 1831-1835 must decode to a non-blank string so the
        // client's serial validity check passes. AIO2/AIO3 stay empty (zeros).
        let s = gateway_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        // Full 10-byte serial must round-trip exactly (guards against
        // byte-pairing bugs that corrupt every register after the first).
        let mut chars = Vec::new();
        for addr in [1831, 1832, 1833, 1834, 1835] {
            let v = ir(addr);
            chars.push((v >> 8) as u8);
            chars.push((v & 0xFF) as u8);
        }
        assert_eq!(&chars[..], b"SA24230001", "aio1 serial must round-trip");
        // first_inverter_serial (1627-1631) mirrors aio1
        let mut first = Vec::new();
        for addr in [1627, 1628, 1629, 1630, 1631] {
            let v = ir(addr);
            first.push((v >> 8) as u8);
            first.push((v & 0xFF) as u8);
        }
        assert_eq!(
            &first[..],
            b"SA24230001",
            "first_inverter_serial must round-trip"
        );
        assert_eq!(ir(1838), 0, "aio2 serial empty for single-AIO");
        assert_eq!(ir(1845), 0, "aio3 serial empty for single-AIO");
    }

    #[test]
    fn gateway_multi_aio_populates_all_registers() {
        // With parallel_aio_num = 3, all AIO2/AIO3 registers must carry
        // data (evenly split from the single battery stack).
        let mut s = gateway_state();
        s.config.parallel_aio_num = 3;
        // 3 batteries → 1 per AIO
        let b2 = s.batteries[0].clone();
        let b3 = s.batteries[0].clone();
        s.batteries.push(b2);
        s.batteries.push(b3);
        s.sync_battery_from_vec();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        let i16 = |a: u16| ir(a) as i16;

        // AIO summary
        assert_eq!(ir(1700), 3, "parallel_aio_num = 3");
        assert_eq!(ir(1701), 3, "parallel_aio_online_num = 3");

        // Each AIO gets 1/3 of total battery power.
        // 3 modules × 2kW charging each = 6kW total. Gateway wire convention
        // (opposite of standard IR 52 p_battery): raw + = charging, so each
        // AIO emits +6000/3 = +2000W on the wire (charging).
        for aio_i in 0..3 {
            let power_reg = 1816u16 + aio_i as u16;
            let p = i16(power_reg);
            assert_eq!(
                p,
                2000,
                "AIO{} inverter power must be +2000W (charging, Gateway convention), got {}",
                aio_i + 1,
                p
            );
        }

        // Each AIO gets 1/3 of SOC (same aggregate SoC for all)
        assert_eq!(ir(1801), 65, "aio1_soc");
        assert_eq!(ir(1802), 65, "aio2_soc shared");
        assert_eq!(ir(1803), 65, "aio3_soc shared");

        // Each AIO gets 1/3 of charge energy
        // Total charge = 50 kWh, each gets ~16.667 kWh → deci = 167
        assert_eq!(ir(1705), 167, "aio1_charge_today");
        assert_eq!(ir(1708), 167, "aio2_charge_today");
        assert_eq!(ir(1711), 167, "aio3_charge_today");

        // Each AIO gets 1/3 of discharge energy
        assert_eq!(ir(1750), 100, "aio1_discharge_today");
        assert_eq!(ir(1753), 100, "aio2_discharge_today");
        assert_eq!(ir(1756), 100, "aio3_discharge_today");

        // AIO serials (V1)
        let aio_serial = |base: u16| -> Vec<u8> {
            let mut chars = Vec::new();
            for addr in base..base + 5 {
                let v = ir(addr);
                chars.push((v >> 8) as u8);
                chars.push((v & 0xFF) as u8);
            }
            chars
        };
        assert_eq!(&aio_serial(1831)[..], b"SA24230001", "aio1 serial");
        assert_eq!(&aio_serial(1838)[..], b"SA24230002", "aio2 serial");
        assert_eq!(&aio_serial(1845)[..], b"SA24230003", "aio3 serial");

        // first_inverter_serial mirrors AIO1
        let mut first = Vec::new();
        for addr in [1627, 1628, 1629, 1630, 1631] {
            let v = ir(addr);
            first.push((v >> 8) as u8);
            first.push((v & 0xFF) as u8);
        }
        assert_eq!(&first[..], b"SA24230001");
    }

    #[test]
    fn gateway_identity_block_shows_zero_batteries() {
        // A real Gateway has zero directly-attached batteries (batteries live
        // in the child AIO). The identity block (IR/HR 0-119) must reflect this:
        // battery SOC/voltage/current/power/temp/serial/BMS-fw all zero.
        let s = gateway_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        let hr = |a: u16| store.read_by_space(a, RegisterSpace::Holding).unwrap_or(0);

        // Identity block battery registers must be zero:
        assert_eq!(ir(50), 0, "gateway battery voltage must be 0");
        assert_eq!(ir(51), 0, "gateway battery current must be 0");
        assert_eq!(ir(52), 0, "gateway battery power must be 0");
        assert_eq!(ir(56), 0, "gateway battery temp must be 0");
        assert_eq!(ir(59), 0, "gateway battery SOC must be 0");
        // Battery serial (HR 8-12) must be zeros
        for a in 8..=12 {
            assert_eq!(hr(a), 0, "gateway battery serial HR({a}) must be 0");
        }
        // Battery BMS firmware (HR 18) must be 0
        assert_eq!(hr(18), 0, "gateway battery BMS fw must be 0");
        // Battery module count (HR 214) must be 0
        assert_eq!(
            store.read(214).unwrap_or(0),
            0,
            "gateway battery_module_count must be 0"
        );

        // Meanwhile the aggregation bank still carries the AIO's battery data
        assert_eq!(ir(1801), 65, "aio1_soc must still be in aggregation bank");
    }

    #[test]
    fn non_gateway_identity_block_still_has_battery_data() {
        // Non-gateway inverters must NOT be affected by the gateway battery
        // zeroing — their identity block must still carry battery data.
        let mut s = PlantState::new(test_ts());
        s.config.inverter_type = "Gen3Hybrid".to_string();
        s.batteries[0].soc_percent = 65.0;
        s.batteries[0].power_kw = 1.0;
        s.sync_battery_from_vec();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        let hr = |a: u16| store.read_by_space(a, RegisterSpace::Holding).unwrap_or(0);

        // SOC should be 65% (not zeroed)
        assert_eq!(ir(59), 65, "non-gateway battery SOC must be preserved");
        // Battery serial must still be non-zero
        assert_ne!(hr(8), 0, "non-gateway battery serial must be non-zero");
        // BMS firmware must still be 100
        assert_eq!(hr(18), 100, "non-gateway battery BMS fw must be preserved");
        // Module count must be 1
        assert_eq!(
            store.read(214).unwrap_or(0),
            1,
            "non-gateway battery_module_count must be 1"
        );
    }

    #[test]
    fn gateway_control_writes_accepted() {
        // Writes to gateway control registers must pass access control
        // (same as any other inverter — the PlantState IS the child AIO).
        let s = gateway_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        // Key control registers all ReadWrite — writes must succeed.
        assert!(
            store.write(59, 1),
            "HR 59 enable_discharge must accept write"
        );
        assert!(
            store.write(94, 930),
            "HR 94 charge_slot_1_start must accept"
        );
        assert!(store.write(95, 1630), "HR 95 charge_slot_1_end must accept");
        assert!(store.write(96, 1), "HR 96 enable_charge must accept write");
        assert!(store.write(110, 15), "HR 110 soc_reserve must accept write");
        assert!(
            store.write(111, 95),
            "HR 111 charge_limit must accept write"
        );
        assert!(store.write(112, 80), "HR 112 discharge_limit must accept");
        assert!(
            store.write(116, 100),
            "HR 116 charge_target_soc must accept"
        );
        assert!(
            store.write(56, 800),
            "HR 56 discharge_slot_1_start must accept"
        );
        assert!(
            store.write(57, 2000),
            "HR 57 discharge_slot_1_end must accept"
        );

        // Verify values persisted
        assert_eq!(store.read(59), Some(1));
        assert_eq!(store.read(94), Some(930));
        assert_eq!(store.read(110), Some(15));
        assert_eq!(store.read(116), Some(100));
        assert_eq!(store.read(56), Some(800));
        assert_eq!(store.read(57), Some(2000));

        // System time registers (HR 35-40) must also accept writes
        assert!(store.write(35, 2025), "HR 35 year must accept write");
        assert!(store.write(36, 6), "HR 36 month must accept write");
        assert_eq!(store.read(35), Some(2025));
    }

    #[test]
    fn gateway_snapshot_includes_bank() {
        // snapshot() length must equal catalogue length — proves every gateway
        // value inserted by project_gateway_bank has a matching RegisterDef.
        let s = gateway_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(
            store.snapshot().len(),
            default_register_catalogue().len(),
            "every projected gateway value must have a catalogue def"
        );
    }

    /// Create a gateway state with V2 firmware (GA0000010).
    fn gateway_v2_state() -> PlantState {
        let mut s = gateway_state();
        s.config.gateway_fw_version = 10;
        s
    }

    #[test]
    fn gateway_v2_version_selector() {
        // V2: IR(1603) >= 10 selects V2 firmware variant.
        let s = gateway_v2_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        assert_eq!(ir(1600), (b'G' as u16) << 8 | b'A' as u16);
        assert_eq!(ir(1601), (b'0' as u16) << 8 | b'0' as u16);
        assert_eq!(ir(1602), 0x0000);
        assert_eq!(ir(1603), 10, "IR(1603) must be >= 10 for V2");
        // Work mode still On Grid
        assert_eq!(ir(1604), 2);
    }

    #[test]
    fn gateway_v2_energy_totals_swapped_byte_order() {
        // V2: uint32 energy totals are low-register-first (swapped vs V1).
        // grid_import_kwh = 100 → raw = 1000 = 0x03E8
        // V2: (lo=1000, hi=0) i.e. IR(1641)=1000, IR(1642)=0.
        let s = gateway_v2_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        // Daily counters are unchanged in V2
        assert_eq!(ir(1640), 1000, "e_grid_import_today = 100kWh * 10");
        // V2: lo word first, then hi word (SWAPPED vs V1)
        assert_eq!(ir(1641), 1000, "V2: e_grid_import_total_lo first");
        assert_eq!(ir(1642), 0, "V2: e_grid_import_total_hi second");
        // AIO1 charge total also swapped
        assert_eq!(ir(1705), 500, "e_aio1_charge_today unchanged");
        assert_eq!(ir(1706), 500, "V2: e_aio1_charge_total_lo first");
        assert_eq!(ir(1707), 0, "V2: e_aio1_charge_total_hi second");
        // Battery energy totals also swapped
        assert_eq!(ir(1796), 500, "V2: e_battery_charge_total_lo first");
        assert_eq!(ir(1797), 0, "V2: e_battery_charge_total_hi second");
    }

    #[test]
    fn gateway_v1_energy_totals_unchanged_by_v2_code() {
        // Regression: V1 byte order must still be hi-first (existing V1
        // energy test passes, but this explicitly checks swapped pairs).
        let s = gateway_state(); // V1 default (fw_version = 9)
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        // V1: hi word first, lo word second
        assert_eq!(ir(1641), 0, "V1: e_grid_import_total_hi first");
        assert_eq!(ir(1642), 1000, "V1: e_grid_import_total_lo second");
        // AIO1 charge total V1 order
        assert_eq!(ir(1706), 0, "V1: e_aio1_charge_total_hi first");
        assert_eq!(ir(1707), 500, "V1: e_aio1_charge_total_lo second");
        // Battery charge total V1 order
        assert_eq!(ir(1796), 0, "V1: e_battery_charge_total_hi first");
        assert_eq!(ir(1797), 500, "V1: e_battery_charge_total_lo second");
    }

    #[test]
    fn gateway_v2_serial_at_v2_addresses() {
        // V2: aio1 serial at IR 1841-1845 (not 1831-1835).
        let s = gateway_v2_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);

        let ir = |a: u16| store.read_by_space(a, RegisterSpace::Input).unwrap_or(0);
        // V2 aio1 serial at 1841-1845
        let mut chars = Vec::new();
        for addr in [1841, 1842, 1843, 1844, 1845] {
            let v = ir(addr);
            chars.push((v >> 8) as u8);
            chars.push((v & 0xFF) as u8);
        }
        assert_eq!(&chars[..], b"SA24230001", "V2 aio1 serial at 1841-1845");
        // V1 address 1831-1835 must be zero
        for addr in [1831, 1832, 1833, 1834, 1835] {
            assert_eq!(ir(addr), 0, "V2: V1 serial addr {addr} must be zero");
        }
        // first_inverter_serial (1627-1631) mirrors aio1 regardless of variant
        let mut first = Vec::new();
        for addr in [1627, 1628, 1629, 1630, 1631] {
            let v = ir(addr);
            first.push((v >> 8) as u8);
            first.push((v & 0xFF) as u8);
        }
        assert_eq!(
            &first[..],
            b"SA24230001",
            "first_inverter_serial must round-trip (V2)"
        );
    }

    #[test]
    fn gateway_v2_snapshot_includes_bank() {
        // Same snapshot length test for V2 variant.
        let s = gateway_v2_state();
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&s);
        assert_eq!(
            store.snapshot().len(),
            default_register_catalogue().len(),
            "every projected V2 gateway value must have a catalogue def"
        );
    }

    // ================================================================
    // Fault-bit correctness — verified against the GivEnergy reference
    // decoders in givenergy-modbus (model/inverter.py,
    // model/inverter_threephase.py) and giv_tcp (baseinverter.py,
    // threephase.py, register.py).
    // ================================================================

    /// Full single-phase `_inverter_fault_code` table. Index `i` corresponds to
    /// bit `31-i` of the uint32 (HR223 hi | HR224 lo), decoded MSB-first.
    fn decode_single_phase_faults(val: u32) -> Vec<&'static str> {
        const FAULTS: [Option<&str>; 32] = [
            None,
            None,
            None,
            Some("Backup Overload Fault"),
            None,
            None,
            Some("Grid Monitor Comm Fault"),
            Some("ARM Comms Fault"),
            Some("Consistent Fault"),
            Some("EEPROM Fault"),
            None,
            None,
            None,
            None,
            None,
            None,
            Some("Inverter Frequency Fault"),
            Some("Relay Fault"),
            Some("Inverter Voltage Fault"),
            Some("GFCI Fault"),
            Some("Hail Sensor Fault"),
            Some("DSP Comms Fault"),
            Some("Bus over voltage"),
            Some("Inverter Current Fault"),
            Some("No Utility"),
            Some("PV Isolation Fault"),
            Some("Current leak high"),
            Some("DCI high"),
            Some("PV Over voltage"),
            Some("Grid voltage Fault"),
            Some("Grid Frequency Fault"),
            Some("Inverter NTC Fault"),
        ];
        let mut out = Vec::new();
        for (i, name) in FAULTS.iter().enumerate() {
            if val & (1 << (31 - i)) != 0
                && let Some(n) = name
            {
                out.push(*n);
            }
        }
        out
    }

    fn projected_fault_word(state: &PlantState) -> u32 {
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(state);
        let hi = store.read_by_space(223, RegisterSpace::Holding).unwrap() as u32;
        let lo = store.read_by_space(224, RegisterSpace::Holding).unwrap() as u32;
        (hi << 16) | lo
    }

    #[test]
    fn gen3_grid_loss_decodes_to_no_utility() {
        // Default "Gen3" is a single-phase Gen3-style inverter. Grid loss MUST
        // land on bit 7 = "No Utility" of the HR(223-224) inverter fault word.
        let mut state = make_state();
        state.active_faults = vec!["grid_loss".into()];
        let word = projected_fault_word(&state);
        assert_eq!(
            decode_single_phase_faults(word),
            vec!["No Utility"],
            "grid_loss must decode to exactly [No Utility] (word={word:#010x})"
        );
        // Exact-bit check: bit 7 lives in the low word (HR224).
        assert_eq!(store_hr224(&state), 0x0080);
        assert_eq!(store_hr223(&state), 0);
    }

    #[test]
    fn gen3_inverter_trip_decodes_to_consistent_fault() {
        let mut state = make_state();
        state.active_faults = vec!["inverter_trip".into()];
        let word = projected_fault_word(&state);
        assert_eq!(
            decode_single_phase_faults(word),
            vec!["Consistent Fault"],
            "inverter_trip must decode to exactly [Consistent Fault] (word={word:#010x})"
        );
        // Bit 23 of the composite word lives in the high word (HR223) at bit 7.
        assert_eq!(store_hr223(&state), 0x0080);
        assert_eq!(store_hr224(&state), 0);
        // Auxiliary signal: IR 0 status reflects FAULT (3).
        assert_eq!(store_ir0(&state), 3);
    }

    #[test]
    fn gen3_battery_over_temp_decodes_to_inverter_ntc_fault() {
        // Single-phase inverters have no dedicated battery-over-temp bit on the
        // inverter fault word; the closest client-decodable thermal bit is
        // "Inverter NTC Fault" (idx 31 → bit 0).
        let mut state = make_state();
        state.active_faults = vec!["battery_over_temp".into()];
        let word = projected_fault_word(&state);
        assert_eq!(
            decode_single_phase_faults(word),
            vec!["Inverter NTC Fault"],
            "battery_over_temp must decode to [Inverter NTC Fault] (word={word:#010x})"
        );
        assert_eq!(store_hr224(&state), 0x0001);
        // Auxiliary signal: IR 57 charger_warning_code is non-zero.
        assert_eq!(store_ir57(&state), 1);
    }

    #[test]
    fn gen3_all_faults_combine_into_named_word() {
        let mut state = make_state();
        state.active_faults = vec![
            "grid_loss".into(),
            "inverter_trip".into(),
            "battery_over_temp".into(),
            "comm_timeout".into(),
        ];
        let word = projected_fault_word(&state);
        let mut names = decode_single_phase_faults(word);
        names.sort();
        let mut expected = vec![
            "No Utility",
            "Consistent Fault",
            "Inverter NTC Fault",
            "ARM Comms Fault",
        ];
        expected.sort();
        assert_eq!(names, expected, "combined faults (word={word:#010x})");
    }

    #[test]
    fn ir_39_40_mirrors_hr_223_224_for_single_phase() {
        let mut state = make_state();
        state.active_faults = vec!["grid_loss".into(), "inverter_trip".into()];
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        let ir39 = store.read_by_space(39, RegisterSpace::Input).unwrap() as u32;
        let ir40 = store.read_by_space(40, RegisterSpace::Input).unwrap() as u32;
        let ir_word = (ir39 << 16) | ir40;
        let hr_word = projected_fault_word(&state);
        assert_eq!(ir_word, hr_word, "IR 39-40 must mirror HR 223-224");
    }

    #[test]
    fn single_phase_leaves_threephase_fault_words_zero() {
        let mut state = make_state(); // Gen3 (single-phase)
        state.active_faults = vec!["grid_loss".into(), "battery_over_temp".into()];
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        for addr in 1300..=1307 {
            assert_eq!(
                store.read_by_space(addr, RegisterSpace::Input),
                Some(0),
                "single-phase must leave IR {addr} at zero"
            );
        }
    }

    #[test]
    fn three_phase_grid_loss_uses_ir_1301_no_grid_connection() {
        let mut state = make_state();
        state.config.inverter_type = "ThreePhase".into();
        state.active_faults = vec!["grid_loss".into()];
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        // word 1 (IR1301) idx 15 → bit 0 = "No Grid connection"
        assert_eq!(
            store.read_by_space(1301, RegisterSpace::Input),
            Some(0x0001)
        );
        // three-phase does not surface faults on HR 223-224
        assert_eq!(store.read_by_space(223, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(224, RegisterSpace::Holding), Some(0));
        assert_eq!(store.read_by_space(39, RegisterSpace::Input), Some(0));
    }

    #[test]
    fn three_phase_inverter_trip_uses_ir_1305_relay_fault() {
        let mut state = make_state();
        state.config.inverter_type = "ThreePhase10kW".into();
        state.active_faults = vec!["inverter_trip".into()];
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        // word 5 (IR1305) idx 11 → bit 4 = "Relay fault"
        assert_eq!(
            store.read_by_space(1305, RegisterSpace::Input),
            Some(0x0010)
        );
    }

    #[test]
    fn three_phase_battery_over_temp_uses_ir_1307() {
        // Three-phase has a REAL dedicated battery-over-temp bit, unlike
        // single-phase. word 7 (IR1307) idx 6 → bit 9.
        let mut state = make_state();
        state.config.inverter_type = "ACThreePhase".into();
        state.active_faults = vec!["battery_over_temp".into()];
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        assert_eq!(
            store.read_by_space(1307, RegisterSpace::Input),
            Some(0x0200)
        );
    }

    #[test]
    fn three_phase_combined_fault_words() {
        let mut state = make_state();
        state.config.inverter_type = "ThreePhase".into();
        state.active_faults = vec![
            "grid_loss".into(),
            "inverter_trip".into(),
            "battery_over_temp".into(),
            "comm_timeout".into(),
            "sensor_drift".into(),
        ];
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);
        // IR1301: grid_loss bit 0 ("No Grid connection") | comm_timeout bit 15 ("Gateway Comm fault")
        assert_eq!(
            store.read_by_space(1301, RegisterSpace::Input),
            Some(0x8001)
        );
        // IR1305: inverter_trip bit 4 ("Relay fault") | sensor_drift bit 13 ("NTC open")
        assert_eq!(
            store.read_by_space(1305, RegisterSpace::Input),
            Some(0x2010)
        );
        // IR1307: battery_over_temp bit 9 ("Battery over temperature")
        assert_eq!(
            store.read_by_space(1307, RegisterSpace::Input),
            Some(0x0200)
        );
        // All other words stay zero.
        for addr in [1300, 1302, 1303, 1304, 1306] {
            assert_eq!(store.read_by_space(addr, RegisterSpace::Input), Some(0));
        }
    }

    // -----------------------------------------------------------------------
    // IR 31: EPS backup power — reported when the EPS circuit is actually
    // flowing power (i.e. during grid-loss island mode), not just armed.
    // -----------------------------------------------------------------------
    #[test]
    fn ir31_eps_backup_power_zero_when_eps_disabled() {
        let state = make_state();
        // make_state() leaves enable_eps = false (PlantState default).
        assert_eq!(store_ir31(&state), 0);
    }

    #[test]
    fn ir31_eps_backup_power_zero_when_eps_armed_but_grid_healthy() {
        // HR(317)=1 just arms the circuit; until the grid actually trips,
        // the inverter supplies the house from the grid, so EPS power = 0.
        let mut state = make_state();
        state.enable_eps = true;
        // Grid is up by default; no faults active.
        assert!(state.grid.connected);
        assert_eq!(store_ir31(&state), 0);
    }

    #[test]
    fn ir31_eps_backup_power_reflects_battery_discharge_during_grid_loss() {
        // During grid-loss island mode the inverter feeds the house from the
        // battery; report that discharge on IR(31) in GE-wire convention
        // (positive = power flowing OUT of the battery).
        // Internal convention is the opposite, so the projection negates.
        let mut state = make_state();
        state.enable_eps = true;
        state.active_faults.push("grid_loss".into());
        // Battery is supplying 2.5 kW to the backup load (negative per
        // internal convention). Expected IR(31) = 2500 W.
        state.batteries[0].power_kw = -2.5;
        state.sync_battery_from_vec();
        assert_eq!(store_ir31(&state), 2500);
    }

    #[test]
    fn ir31_eps_backup_power_zero_during_grid_loss_when_eps_not_enabled() {
        // HR(317)=0 + grid_loss must read 0; EPS only fires if the user armed
        // it before the outage. Without that, the inverter has no way to
        // island, so IR(31) stays 0.
        let mut state = make_state();
        state.enable_eps = false;
        state.active_faults.push("grid_loss".into());
        state.batteries[0].power_kw = -1.0;
        state.sync_battery_from_vec();
        assert_eq!(store_ir31(&state), 0);
    }

    #[test]
    fn ir31_eps_backup_power_clamps_negative_during_grid_loss_charging() {
        // If the battery is still charging during grid_loss (e.g. PV surplus
        // charges the bank in island mode), IR(31) must clamp to 0 — EPS
        // power is never negative on the wire.
        let mut state = make_state();
        state.enable_eps = true;
        state.active_faults.push("grid_loss".into());
        state.batteries[0].power_kw = 1.5; // charging
        state.sync_battery_from_vec();
        assert_eq!(store_ir31(&state), 0);
    }

    // -----------------------------------------------------------------------
    // IR 6-7: Battery throughput total (uint32, ×0.1 kWh).
    //
    // Plant creation seeds each module to a realistic 3-year-old value
    // (`BATTERY_DEFAULT_AGE_YEARS` × `BATTERY_CYCLES_PER_YEAR` cycles) so the
    // register reads non-zero on day one and the runtime BatteryEngine
    // accumulates further throughput from there.
    // -----------------------------------------------------------------------
    #[test]
    fn ir_6_7_throughput_reflects_seeded_three_year_pack() {
        // End-to-end: PlantState::new() seeds throughput_kwh → projection
        // writes the (hi, lo) uint32 pair into IR(6-7) at ×0.1 kWh.
        let state = PlantState::new(test_ts());
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        let total_kwh = state.batteries[0].throughput_kwh;
        assert!(
            total_kwh > 0.0,
            "plant creation must seed throughput_kwh to a realistic value, got {total_kwh}",
        );

        let raw = (total_kwh / 0.1).round() as u32;
        let ir6 = store.read_by_space(6, RegisterSpace::Input).unwrap() as u32;
        let ir7 = store.read_by_space(7, RegisterSpace::Input).unwrap() as u32;
        let recombined = (ir6 << 16) | ir7;
        assert_eq!(
            recombined, raw,
            "IR(6-7) {ir6:#06x}::{ir7:#06x} = {recombined} (kWh*10) must match seed raw {raw}",
        );
    }

    #[test]
    fn ir_6_7_throughput_scales_linearly_with_battery_capacity() {
        // Two plants, same age but different nominal_capacity_kwh. The
        // throughput register should scale linearly with capacity since
        // cycles = throughput / capacity (same cycle count → same SOH).
        // PlantState::new seeds at 9.5 kWh default; to test scaling we
        // build batteries from scratch and apply the seed explicitly.
        let small_b = sim_models::BatteryState {
            capacity_kwh: 5.0,
            nominal_capacity_kwh: 5.0,
            ..sim_models::BatteryState::default()
        };
        let big_b = sim_models::BatteryState {
            capacity_kwh: 10.0,
            nominal_capacity_kwh: 10.0,
            ..sim_models::BatteryState::default()
        };

        let mut small = vec![small_b];
        let mut big = vec![big_b];
        sim_models::seed_batteries_for_age(&mut small, 3.0);
        sim_models::seed_batteries_for_age(&mut big, 3.0);

        assert!(
            big[0].throughput_kwh > small[0].throughput_kwh,
            "bigger battery must have proportionally more seeded throughput",
        );
        let ratio = big[0].throughput_kwh / small[0].throughput_kwh;
        assert!(
            (ratio - 2.0).abs() < 0.01,
            "ratio should be 2.0 (10/5), got {ratio}",
        );
    }

    // -----------------------------------------------------------------------
    // IR 47-48: Inverter powered-on runtime total (uint32, hours).
    //
    // Plant creation seeds `state.inverter.work_time_hours` to a 3-year
    // continuous-runtime baseline (26_280 h) so the register reads a
    // plausible mid-life value on day one.
    // -----------------------------------------------------------------------
    #[test]
    fn ir_47_48_work_time_reflects_seeded_three_year_runtime() {
        let state = PlantState::new(test_ts());
        let mut store = RegisterStore::new(default_register_catalogue());
        store.project_from_state(&state);

        let hours = state.inverter.work_time_hours;
        assert!(
            hours > 0.0,
            "plant creation must seed work_time_hours to a realistic value, got {hours}",
        );

        let raw = hours.round() as u32;
        let ir47 = store.read_by_space(47, RegisterSpace::Input).unwrap() as u32;
        let ir48 = store.read_by_space(48, RegisterSpace::Input).unwrap() as u32;
        let recombined = (ir47 << 16) | ir48;
        assert_eq!(
            recombined, raw,
            "IR(47-48) {ir47:#06x}::{ir48:#06x} = {recombined} h must match seed raw {raw}",
        );
    }

    #[test]
    fn ir_47_48_work_time_scales_linearly_with_age() {
        // 1-year-old inverter should read exactly INVERTER_HOURS_PER_YEAR,
        // independent of any battery state.
        let mut inv = sim_models::InverterState::default();
        sim_models::seed_inverter_for_age(&mut inv, 1.0);
        assert!(
            (inv.work_time_hours - sim_models::INVERTER_HOURS_PER_YEAR).abs() < 0.01,
            "1-year seed must equal {INVERTER_HOURS_PER_YEAR}, got {}",
            inv.work_time_hours,
        );
        let mut inv6 = sim_models::InverterState::default();
        sim_models::seed_inverter_for_age(&mut inv6, 6.0);
        assert!(
            (inv6.work_time_hours - 6.0 * sim_models::INVERTER_HOURS_PER_YEAR).abs() < 0.01,
            "6-year seed must equal 6 × {INVERTER_HOURS_PER_YEAR}, got {}",
            inv6.work_time_hours,
        );
    }

    // Small read-back helpers (project a state and read one register).
    fn store_hr223(state: &PlantState) -> u16 {
        let mut s = RegisterStore::new(default_register_catalogue());
        s.project_from_state(state);
        s.read_by_space(223, RegisterSpace::Holding).unwrap()
    }
    fn store_hr224(state: &PlantState) -> u16 {
        let mut s = RegisterStore::new(default_register_catalogue());
        s.project_from_state(state);
        s.read_by_space(224, RegisterSpace::Holding).unwrap()
    }
    fn store_ir0(state: &PlantState) -> u16 {
        let mut s = RegisterStore::new(default_register_catalogue());
        s.project_from_state(state);
        s.read_by_space(0, RegisterSpace::Input).unwrap()
    }
    fn store_ir57(state: &PlantState) -> u16 {
        let mut s = RegisterStore::new(default_register_catalogue());
        s.project_from_state(state);
        s.read_by_space(57, RegisterSpace::Input).unwrap()
    }
    fn store_ir31(state: &PlantState) -> u16 {
        let mut s = RegisterStore::new(default_register_catalogue());
        s.project_from_state(state);
        s.read_by_space(31, RegisterSpace::Input).unwrap()
    }
}
