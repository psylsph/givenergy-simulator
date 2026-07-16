//! Device model trait, tick context, and plant state types.
//!
//! All device models (solar, load, battery, inverter) implement
//! [`DeviceModel`], called once per simulation tick with mutable access
//! to [`PlantState`].

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Dongle misbehaviour simulation
// ---------------------------------------------------------------------------

/// Modes for simulating a faulty inverter dongle on the Modbus TCP port.
///
/// Real dongles sometimes return bad, stale, empty, or no data. This enum
/// lets the GUI toggle between failure modes so client apps can be tested
/// against realistic fault conditions.
#[derive(Debug, Clone, Copy, PartialEq, Default, Serialize, Deserialize)]
pub enum DongleMisbehaviourMode {
    /// Normal operation — return real register data.
    #[default]
    Off,
    /// Return all zeros for every register read (no data).
    EmptyData,
    /// Return stale/frozen data — registers never update after first read.
    StaleData,
    /// Return random/garbage values for every register.
    GarbageData,
    /// Drop the TCP connection on read attempts (no response).
    DropConnection,
    /// Intermittent failures — randomly return zeros ~50% of reads.
    Intermittent,
}

// ---------------------------------------------------------------------------
// UK EREC G98 / G99 standard grid-export limits
// ---------------------------------------------------------------------------
//
// Per EREC G98 (Issue 1 Amendment 7, October 2022, ENA), a "Micro-generator"
// is limited to a Registered Capacity of 16 A per phase, which at 230 V
// single-phase nominal is exactly 3680 W (16 A × 230 V). Three-phase G98
// Micro-generators can therefore export up to 3 × 3680 = 11_040 W across the
// three phases — but the `givenergy-modbus` HR 1063 (`p_export_limit`,
// C.deci) is hard-capped at `max=6500` on the wire (u16 × 0.1 dW = 6553.5 W
// representable max), so the ThreePhase family default is clamped to 6500 W
// to round-trip exactly on the wire. EMS HR 2071 is a full 16-bit register
// (0–65 535 W). Single-phase HR 26 is read-only on the wire (mirrors
// `config.max_ac_watts`) and has no client-settable export-limit field.
//
// `default_export_limit_w_for(inverter_type)` returns the standard UK
// legal default that should seed a brand-new plant so a freshly built
// sim matches what an MCS installer would set before the customer even
// touches the GUI.

/// Default UK single-phase G98 Micro-generator limit: 16 A × 230 V = 3680 W.
/// Reference: EREC G98 § Scope (Issue 1 Amendment 7, October 2022):
/// "16 A per phase, 230/400 V AC corresponds to 3.68 kilowatts (kW) on a
/// single-phase supply".
pub const DEFAULT_G98_SINGLE_PHASE_EXPORT_W: f64 = 3680.0;

/// Wire ceiling for the three-phase export limit, in watts.
///
/// The UK EREC G98 three-phase legal limit is 3 × (16 A × 230 V) = 11_040 W,
/// but the `givenergy-modbus` register HR 1063 (`p_export_limit`, C.deci)
/// encodes the value as watts × 10 into a u16, giving a representable max of
/// 65 535 dW = 6553.5 W, and givenergy-modbus itself hard-caps writes at
/// `max=6500`. We default to 6500 W so the value round-trips exactly on the
/// wire (state → HR 1063 read → state): any higher value would be silently
/// clamped by the projection and the Modbus client would read a different
/// number from what `state.inverter.export_limit_w` holds.
pub const DEFAULT_G98_THREE_PHASE_EXPORT_W: f64 = 6500.0;

/// Default UK EREC G98 export limit for a given inverter family, in watts.
///
/// Returns the standard default that should seed a brand-new plant so a
/// freshly built sim matches what an MCS installer would set before the
/// customer touches the GUI, **clamped to what the wire register can
/// actually represent**:
/// * Single-phase (Gen1-4 Hybrid, Polar, Gen3+, AC-coupled, PV, AIO,
///   AIOHybrid, Gateway) — 3680 W (UK EREC G98 single-phase = 16 A × 230 V).
/// * Three-phase (ThreePhase*, ACThreePhase) — 6500 W, the wire ceiling of
///   HR 1063 (`max=6500` in givenergy-modbus). The UK legal three-phase
///   G98 cap is 11_040 W but the register physically cannot represent it,
///   so we use the wire ceiling to keep the state ↔ HR 1063 round-trip
///   exact rather than silently clamping.
/// * EMS / EmsCommercial — 0 W. HR 2071 is full 16-bit with no G98 cap on
///   the wire; we return 0 so the export-limit code path is disabled
///   until an operator explicitly configures it (no sensible "legal
///   default" applies).
pub fn default_export_limit_w_for(inverter_type: &str) -> f64 {
    if inverter_type.starts_with("ThreePhase") || inverter_type == "ACThreePhase" {
        DEFAULT_G98_THREE_PHASE_EXPORT_W
    } else if inverter_type == "EMS" || inverter_type == "EmsCommercial" {
        0.0
    } else {
        DEFAULT_G98_SINGLE_PHASE_EXPORT_W
    }
}

/// Physical AC output capability of a given inverter type, in watts.
///
/// This is the inverter's *hardware* cap — how much AC power it can
/// physically produce. Distinct from [`default_export_limit_w_for`], which
/// is the *regulatory* DNO-facing export limit. For an AC-coupled 3 kW
/// inverter this is 3000 W (the inverter can't make more), while the UK
/// EREC G98 export limit is independently 3680 W.
///
/// Mirrors the table that previously lived only in
/// `sim_tauri::commands::create_plant` and `sim_api::main::configure_inverter`.
/// Both call sites now use this function so the inverter catalogue has a
/// single source of truth.
pub fn max_ac_watts_for(inverter_type: &str) -> f64 {
    match inverter_type {
        "Gen3Hybrid8kW" => 8000.0,
        "Gen3Hybrid10kW" => 10000.0,
        "Gen3Plus6kW" => 5000.0,
        "Gen3Plus4600" => 4600.0,
        "Gen3Plus3600" => 3600.0,
        "Gen3Plus6kW2" => 6000.0,
        "AllInOne6" => 6000.0,
        "AIO8kW" => 8000.0,
        "AIO10kW" => 10000.0,
        "AIOHybrid6kW" => 6000.0,
        "AIOHybrid8kW" => 8000.0,
        "AIOHybrid10kW" => 10000.0,
        "ThreePhase" => 6000.0,
        "ThreePhase8kW" => 8000.0,
        "ThreePhase10kW" => 10000.0,
        "ThreePhase11kW" => 11000.0,
        "ACCoupled" | "ACCoupled2" => 3000.0,
        "AllInOne" => 6000.0,
        "AllInOne5" => 5000.0,
        "Gen1Hybrid" => 5000.0,
        "Gen2Hybrid" => 5000.0,
        "Gen3Hybrid" => 5000.0,
        // Gateway: aggregates an All-in-One (6kW AC) behind it.
        "Gateway12kW" => 6000.0,
        _ => 5000.0,
    }
}

/// Physical battery (DC-side) power limit per inverter type, in watts.
///
/// This is the *hardware* cap on battery charge/discharge throughput
/// (continuous), derived from each inverter's datasheet. Distinct from
/// [`max_ac_watts_for`] (the AC cap) and from the user's
/// `battery_charge_limit_percent` / `battery_discharge_limit_percent`
/// percentage limits (HR 111/112).
///
/// Both `sim_tauri::commands::create_plant` and `sim_api::main::configure_inverter`
/// previously maintained their own match tables; the tables have been
/// merged here so the inverter catalogue has a single source of truth.
pub fn max_batt_w_for_inverter(inverter_type: &str) -> f64 {
    match inverter_type {
        // Gen 1 Hybrid 5.0: 2500W charge/discharge
        "Gen1Hybrid" => 2500.0,
        // Gen2 Hybrid 5.0: 3600W charge/discharge (same DC limit as Gen3)
        "Gen2Hybrid" => 3600.0,
        // Gen3 Hybrid 3.6/5.0: charge 3300W, discharge 3600W. Use 3600 as
        // the DC battery limit (the more conservative figure).
        "Gen3Hybrid" => 3600.0,
        // Gen3 Hybrid 8.0: charge 8000W, discharge 8500W
        "Gen3Hybrid8kW" => 8000.0,
        // Gen3 Hybrid 10.0: charge 10000W, discharge 10500W
        "Gen3Hybrid10kW" => 10000.0,
        // Gen3 Plus variants: 2600W (per datasheet)
        "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 2600.0,
        // AC Coupled / Mk2: 3000W charge/discharge
        "ACCoupled" | "ACCoupled2" => 3000.0,
        // All-in-One variants
        "AllInOne6" | "AllInOne" => 6000.0,
        "AllInOne5" => 5000.0,
        "AIO8kW" => 8000.0,
        "AIO10kW" => 10000.0,
        "AIOHybrid6kW" => 6000.0,
        "AIOHybrid8kW" => 8000.0,
        "AIOHybrid10kW" => 10000.0,
        // Three-phase variants
        "ThreePhase" => 6000.0,
        "ThreePhase8kW" => 8000.0,
        "ThreePhase10kW" => 10000.0,
        "ThreePhase11kW" => 11000.0,
        // Gateway: aggregates an All-in-One (6kW continuous) behind it.
        "Gateway12kW" => 6000.0,
        // Fallback: 3600W (the default for unlisted hybrid inverters).
        _ => 3600.0,
    }
}

/// DSP firmware version per inverter type, projected to HR 19. Mirrors
/// the table previously living in both `sim_tauri::commands::create_plant`
/// and `sim_api::main::configure_inverter`.
pub fn dsp_firmware_for_inverter(inv_type: &str) -> u16 {
    match inv_type {
        "Gen1Hybrid" => 110,
        "Gen2Hybrid" => 230,
        "Gen3Hybrid" => 449,
        "Gen3Plus6kW" | "Gen3Plus4600" | "Gen3Plus3600" | "Gen3Plus6kW2" => 510,
        "ACCoupled" | "ACCoupled2" => 305,
        "ThreePhase" | "ThreePhase8kW" | "ThreePhase10kW" => 612,
        "ThreePhase11kW" => 11043,
        "AllInOne6" | "AllInOne" | "AllInOne5" => 1010,
        "AIO8kW" | "AIO10kW" => 1010,
        "AIOHybrid6kW" | "AIOHybrid8kW" | "AIOHybrid10kW" => 1010,
        _ => 449,
    }
}

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
        // Default to Eco so a freshly-created plant reports `battery_mode = Eco`
        // (rather than the catch-all `ExportPaused` arm in the GUI projection).
        // Users who want full solar-to-battery priority can switch to Normal
        // explicitly; the inverter's factory mode is treated as a deliberate
        // user choice rather than a silent default.
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
    /// Manual inverter temperature override in °C. When `Some(t)` the
    /// inverter thermal model is bypassed and `temperature_celsius` is held
    /// at `t` every tick — useful for holding a fixed temperature to exercise
    /// derating / over-temperature behaviour. Set to `None` to restore the
    /// thermal model. Driven by the `SetInverterTemperature` command (GUI +
    /// CLI `--inverter-temperature`). Not a Modbus-writable register (IR 41
    /// is input/read-only on real hardware), so there is no HR write route.
    #[serde(default)]
    pub temperature_override: Option<f64>,
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
        let mut s = Self {
            mode_state: ModeState::default(),
            ac_power_w: 0.0,
            // UK EREC G98 single-phase Micro-generator limit: 16 A × 230 V
            // = 3680 W. See DEFAULT_G98_SINGLE_PHASE_EXPORT_W for the source.
            // `default_export_limit_w_for(inverter_type)` should be preferred
            // at plant-creation time so three-phase / EMS plants get the
            // correct per-family default.
            export_limit_w: DEFAULT_G98_SINGLE_PHASE_EXPORT_W,
            temperature_celsius: 35.0,
            temperature_override: None,
            dsp_firmware_version: default_dsp_firmware(),
            arm_firmware_version: 0,
            work_time_hours: 0.0,
            battery_self_heating: false,
            manual_battery_heater: false,
        };
        // Seed work_time_hours to a 3-year-old baseline so IR(47-48) reads
        // a plausible mid-life runtime from the moment a plant is built,
        // matching the battery throughput/SOH seed pattern.
        seed_inverter_for_age(&mut s, INVERTER_DEFAULT_AGE_YEARS);
        s
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

// ---------------------------------------------------------------------------
// Battery age / throughput seed constants and helpers.
//
// Used by `PlantState::with_battery_count` and the GUI create-plant path so a
// fresh plant ships with `throughput_kwh` already at a realistic mid-life
// value (and `soh` consistent with it), instead of zero. IR(6-7) therefore
// reads as a believable 3-year-old pack from the moment a plant is built,
// and the BatteryEngine accumulates further throughput from there.
//
// The three constants below MUST stay in sync with the runtime degradation
// model in `sim_core::BatteryEngine`:
//   `degradation_per_cycle` (default 0.0002 → 0.02%/cycle)
//   `min_soh` (default 0.5)
// so a seeded plant never reports a (throughput, soh) pair that the engine
// would immediately contradict on its first aging tick.
// ---------------------------------------------------------------------------

/// Baseline battery age (years) used when a fresh plant is created without
/// any user-supplied SOH or age. Three years is the canonical "mid-life"
/// reference: ~1000 cycles, around 80% of the design-cycle envelope.
pub const BATTERY_DEFAULT_AGE_YEARS: f64 = 3.0;

/// Equivalent full charge/discharge cycles per year for a typical residential
/// LFP battery bank. Used to convert age in years → cumulative cycles.
pub const BATTERY_CYCLES_PER_YEAR: f64 = 330.0;

/// Per-cycle capacity loss (fraction). Must match
/// `sim_core::BatteryEngine::degradation_per_cycle` (default 0.0002 = 0.02%).
pub const BATTERY_DEGRADATION_PER_CYCLE: f64 = 0.0002;

/// Derive a realistic `throughput_kwh` seed for a battery module of the given
/// age, in internal agreement with the degradation model. The runtime
/// `BatteryEngine` accumulates further throughput tick by tick; IR(6-7)
/// therefore climbs naturally from this baseline.
///
/// `nominal_capacity_kwh` is clamped to >= 0.1 to keep the math well-defined
/// for the empty / placeholder packs some tests construct.
pub fn seed_battery_throughput_for_age(years: f64, nominal_capacity_kwh: f64) -> f64 {
    let cycles = (years.max(0.0) * BATTERY_CYCLES_PER_YEAR).max(0.0);
    cycles * nominal_capacity_kwh.max(0.1)
}

/// Companion to [`seed_battery_throughput_for_age`]: derive the SOH that
/// matches a `throughput_kwh` value computed for the given age, clamped to
/// the `[MIN_SOH, 1.0]` band the runtime degradation uses.
pub fn seed_battery_soh_for_age(years: f64) -> f64 {
    let cycles = (years.max(0.0) * BATTERY_CYCLES_PER_YEAR).max(0.0);
    let soh = 1.0 - cycles * BATTERY_DEGRADATION_PER_CYCLE;
    soh.clamp(BATTERY_MIN_SOH, 1.0)
}

/// Lower bound for SOH used when seeding a freshly-created plant. Matches
/// `sim_core::BatteryEngine::min_soh` (default 0.5) so the runtime
/// degradation never immediately contradicts the seed.
pub const BATTERY_MIN_SOH: f64 = 0.5;

/// Apply the throughput + SOH seed to every module in `batteries` so they
/// look like a `years`-old pack at construction time. Convenience wrapper for
/// the two `PlantState` constructors and the GUI create-plant path; mutates
/// `throughput_kwh` and `soh` in place.
pub fn seed_batteries_for_age(batteries: &mut [BatteryState], years: f64) {
    let throughput = seed_battery_throughput_for_age(years, 1.0);
    // Per-module throughput scales with capacity; SOH is uniform.
    let soh = seed_battery_soh_for_age(years);
    for b in batteries.iter_mut() {
        b.throughput_kwh = throughput * b.nominal_capacity_kwh.max(0.1);
        b.soh = soh;
    }
}

// ---------------------------------------------------------------------------
// Inverter age / work-time seed.
//
// The inverter's `work_time_hours` field projects to IR(47-48) — the
// powered-on runtime hours the GivEnergy portal shows as the device's
// lifetime operational counter. A brand-new plant constructed via
// `PlantState::new` / `with_battery_count` would otherwise read `0` on
// day one, which is implausible for any real installation and shows up
// immediately when a client connects.
//
// Seed it to a 3-year-old continuous-runtime baseline so the register
// reads as a believable mid-life unit from the moment the plant is built,
// and `SimulationEngine::tick` accumulates further runtime from there.
// ---------------------------------------------------------------------------

/// Baseline inverter age (years) used when a fresh plant is created without
/// any user-supplied runtime override. Mirrors `BATTERY_DEFAULT_AGE_YEARS`
/// so a default plant looks like a coherent 3-year-old installation.
pub const INVERTER_DEFAULT_AGE_YEARS: f64 = 3.0;

/// Powered-on hours per calendar year. Real residential inverters sit at
/// standby for parts of the day, so a true installation would accrue
/// fewer than this; we use the continuous figure as a clean upper-bound
/// baseline that lines up with `INVERTER_DEFAULT_AGE_YEARS × HOURS_PER_YEAR
/// = 26_280` (a recognisable "3 calendar years" total).
pub const INVERTER_HOURS_PER_YEAR: f64 = 8760.0;

/// Derive the inverter's seeded `work_time_hours` for the given age. Mirrors
/// `seed_battery_throughput_for_age`: a realistic mid-life runtime figure so
/// IR(47-48) doesn't read `0` on a freshly-built plant.
pub fn seed_inverter_work_time_for_age(years: f64) -> f64 {
    (years.max(0.0) * INVERTER_HOURS_PER_YEAR).max(0.0)
}

/// Apply the work-time seed to `state.inverter.work_time_hours`. Mutates in
/// place; `SimulationEngine::tick` continues to accumulate `dt_hours` from
/// the seeded baseline.
pub fn seed_inverter_for_age(inverter: &mut InverterState, years: f64) {
    inverter.work_time_hours = seed_inverter_work_time_for_age(years);
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
    /// **Lifetime** solar generation in kWh — never reset by `EnergyTracker`,
    /// never zeroed at midnight. Projected to the IR 11-12 / IR 1374-1375
    /// "PV lifetime total" registers so clients see a stable lifetime figure
    /// rather than a daily bucket re-zeroed every night. Plant creation seeds
    /// this to a baseline so the registers read plausibly non-zero from the
    /// moment a plant is built.
    #[serde(default)]
    pub solar_lifetime_kwh: f64,
}

impl EnergyTotals {
    /// Small non-zero starter totals used **only by tests** that want to assert
    /// non-zero energy registers without running a full day of simulation.
    ///
    /// This is deliberately NOT used at runtime or in register projection: daily
    /// energy registers (PV energy today, import/export today, etc.) are a true
    /// power-integral accumulated by `EnergyTracker` and reset at midnight, so a
    /// fresh / early-morning plant legitimately reads zero and climbs smoothly
    /// with power. Injecting this fixture caused daily registers to jump to a
    /// fixed non-zero value the instant a client polled.
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
            solar_lifetime_kwh: 0.0,
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
    /// Test-only — never call from runtime/projection code.
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
            solar_lifetime_kwh: 0.0,
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
    /// Gateway firmware version byte (IR 1603). Values < 10 select V1
    /// (`GA000009`, high-register-first uint32, aio1 serial @ 1831-1835);
    /// values >= 10 select V2 (`GA000010`, low-register-first uint32,
    /// aio1 serial @ 1841-1845). Only meaningful for gateway inverters.
    #[serde(default = "default_gateway_fw_version")]
    pub gateway_fw_version: u16,
    /// Number of parallel AIO units behind the Gateway (1-3).
    /// Controls how the battery stack is partitioned into per-AIO registers.
    /// Only meaningful for gateway inverters.
    #[serde(default = "default_parallel_aio_num")]
    pub parallel_aio_num: u16,
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
fn default_gateway_fw_version() -> u16 {
    9
}
fn default_parallel_aio_num() -> u16 {
    1
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
            gateway_fw_version: 9,
            parallel_aio_num: 1,
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
    /// Simulate a faulty inverter dongle on the Modbus TCP port.
    /// Defaults to `Off` (normal operation).
    #[serde(default)]
    pub dongle_misbehaviour: DongleMisbehaviourMode,
}

impl PlantState {
    /// Create a default state at the given timestamp.
    /// The single battery is seeded as a `BATTERY_DEFAULT_AGE_YEARS`
    /// (3-year-old) pack so IR(6-7) reads a realistic value on day one and
    /// the runtime `BatteryEngine` continues to accumulate from there.
    pub fn new(timestamp: NaiveDateTime) -> Self {
        let mut default_battery = BatteryState::default();
        seed_batteries_for_age(
            std::slice::from_mut(&mut default_battery),
            BATTERY_DEFAULT_AGE_YEARS,
        );
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
            dongle_misbehaviour: DongleMisbehaviourMode::Off,
        }
    }

    /// Create a state with the given number of battery modules (1–6).
    /// Each module is seeded as a `BATTERY_DEFAULT_AGE_YEARS` (3-year-old)
    /// pack: `throughput_kwh` and `soh` are pre-populated from the
    /// degradation model so IR(6-7) and the GUI's battery-card read
    /// realistic values immediately instead of `0` / `1.0`. Runtime
    /// `BatteryEngine` accumulates further throughput from that baseline.
    pub fn with_battery_count(timestamp: NaiveDateTime, count: usize) -> Self {
        let count = count.clamp(1, 6);
        let mut batts: Vec<BatteryState> = (0..count).map(|_| BatteryState::default()).collect();
        seed_batteries_for_age(&mut batts, BATTERY_DEFAULT_AGE_YEARS);
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
            dongle_misbehaviour: DongleMisbehaviourMode::Off,
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

    /// Max charging power (W) the battery bank can absorb *right now*: the
    /// hardware C-rate scaled by the user's charge-power-limit %, clamped to 0
    /// at the SOC ceiling so a full battery stops absorbing and surplus flows
    /// to the grid instead.
    ///
    /// This replaces the old `soc_headroom` term, which computed *energy*
    /// remaining (Wh) but used it as a *power* cap (W) — a units mismatch that
    /// throttled charging far too early (e.g. a 10 kWh bank at 80% SOC has
    /// 2 kWh of headroom, which the old code read as a 2 kW charge cap). The
    /// `BatteryEngine` already clamps SOC to `[min_soc, max_soc]`, so only the
    /// boundary needs guarding here.
    ///
    /// Does NOT include the inverter AC throughput cap — callers apply that.
    pub fn charge_power_ceiling_w(&self) -> f64 {
        if self.aggregate_soc() >= self.max_aggregate_soc() - 1e-3 {
            return 0.0;
        }
        self.effective_max_charge_w().max(0.0)
    }

    /// Max discharging power (W) the battery bank can deliver *right now*: the
    /// hardware C-rate scaled by the user's discharge-power-limit %, clamped to
    /// 0 at the SOC floor so an empty battery stops discharging and load falls
    /// to the grid.
    ///
    /// Mirrors `charge_power_ceiling_w` (replaces the old `soc_available` Wh-as-W
    /// term). Does NOT include the inverter AC throughput cap.
    pub fn discharge_power_ceiling_w(&self) -> f64 {
        if self.aggregate_soc() <= self.min_aggregate_soc() + 1e-3 {
            return 0.0;
        }
        self.effective_max_discharge_w().max(0.0)
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
    /// HR 96: enables charging according to the configured charge slots.
    pub enable_charge: bool,
    /// HR 59: enables discharging according to the configured discharge slots.
    pub enable_discharge: bool,
    /// Raw Modbus values for schedule time registers.
    ///
    /// Real inverters retain arbitrary `u16` values written to slot registers,
    /// even when they are not valid HHMM times. The simulation uses the parsed
    /// decimal-hour fields above for physics, while this map preserves the exact
    /// wire values for projection, persistence, and read-after-write behaviour.
    #[serde(default)]
    pub raw_time_registers: std::collections::BTreeMap<u16, u16>,
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

impl Schedule {
    /// Apply Modbus schedule register writes without coupling enable flags to
    /// slot storage. Raw time values are retained exactly; invalid HHMM values
    /// are represented as `-1.0` in the parsed physics model.
    pub fn apply_modbus_updates(&mut self, updates: &std::collections::HashMap<u16, u16>) {
        self.record_raw_time_writes(updates);
        // Charge slot 1 (HR 94-95) — primary
        if let Some(&v) = updates.get(&94) {
            self.charge_start = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&95) {
            self.charge_end = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        // Charge slot 2 (HR 31-32, GivTCP Gen3 aliases HR 243-244)
        if let Some(&v) = updates.get(&31).or_else(|| updates.get(&243)) {
            self.charge_start_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&32).or_else(|| updates.get(&244)) {
            self.charge_end_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        // Discharge slot 1 (HR 56-57) — primary
        if let Some(&v) = updates.get(&56) {
            self.discharge_start = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&57) {
            self.discharge_end = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        // Discharge slot 2 (HR 44-45)
        if let Some(&v) = updates.get(&44) {
            self.discharge_start_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&45) {
            self.discharge_end_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        // Charge target SOC (HR 116)
        if let Some(&v) = updates.get(&116) {
            self.charge_target_soc = v as f64;
        }
        // Charge target SOC slot 1 per-slot (HR 242)
        if let Some(&v) = updates.get(&242) {
            self.charge_target_soc = v as f64;
        }
        // Charge target SOC slot 2 per-slot (HR 245)
        if let Some(&v) = updates.get(&245) {
            self.charge_target_soc_2 = v as f64;
        }
        // Discharge target SOC slot 1 per-slot (HR 272)
        if let Some(&v) = updates.get(&272) {
            self.discharge_target_soc = v as f64;
        }
        // Discharge target SOC slot 2 per-slot (HR 275)
        if let Some(&v) = updates.get(&275) {
            self.discharge_target_soc_2 = v as f64;
        }
        // Charge slot 3-10 (HR 246-268, alternating start/end)
        if let Some(&v) = updates.get(&246) {
            self.charge_start_3 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&247) {
            self.charge_end_3 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&248) {
            self.charge_target_soc_3 = v as f64;
        }
        if let Some(&v) = updates.get(&249) {
            self.charge_start_4 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&250) {
            self.charge_end_4 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&251) {
            self.charge_target_soc_4 = v as f64;
        }
        if let Some(&v) = updates.get(&252) {
            self.charge_start_5 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&253) {
            self.charge_end_5 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&254) {
            self.charge_target_soc_5 = v as f64;
        }
        if let Some(&v) = updates.get(&255) {
            self.charge_start_6 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&256) {
            self.charge_end_6 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&257) {
            self.charge_target_soc_6 = v as f64;
        }
        if let Some(&v) = updates.get(&258) {
            self.charge_start_7 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&259) {
            self.charge_end_7 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&260) {
            self.charge_target_soc_7 = v as f64;
        }
        if let Some(&v) = updates.get(&261) {
            self.charge_start_8 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&262) {
            self.charge_end_8 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&263) {
            self.charge_target_soc_8 = v as f64;
        }
        if let Some(&v) = updates.get(&264) {
            self.charge_start_9 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&265) {
            self.charge_end_9 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&266) {
            self.charge_target_soc_9 = v as f64;
        }
        if let Some(&v) = updates.get(&267) {
            self.charge_start_10 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&268) {
            self.charge_end_10 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&269) {
            self.charge_target_soc_10 = v as f64;
        }
        // Discharge slot 3-10 (HR 276-298, alternating start/end)
        if let Some(&v) = updates.get(&276) {
            self.discharge_start_3 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&277) {
            self.discharge_end_3 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&278) {
            self.discharge_target_soc_3 = v as f64;
        }
        if let Some(&v) = updates.get(&279) {
            self.discharge_start_4 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&280) {
            self.discharge_end_4 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&281) {
            self.discharge_target_soc_4 = v as f64;
        }
        if let Some(&v) = updates.get(&282) {
            self.discharge_start_5 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&283) {
            self.discharge_end_5 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&284) {
            self.discharge_target_soc_5 = v as f64;
        }
        if let Some(&v) = updates.get(&285) {
            self.discharge_start_6 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&286) {
            self.discharge_end_6 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&287) {
            self.discharge_target_soc_6 = v as f64;
        }
        if let Some(&v) = updates.get(&288) {
            self.discharge_start_7 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&289) {
            self.discharge_end_7 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&290) {
            self.discharge_target_soc_7 = v as f64;
        }
        if let Some(&v) = updates.get(&291) {
            self.discharge_start_8 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&292) {
            self.discharge_end_8 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&293) {
            self.discharge_target_soc_8 = v as f64;
        }
        if let Some(&v) = updates.get(&294) {
            self.discharge_start_9 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&295) {
            self.discharge_end_9 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&296) {
            self.discharge_target_soc_9 = v as f64;
        }
        if let Some(&v) = updates.get(&297) {
            self.discharge_start_10 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&298) {
            self.discharge_end_10 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&299) {
            self.discharge_target_soc_10 = v as f64;
        }
        // EMS discharge slots 1-3 (HR 2044-2052)
        if let Some(&v) = updates.get(&2044) {
            self.discharge_start = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2045) {
            self.discharge_end = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2046) {
            self.discharge_target_soc = v as f64;
        }
        if let Some(&v) = updates.get(&2047) {
            self.discharge_start_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2048) {
            self.discharge_end_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2049) {
            self.discharge_target_soc_2 = v as f64;
        }
        if let Some(&v) = updates.get(&2050) {
            self.discharge_start_3 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2051) {
            self.discharge_end_3 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2052) {
            self.discharge_target_soc_3 = v as f64;
        }
        // EMS charge slots 1-3 (HR 2053-2061)
        if let Some(&v) = updates.get(&2053) {
            self.charge_start = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2054) {
            self.charge_end = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2055) {
            self.charge_target_soc = v as f64;
        }
        if let Some(&v) = updates.get(&2056) {
            self.charge_start_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2057) {
            self.charge_end_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2058) {
            self.charge_target_soc_2 = v as f64;
        }
        if let Some(&v) = updates.get(&2059) {
            self.charge_start_3 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2060) {
            self.charge_end_3 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&2061) {
            self.charge_target_soc_3 = v as f64;
        }

        // HR 96/59 toggle schedule execution without modifying stored slots.
        if let Some(&v) = updates.get(&96) {
            self.enable_charge = v != 0;
        }
        if let Some(&v) = updates.get(&59) {
            self.enable_discharge = v != 0;
        }
        // TPH charge target SOC (HR 1111) — same as HR 116
        if let Some(&v) = updates.get(&1111) {
            self.charge_target_soc = v as f64;
        }
        if let Some(&v) = updates.get(&1112) {
            self.enable_charge = v != 0;
        }
        if let Some(&v) = updates.get(&1113) {
            self.charge_start = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&1114) {
            self.charge_end = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&1115) {
            self.charge_start_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&1116) {
            self.charge_end_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&1118) {
            self.discharge_start = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&1119) {
            self.discharge_end = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&1120) {
            self.discharge_start_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        if let Some(&v) = updates.get(&1121) {
            self.discharge_end_2 = hhmm_to_schedule_hours(v).unwrap_or(-1.0);
        }
        // Charge target SOC (HR 116)
    }

    /// Preserve exact raw values for all Modbus schedule start/end registers.
    pub fn record_raw_time_writes(&mut self, updates: &std::collections::HashMap<u16, u16>) {
        for (&address, &value) in updates {
            if is_schedule_time_register(address) {
                self.raw_time_registers.insert(address, value);
            }
        }
    }

    /// Return a preserved raw value, falling back to a value derived from the
    /// parsed schedule model when the register has not been written directly.
    pub fn raw_time_or(&self, address: u16, derived: u16) -> u16 {
        self.raw_time_registers
            .get(&address)
            .copied()
            .unwrap_or(derived)
    }

    /// Whether any charge-slot endpoint has been written directly over Modbus,
    /// including an invalid value that should suppress always-on fallback.
    pub fn has_raw_charge_times(&self) -> bool {
        self.raw_time_registers.keys().any(|address| {
            matches!(
                address,
                31..=32 | 94..=95 | 243..=244 | 246..=269
                    | 1113..=1116 | 2053..=2061
            )
        })
    }

    /// Whether any discharge-slot endpoint has been written directly over
    /// Modbus, including invalid values.
    pub fn has_raw_discharge_times(&self) -> bool {
        self.raw_time_registers.keys().any(|address| {
            matches!(
                address,
                44..=45 | 56..=57 | 276..=299 | 1118..=1121 | 2044..=2052
            )
        })
    }
}

fn hhmm_to_schedule_hours(value: u16) -> Option<f64> {
    if value == 60 {
        return None;
    }
    let hours = value / 100;
    let minutes = value % 100;
    (hours <= 23 && minutes <= 59).then_some(hours as f64 + minutes as f64 / 60.0)
}

fn is_schedule_time_register(address: u16) -> bool {
    matches!(
        address,
        31..=32 | 44..=45 | 56..=57 | 94..=95
            | 243..=244 | 246..=247 | 249..=250 | 252..=253
            | 255..=256 | 258..=259 | 261..=262 | 264..=265 | 267..=268
            | 276..=277 | 279..=280 | 282..=283 | 285..=286
            | 288..=289 | 291..=292 | 294..=295 | 297..=298
            | 1113..=1116 | 1118..=1121
            | 2044..=2045 | 2047..=2048 | 2050..=2051
            | 2053..=2054 | 2056..=2057 | 2059..=2060
    )
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
            raw_time_registers: std::collections::BTreeMap::new(),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn g98_single_phase_default_is_3680_w() {
        // UK EREC G98: 16 A × 230 V = 3680 W is the single-phase
        // Micro-generator Registered Capacity. The constant is the source
        // of truth — the helper must return exactly that value for every
        // single-phase inverter family.
        assert_eq!(DEFAULT_G98_SINGLE_PHASE_EXPORT_W, 3680.0);

        for inv in &[
            "Gen1Hybrid",
            "Gen2Hybrid",
            "Gen3Hybrid",
            "Gen3Hybrid8kW",
            "Gen3Hybrid10kW",
            "Gen3Plus6kW",
            "Gen3Plus4600",
            "Gen3Plus3600",
            "Gen3Plus6kW2",
            "Gen3Plus7kW",
            "Gen3Plus8kW",
            "Polar5kW",
            "Polar4600",
            "Polar3600",
            "Polar6kW",
            "Polar7kW",
            "Polar8kW",
            "PVInverter5kW",
            "PVInverter4600",
            "PVInverter3600",
            "PVInverter6kW",
            "ACCoupled",
            "ACCoupled2",
            "AllInOne6",
            "AllInOne",
            "AllInOne5",
            "AIO6kW",
            "AIO8kW",
            "AIO10kW",
            "AIOHybrid6kW",
            "AIOHybrid8kW",
            "AIOHybrid10kW",
            "AIOHybrid12kW",
            "Gen4Hybrid6kW",
            "Hybrid3600",
            "Hybrid4600",
            "Gateway12kW",
        ] {
            assert_eq!(
                default_export_limit_w_for(inv),
                3680.0,
                "single-phase {inv} should default to UK G98 3680 W"
            );
        }
    }

    #[test]
    fn three_phase_default_is_wire_ceiling_6500_w() {
        // UK EREC G98 three-phase Micro-generator cap is 11.04 kW, but the
        // wire register HR 1063 (`p_export_limit`, C.deci) encodes watts × 10
        // into a u16. givenergy-modbus hard-caps writes at `max=6500`, and
        // even the raw u16 ceiling (65 535 dW = 6553.5 W) is below the legal
        // limit. We therefore default to 6500 W so the value round-trips
        // exactly on the wire: the Modbus client reads back the same value
        // that `state.inverter.export_limit_w` holds, with no silent
        // clamping. A higher default (e.g. 11 040 W) would be clamped by the
        // projection and the client would read 6553.5 W instead.
        assert_eq!(DEFAULT_G98_THREE_PHASE_EXPORT_W, 6500.0);
        for inv in &[
            "ThreePhase",
            "ThreePhase8kW",
            "ThreePhase10kW",
            "ThreePhase11kW",
            "ACThreePhase",
        ] {
            assert_eq!(
                default_export_limit_w_for(inv),
                6500.0,
                "{inv} should default to the HR 1063 wire ceiling (6500 W) for exact round-trip"
            );
        }
    }

    #[test]
    fn ems_default_is_zero_until_operator_configures() {
        // EMS / EmsCommercial / Gateway use HR 2071, a full 16-bit register
        // with no G98 cap on the wire. The simulator mustn't pretend a
        // specific "legal default" applies — return 0 so the export-limit
        // code path is disabled until an operator sets it explicitly.
        for inv in &["EMS", "EmsCommercial"] {
            assert_eq!(
                default_export_limit_w_for(inv),
                0.0,
                "{inv} should default to 0 (operator-configured)"
            );
        }
    }

    #[test]
    fn inverter_state_default_uses_g98_single_phase_limit() {
        // `InverterState::default()` is reached whenever a plant snapshot
        // is reconstructed without an explicit inverter_type — e.g. test
        // fixtures. It must produce a sensible UK G98 single-phase value
        // (3680 W), not the old 3600 W.
        let s = InverterState::default();
        assert_eq!(s.export_limit_w, 3680.0);
    }

    #[test]
    fn schedule_enable_writes_do_not_modify_slots() {
        let mut schedule = Schedule {
            charge_start: 1.5,
            charge_end: 4.0,
            discharge_start: 17.0,
            discharge_end: 21.0,
            enable_charge: true,
            enable_discharge: true,
            ..Schedule::default()
        };

        schedule.apply_modbus_updates(&[(96, 0), (59, 0)].into());

        assert!(!schedule.enable_charge);
        assert!(!schedule.enable_discharge);
        assert_eq!((schedule.charge_start, schedule.charge_end), (1.5, 4.0));
        assert_eq!(
            (schedule.discharge_start, schedule.discharge_end),
            (17.0, 21.0)
        );
    }

    #[test]
    fn schedule_preserves_invalid_raw_times_without_activating_them() {
        let mut schedule = Schedule::default();
        schedule.apply_modbus_updates(&[(94, 2360), (95, u16::MAX), (96, 1)].into());

        assert_eq!(schedule.raw_time_or(94, 60), 2360);
        assert_eq!(schedule.raw_time_or(95, 60), u16::MAX);
        assert_eq!(schedule.charge_start, -1.0);
        assert_eq!(schedule.charge_end, -1.0);
        assert!(schedule.enable_charge);
    }

    #[test]
    fn ac_coupled_keeps_3kw_ac_output_but_gets_3680w_grid_export_limit() {
        // Regression guard: a 3 kW AC-coupled inverter physically can't
        // produce more than 3 kW AC, so `max_ac_watts` must stay at 3000
        // (representing the inverter's hardware cap). But the UK EREC G98
        // grid-port export limit is a regulatory cap on what can flow
        // *out* to the grid — that's 16 A × 230 V = 3680 W regardless of
        // how big the inverter is. The two are distinct concepts and must
        // not be conflated: `max_ac_watts` is the inverter's *capability*,
        // `export_limit_w` is the DNO-facing *export limit*. A user with a
        // 3 kW AC-coupled inverter in the UK still gets a 3680 W export
        // limit set as the default — they're free to lower it for their
        // own DNO arrangement, but they shouldn't see "3000 W" as the
        // *grid* limit just because that's how big the inverter is.
        for inv in &["ACCoupled", "ACCoupled2"] {
            assert_eq!(
                max_ac_watts_for(inv),
                3000.0,
                "{inv} must keep its 3 kW physical AC output"
            );
            assert_eq!(
                default_export_limit_w_for(inv),
                3680.0,
                "{inv} grid-port export limit must default to UK G98 3680 W"
            );
            assert!(
                default_export_limit_w_for(inv) > max_ac_watts_for(inv),
                "{inv}: export limit ({}) must exceed physical AC output ({}) so a 3 kW inverter \
                 isn't artificially capped below what its hardware can do.",
                default_export_limit_w_for(inv),
                max_ac_watts_for(inv),
            );
        }
    }
}
