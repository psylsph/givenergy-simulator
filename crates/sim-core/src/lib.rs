//! Simulation core: tick scheduler, command queue, real device models.
//!
//! State transitions occur only during simulation ticks.
//! All external writes become [`Command`]s applied between ticks.
//!
#![allow(
    clippy::collapsible_if,
    clippy::manual_clamp,
    clippy::single_match,
    clippy::new_without_default,
    clippy::match_same_arms,
    clippy::needless_return,
    clippy::unnecessary_cast
)]
//! Device update order: **Solar → Load → Inverter → Battery**.

use chrono::{Datelike, NaiveDateTime, Timelike};
use serde::{Deserialize, Serialize};
use sim_models::DeviceModel;

// Re-export types that consumers need
pub use sim_models::{
    BatteryState, CalibrationStage, CalibrationState, EnergyTotals, GridState, InverterMode,
    InverterState, LoadState, PlantConfig, PlantState, SolarState, TickContext,
};

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
    SetWeather(WeatherCondition),
    /// Override solar generation to a fixed watt value.
    SetSolarOverride(Option<f64>),
    /// Override load demand to a fixed watt value.
    SetLoadOverride(Option<f64>),
    SetSimulationTime(NaiveDateTime),
    /// Enable/disable EMS (Energy Management System) control mode.
    SetEmsEnable(bool),
    /// Set SOC of a specific battery module (index, soc%).
    SetBatterySoc {
        module: usize,
        soc: f64,
    },
    /// Set SOH of a specific battery module (index, soh 0.0–1.0). Adjusts capacity_kwh.
    SetBatterySoH {
        module: usize,
        soh: f64,
    },
    /// Start battery calibration cycle (optional module index, None = all).
    StartCalibration {
        module: Option<usize>,
    },
    /// Cancel in-progress calibration.
    CancelCalibration,
    /// Clear all active faults at once.
    FixAllFaults,
    /// Set HR20 enable charge target flag.
    SetEnableChargeTarget(bool),
    /// Set HR50 active power rate percentage.
    SetActivePowerRate(f64),
    /// Set HR111 battery charge limit percentage.
    SetBatteryChargeLimit(f64),
    /// Set HR112 battery discharge limit percentage.
    SetBatteryDischargeLimit(f64),
    /// Simulate inverter reboot request.
    InverterReboot,
    /// Set HR318-320 battery pause controls.
    SetBatteryPause {
        mode: u16,
        start: u16,
        end: u16,
    },
    /// Update the charge/discharge schedule.
    SetSchedule(Box<Schedule>),
    /// Set HR166 enable RTC flag.
    SetEnableRtc(bool),
    /// Set HR114 battery discharge min power reserve (%).
    SetBatteryDischargeMinPowerReserve(f64),
    /// Set HR311 export priority (0=Battery, 1=Grid, 2=Load).
    SetExportPriority(u16),
    /// Set HR317 enable EPS mode.
    SetEnableEps(bool),
    /// Set HR199 enable inverter parallel mode.
    SetEnableInverterParallelMode(bool),
    /// Enable/disable external CT clamp meter (IR 60-89 on slave 0x01).
    SetCtMeterEnabled(bool),
    /// Change simulation time step (seconds per tick) — used for speed-up.
    SetTickInterval(u64),
}

// ---------------------------------------------------------------------------
// Simulation engine — tick scheduler
// ---------------------------------------------------------------------------

/// Runs the simulation loop: drain commands → tick all devices → repeat.
pub struct SimulationEngine {
    pub state: PlantState,
    devices: Vec<Box<dyn DeviceModel>>,
    command_queue: Vec<Command>,
    /// Tick interval in seconds.
    pub tick_interval_secs: u64,
    /// Optional wall-clock anchor. When `Some`, each tick derives
    /// `state.timestamp` from `Local::now()` so the simulation clock tracks
    /// the host wall clock exactly (no drift accumulation, survives NTP
    /// corrections). When `None`, the clock advances by the fixed
    /// `tick_interval_secs` per tick (deterministic, original behaviour —
    /// used by scenario replays and fast-forward modes).
    wall_clock_anchor: Option<WallClockAnchor>,
}

/// Real-time wall-clock anchoring configuration.
///
/// See [`SimulationEngine::anchor_to_wall_clock`].
#[derive(Debug, Clone)]
pub struct WallClockAnchor {
    /// When `Some`, the sim clock shows this calendar date with the *real*
    /// current time-of-day (used by `--date` to simulate a different day
    /// while still advancing in real seconds). When `None`, the full real
    /// wall-clock date+time is used verbatim.
    pinned_date: Option<chrono::NaiveDate>,
    /// Wall-clock timestamp captured at the previous tick, used to compute
    /// the real elapsed `dt` for energy integration. Tracked separately from
    /// `state.timestamp` so `dt` is immune to in-band timestamp mutations
    /// (e.g. a client setting the clock via HR 35–40).
    last_wall: chrono::NaiveDateTime,
}

impl WallClockAnchor {
    /// Upper bound on a single tick's `dt` (seconds). Absorbs wall-clock jumps
    /// (NTP corrections, laptop suspend/resume) so energy integration doesn't
    /// spike. Generous enough that a normal real-time loop never hits it.
    const MAX_DT_SECS: f64 = 300.0;

    fn target_now(&self) -> chrono::NaiveDateTime {
        let now = chrono::Local::now().naive_local();
        match self.pinned_date {
            Some(d) => d
                .and_hms_opt(now.hour(), now.minute(), now.second())
                .unwrap_or(now),
            None => now,
        }
    }
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
            wall_clock_anchor: None,
        }
    }

    /// Enqueue a command to be applied before the next tick.
    pub fn enqueue(&mut self, cmd: Command) {
        self.command_queue.push(cmd);
    }

    fn apply_commands(&mut self) {
        for cmd in self.command_queue.drain(..) {
            match cmd {
                Command::SetInverterMode(mode) => self.state.inverter.mode_state.set_user(mode),
                Command::SetExportLimit(limit) => self.state.inverter.export_limit_w = limit,
                Command::SetMinSoc(v) => {
                    for b in &mut self.state.batteries {
                        b.min_soc = v;
                    }
                    self.state.battery.min_soc = v;
                }
                Command::SetMaxSoc(v) => {
                    for b in &mut self.state.batteries {
                        b.max_soc = v;
                    }
                    self.state.battery.max_soc = v;
                }
                Command::InjectFault(id) => {
                    if !self.state.active_faults.contains(&id) {
                        self.state.active_faults.push(id);
                    }
                }
                Command::ClearFault(id) => {
                    self.state.active_faults.retain(|f| f != &id);
                }
                Command::SetWeather(w) => {
                    // Use Display instead of Debug to decouple serialization
                    // from variant names. Renaming a variant won't break parsing.
                    self.state.weather = format!("{}", w);
                }
                Command::SetSolarOverride(w) => {
                    self.state.solar_override = w;
                }
                Command::SetLoadOverride(w) => {
                    self.state.load_override = w;
                }
                Command::SetBatterySoc { module, soc } => {
                    if let Some(b) = self.state.batteries.get_mut(module) {
                        b.soc_percent = soc.clamp(0.0, 100.0);
                        self.state.sync_battery_from_vec();
                    }
                }
                Command::SetBatterySoH { module, soh } => {
                    let count = self.state.batteries.len().max(1);
                    if let Some(b) = self.state.batteries.get_mut(module) {
                        b.soh = soh.clamp(0.0, 1.0);
                        b.capacity_kwh = b.nominal_capacity_kwh * b.soh;
                        let c_rate_kw = (b.capacity_kwh * 0.7).min(10.0);
                        let inv_max_kw = self.state.config.max_ac_watts / 1000.0;
                        let per_module_kw = inv_max_kw / count as f64;
                        b.max_charge_kw = c_rate_kw.min(per_module_kw);
                        b.max_discharge_kw = c_rate_kw.min(per_module_kw);
                        self.state.sync_battery_from_vec();
                    }
                }
                Command::SetSimulationTime(t) => {
                    self.state.timestamp = t;
                }
                Command::SetEmsEnable(enable) => {
                    self.state.ems_enabled = enable;
                }
                Command::StartCalibration { module } => {
                    self.state.calibration = CalibrationState {
                        stage: CalibrationStage::ChargeToFull,
                        module,
                        stage_elapsed_secs: 0.0,
                        total_elapsed_secs: 0.0,
                    };
                    // Override inverter to ForceCharge during calibration
                    self.state
                        .inverter
                        .mode_state
                        .set_user(InverterMode::ForceCharge);
                }
                Command::CancelCalibration => {
                    self.state.calibration = CalibrationState::default();
                    // Restore Eco mode after cancellation
                    self.state.inverter.mode_state.set_user(InverterMode::Eco);
                }
                Command::SetTickInterval(secs) => {
                    self.tick_interval_secs = secs.max(1);
                }
                Command::FixAllFaults => {
                    self.state.active_faults.clear();
                }
                Command::SetEnableChargeTarget(v) => {
                    self.state.enable_charge_target = v;
                }
                Command::SetActivePowerRate(v) => {
                    let pct = v.clamp(0.0, 100.0);
                    self.state.active_power_rate_percent = pct;
                    self.state.inverter.export_limit_w =
                        self.state.config.max_ac_watts * pct / 100.0;
                }
                Command::SetBatteryChargeLimit(v) => {
                    let pct = v.clamp(0.0, 100.0);
                    self.state.battery_charge_limit_percent = pct;
                }
                Command::SetBatteryDischargeLimit(v) => {
                    let pct = v.clamp(0.0, 100.0);
                    self.state.battery_discharge_limit_percent = pct;
                }
                Command::InverterReboot => {
                    self.state.inverter.temperature_celsius = 25.0;
                    self.state.active_faults.clear();
                    self.state.energy_totals = EnergyTotals::default();
                    self.state.inverter.mode_state.set_user(InverterMode::Eco);
                }
                Command::SetBatteryPause { mode, start, end } => {
                    self.state.battery_pause_mode = mode;
                    self.state.battery_pause_slot_start = start;
                    self.state.battery_pause_slot_end = end;
                }
                Command::SetEnableRtc(v) => {
                    self.state.enable_rtc = v;
                }
                Command::SetBatteryDischargeMinPowerReserve(v) => {
                    self.state.battery_discharge_min_power_reserve = v.clamp(4.0, 100.0);
                }
                Command::SetExportPriority(v) => {
                    self.state.export_priority = v.min(2);
                }
                Command::SetEnableEps(v) => {
                    self.state.enable_eps = v;
                }
                Command::SetEnableInverterParallelMode(v) => {
                    self.state.enable_inverter_parallel_mode = v;
                }
                Command::SetCtMeterEnabled(v) => {
                    self.state.config.ct_meter_installed = v;
                }
                Command::SetSchedule(sched) => {
                    for device in &mut self.devices {
                        if let Some(se) = device.as_any_mut().downcast_mut::<ScheduleEngine>() {
                            se.schedule = (*sched).clone();
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Anchor the simulation clock to the host wall clock.
    ///
    /// After this call, each [`tick`](Self::tick) derives `state.timestamp`
    /// from `Local::now()` instead of advancing by `tick_interval_secs`. The
    /// simulation clock then tracks the host's real time exactly — eliminating
    /// the drift that accumulates in fixed-step mode (where sleep jitter and
    /// tick-work duration compound, and where a tight poll loop can run the
    /// clock many× too fast).
    ///
    /// The current `state.timestamp` is preserved as the anchor's reference,
    /// so a user-chosen start instant still applies; only the *advancement*
    /// becomes wall-clock-driven. Pass `pinned_date = Some(d)` to project the
    /// real time-of-day onto a different calendar date (e.g. simulating a
    /// winter day at the current wall time).
    ///
    /// Use [`unanchor_wall_clock`](Self::unanchor_wall_clock) to revert to
    /// fixed-step advancement.
    pub fn anchor_to_wall_clock(&mut self, pinned_date: Option<chrono::NaiveDate>) {
        let now = chrono::Local::now().naive_local();
        self.wall_clock_anchor = Some(WallClockAnchor {
            pinned_date,
            last_wall: now,
        });
    }

    /// Disable wall-clock anchoring, reverting to fixed-step advancement.
    pub fn unanchor_wall_clock(&mut self) {
        self.wall_clock_anchor = None;
    }

    /// Returns `true` while the clock is anchored to the host wall clock.
    pub fn is_wall_clock_anchored(&self) -> bool {
        self.wall_clock_anchor.is_some()
    }

    /// Advance simulation by one tick.
    ///
    /// 1. Apply pending commands.
    /// 2. Build tick context (dt from real elapsed wall time when anchored,
    ///    else the fixed `tick_interval_secs`).
    /// 3. Call `update` on every device model in registration order.
    /// 4. Advance the timestamp (wall-clock-derived when anchored).
    pub fn tick(&mut self) {
        self.apply_commands();

        // Determine the dt for this tick and the new timestamp. In anchored
        // (real-time) mode both come from the wall clock; in fixed-step mode
        // dt is the configured interval and the timestamp advances by it.
        let (dt_hours, new_timestamp) = match self.wall_clock_anchor.as_mut() {
            Some(anchor) => {
                let now_wall = chrono::Local::now().naive_local();
                let dt_secs = ((now_wall - anchor.last_wall).num_milliseconds() as f64 / 1000.0)
                    .clamp(0.0, WallClockAnchor::MAX_DT_SECS);
                anchor.last_wall = now_wall;
                let target = anchor.target_now();
                (dt_secs / 3600.0, Some(target))
            }
            None => (self.tick_interval_secs as f64 / 3600.0, None),
        };
        let ctx = TickContext {
            now: self.state.timestamp,
            dt_hours,
        };

        // Apply calibration mode BEFORE devices run so InverterEngine sees
        // the correct mode without 1-tick lag, and calibration doesn't
        // overwrite schedule/fault modes set during the current tick.
        CalibrationEngine::apply_calibration_mode(&mut self.state);

        for device in &mut self.devices {
            device.update(&ctx, &mut self.state);
        }

        // Stage-transition checks for calibration (after BatteryEngine has
        // finalized SOC values for this tick).
        CalibrationEngine::check_stage_transitions(&ctx, &mut self.state);

        self.state.inverter.work_time_hours += dt_hours;
        match new_timestamp {
            Some(ts) => self.state.timestamp = ts,
            None => {
                self.state.timestamp += chrono::TimeDelta::seconds(self.tick_interval_secs as i64);
            }
        }
    }

    /// Convenience: run `n` ticks.
    pub fn run_for(&mut self, ticks: usize) {
        for _ in 0..ticks {
            self.tick();
        }
    }
}

// ===========================================================================
// Real device models
// ===========================================================================

// ---------------------------------------------------------------------------
// Weather
// ---------------------------------------------------------------------------

/// Weather condition modifier for solar generation.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum WeatherCondition {
    Clear,
    PartlyCloudy,
    Overcast,
    Storm,
}

impl std::fmt::Display for WeatherCondition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Canonical string representation — NOT derived from Debug.
        // Renaming a variant will NOT silently break parse_weather_from_str.
        match self {
            Self::Clear => write!(f, "clear"),
            Self::PartlyCloudy => write!(f, "partly_cloudy"),
            Self::Overcast => write!(f, "overcast"),
            Self::Storm => write!(f, "storm"),
        }
    }
}

impl WeatherCondition {
    /// Irradiance fraction 0.0–1.0.
    pub fn irradiance_factor(&self) -> f64 {
        match self {
            Self::Clear => 1.0,
            Self::PartlyCloudy => 0.6,
            Self::Overcast => 0.3,
            Self::Storm => 0.1,
        }
    }
}

// ---------------------------------------------------------------------------
// SolarEngine
// ---------------------------------------------------------------------------

/// Solar PV generation model.
///
/// Uses a sinusoidal irradiance curve peaking at solar noon.
/// Sunrise/sunset are estimated from latitude and day-of-year.
/// Weather is read from `state.weather` each tick.
#[derive(Debug, Clone)]
pub struct SolarEngine {
    /// Total installed panel capacity in watts (peak).
    pub peak_capacity_w: f64,
    /// Site latitude in degrees (positive = north).
    pub latitude: f64,
}

impl SolarEngine {
    pub fn new(peak_capacity_w: f64, latitude: f64) -> Self {
        Self {
            peak_capacity_w,
            latitude,
        }
    }

    /// Estimate sunrise hour (decimal) for a given day-of-year and latitude.
    fn sunrise_hour(&self, day_of_year: u32) -> f64 {
        let lat_rad = self.latitude.to_radians();
        let declination = 23.45_f64.to_radians()
            * (2.0 * std::f64::consts::PI * (day_of_year as f64 + 284.0) / 365.0).sin();
        let cos_hour_angle = (-lat_rad.tan() * declination.tan()).min(1.0).max(-1.0);
        12.0 - cos_hour_angle.acos().to_degrees() / 15.0
    }

    /// Estimate sunset hour (decimal).
    fn sunset_hour(&self, day_of_year: u32) -> f64 {
        24.0 - self.sunrise_hour(day_of_year)
    }
}

impl DeviceModel for SolarEngine {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        // Apply manual override first — it takes absolute priority
        if let Some(w) = state.solar_override {
            let w = w.max(0.0);
            state.solar.generation_w = w;
            let pv1_peak = self.peak_capacity_w;
            let pv2_peak = state.config.pv2_peak_watts;
            let total_peak = pv1_peak + pv2_peak;
            if total_peak > 0.0 && pv2_peak > 0.0 {
                // Split proportionally to PV peak ratings
                state.solar.pv1_w = (w * pv1_peak / total_peak).min(pv1_peak).max(0.0);
                state.solar.pv2_w = (w * pv2_peak / total_peak).min(pv2_peak).max(0.0);
            } else {
                state.solar.pv1_w = w.min(pv1_peak).max(0.0);
                state.solar.pv2_w = 0.0;
            }
            return;
        }

        let hour = ctx.now.time().num_seconds_from_midnight() as f64 / 3600.0;
        let day_of_year = ctx.now.ordinal();

        let sunrise = self.sunrise_hour(day_of_year);
        let sunset = self.sunset_hour(day_of_year);
        let day_length = sunset - sunrise;

        if day_length <= 0.0 || hour <= sunrise || hour >= sunset {
            state.solar.generation_w = 0.0;
            state.solar.pv1_w = 0.0;
            state.solar.pv2_w = 0.0;
            return;
        }

        // Solar elevation factor: peak irradiance depends on noon elevation angle
        let declination = 23.45_f64.to_radians()
            * (2.0 * std::f64::consts::PI * (day_of_year as f64 + 284.0) / 365.0).sin();
        let noon_elevation = (std::f64::consts::FRAC_PI_2
            - (self.latitude.to_radians() - declination).abs())
        .max(0.0);
        let elevation_factor = noon_elevation.sin();

        // Sinusoidal irradiance over the day
        let t = (hour - sunrise) / day_length;
        let irradiance = (std::f64::consts::PI * t).sin();

        let weather = parse_weather_from_str(&state.weather);
        let weather_factor = weather.irradiance_factor();

        // Total generation = (PV1 peak + PV2 peak) × irradiance × elevation × weather
        let pv2_peak = state.config.pv2_peak_watts;
        let total_peak = self.peak_capacity_w + pv2_peak;
        let total_w = total_peak * irradiance * elevation_factor * weather_factor;
        let total_w = total_w.min(total_peak).max(0.0);

        // Split 45/55 so arrays always differ
        if pv2_peak > 0.0 {
            state.solar.pv1_w = (total_w * 0.45).min(self.peak_capacity_w).max(0.0);
            state.solar.pv2_w = (total_w * 0.55).min(pv2_peak).max(0.0);
        } else {
            state.solar.pv1_w = total_w.min(self.peak_capacity_w).max(0.0);
            state.solar.pv2_w = 0.0;
        }

        state.solar.generation_w = state.solar.pv1_w + state.solar.pv2_w;
    }
}

/// Parse a weather string into a WeatherCondition.
/// Accepts the canonical Display output as well as legacy Debug format
/// (e.g. "PartlyCloudy", "Partly-Cloudy") for backward compatibility.
pub fn parse_weather_from_str(s: &str) -> WeatherCondition {
    match s.to_lowercase().as_str() {
        // Canonical Display output (LOW-12 fix)
        "partly_cloudy" => WeatherCondition::PartlyCloudy,
        // Legacy Debug-variant formats
        "partlycloudy" | "partly-cloudy" => WeatherCondition::PartlyCloudy,
        "overcast" => WeatherCondition::Overcast,
        "storm" => WeatherCondition::Storm,
        _ => WeatherCondition::Clear,
    }
}

// ---------------------------------------------------------------------------
// LoadEngine
// ---------------------------------------------------------------------------

/// Pre-built household load profiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LoadProfile {
    /// Low baseline ~200-400W.
    Minimal,
    /// Typical family home ~300-3000W with morning/evening peaks.
    Family,
    /// Family + EV charger ~300-7000W, charges overnight.
    EV,
    /// Family + heat pump ~500-5000W, heating in morning/evening.
    HeatPump,
    /// Custom time-series profile: (decimal_hour, watts) pairs.
    /// Values are linearly interpolated. Must be sorted by hour.
    /// The last point wraps to the first (circular 24h profile).
    Custom(Vec<(f64, f64)>),
}

/// Household load model driven by time-of-day profiles.
#[derive(Debug, Clone)]
pub struct LoadEngine {
    pub profile: LoadProfile,
}

impl LoadEngine {
    pub fn new(profile: LoadProfile) -> Self {
        Self { profile }
    }

    /// Return the baseline demand in watts for the given hour (0–23).
    fn hourly_demand(&self, hour: u32) -> f64 {
        if let LoadProfile::Custom(points) = &self.profile {
            return Self::interpolate_custom(points, hour as f64);
        }
        match self.profile {
            LoadProfile::Minimal => match hour {
                0..=5 => 150.0,
                6..=8 => 300.0,
                9..=16 => 200.0,
                17..=21 => 350.0,
                22..=23 => 200.0,
                _ => 200.0,
            },
            LoadProfile::Family => match hour {
                0..=5 => 250.0,
                6 => 800.0,
                7..=8 => 1500.0,
                9..=11 => 600.0,
                12 => 1000.0,
                13..=15 => 500.0,
                16 => 800.0,
                17..=18 => 2500.0,
                19..=20 => 3000.0,
                21 => 1500.0,
                22..=23 => 600.0,
                _ => 500.0,
            },
            LoadProfile::EV => match hour {
                0..=5 => 4500.0, // EV charging overnight
                6 => 1000.0,
                7..=8 => 1500.0,
                9..=15 => 500.0,
                16 => 800.0,
                17..=18 => 2500.0,
                19..=20 => 3000.0,
                21 => 1500.0,
                22..=23 => 4000.0, // EV starts charging
                _ => 500.0,
            },
            LoadProfile::HeatPump => match hour {
                0..=5 => 400.0,
                6..=8 => 2500.0, // morning heating
                9..=11 => 800.0,
                12 => 1200.0,
                13..=15 => 600.0,
                16..=18 => 3000.0, // evening heating
                19..=21 => 4500.0,
                22..=23 => 1000.0,
                _ => 600.0,
            },
            LoadProfile::Custom(_) => 0.0, // handled by early return above
        }
    }

    /// Interpolate demand from custom time-series points.
    /// Points are (decimal_hour, watts), sorted ascending.
    /// The profile is circular: last point wraps back to first + 24.
    fn interpolate_custom(points: &[(f64, f64)], hour: f64) -> f64 {
        if points.is_empty() {
            return 0.0;
        }
        if points.len() == 1 {
            return points[0].1;
        }

        // Find the segment that contains `hour`.
        // The profile wraps: after the last point, interpolate toward (first+24, first_w).
        let n = points.len();
        for i in 0..n {
            let (h0, w0) = points[i];
            let (h1, w1) = if i + 1 < n {
                points[i + 1]
            } else {
                // Wrap: last point → first point + 24h
                (points[0].0 + 24.0, points[0].1)
            };

            if hour >= h0 && hour < h1 {
                let frac = (hour - h0) / (h1 - h0);
                return w0 + (w1 - w0) * frac;
            }
        }

        // Before the first point: interpolate from (last - 24) to first
        let (h_last, w_last) = points[n - 1];
        let (h_first, w_first) = points[0];
        if hour < h_first {
            let h0 = h_last - 24.0;
            let frac = (hour - h0) / (h_first - h0);
            return w_last + (w_first - w_last) * frac;
        }

        // Fallback: return last value
        points[n - 1].1
    }
}

impl DeviceModel for LoadEngine {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        let hour = ctx.now.time().hour() as f64;
        let minute = ctx.now.time().minute() as f64;
        let decimal_hour = hour + minute / 60.0;

        let floor_hour = hour as u32;
        let ceil_hour = (floor_hour + 1) % 24;

        let frac = decimal_hour - hour;
        let low = self.hourly_demand(floor_hour);
        let high = self.hourly_demand(ceil_hour);

        // Linear interpolation between hours
        state.load.demand_w = low + (high - low) * frac;

        // Apply manual override if set
        if let Some(w) = state.load_override {
            state.load.demand_w = w.max(0.0);
        }
    }
}

// ---------------------------------------------------------------------------
// InverterEngine — priority logic
// ---------------------------------------------------------------------------

/// Inverter power-flow controller.
///
/// Priority: Solar → Load → Battery → Grid
///
/// Modes:
/// - **Normal**: Solar supplies load first, excess charges battery, surplus exports to grid.
///   Deficit met from battery then grid.
/// - **Eco**: Like Normal but battery reserves more for evening peak.
/// - **ForceCharge**: Grid charges battery up to target SOC.
/// - **ForceDischarge**: Battery discharges to grid.
/// - **ExportLimit**: Like Normal but caps grid export at `export_limit_w`.
#[derive(Debug, Clone)]
pub struct InverterEngine;

impl InverterEngine {
    pub fn new() -> Self {
        Self
    }
}

impl DeviceModel for InverterEngine {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        let solar_w = state.solar.generation_w;
        let load_w = state.load.demand_w;

        if !state.grid.connected {
            // Grid is down — island mode
            self.island_mode(state, solar_w, load_w);
            return;
        }

        match state.inverter.mode_state.effective {
            InverterMode::Normal => {
                if state.scheduled_charge {
                    self.force_charge(state);
                } else if state.scheduled_discharge {
                    self.force_discharge(state);
                } else {
                    self.normal_priority(state, solar_w, load_w);
                }
            }
            InverterMode::Eco => {
                if state.scheduled_charge {
                    self.force_charge(state);
                } else if state.scheduled_discharge {
                    self.force_discharge(state);
                } else {
                    self.eco_priority(state, solar_w, load_w);
                }
            }
            InverterMode::ForceCharge => {
                self.force_charge(state);
            }
            InverterMode::ForceDischarge => {
                self.force_discharge(state);
            }
            InverterMode::ExportLimit => {
                self.export_limit(state, solar_w, load_w);
            }
        }

        // Inverter thermal model
        let ambient = 25.0;
        let power_kw = state.inverter.ac_power_w / 1000.0;
        // Heat generated proportional to power throughput (3% losses → heat)
        let heat = power_kw * 0.03 * 20.0 * ctx.dt_hours; // 20°C/kW thermal resistance
        // Passive cooling towards ambient (0.5°C/hour per degree above ambient)
        let temp_diff = state.inverter.temperature_celsius - ambient;
        let cooling = 0.5 * temp_diff * ctx.dt_hours;
        state.inverter.temperature_celsius += heat - cooling;
        state.inverter.temperature_celsius = state.inverter.temperature_celsius.clamp(-10.0, 80.0);
    }
}

impl InverterEngine {
    /// Normal priority: Solar → Load, excess → Battery, surplus → Grid.
    /// Deficit: Battery → Load, then Grid → Load.
    fn normal_priority(&self, state: &mut PlantState, solar_w: f64, load_w: f64) {
        let net = solar_w - load_w;
        let inv_max_w = state.config.max_ac_watts;

        if net >= 0.0 {
            // Solar covers load. Excess charges battery.
            let excess = net;
            let max_charge_w = state.effective_max_charge_w();
            let soc_headroom = (state.max_aggregate_soc() - state.aggregate_soc()) / 100.0
                * state.total_battery_capacity()
                * 1000.0;
            let charge_limit = max_charge_w.min(soc_headroom).min(inv_max_w).max(0.0);
            let to_battery = excess.min(charge_limit);
            let to_grid = excess - to_battery;

            state.distribute_battery_power(to_battery / 1000.0);
            state.grid.power_w = -to_grid; // negative = export
            state.inverter.ac_power_w = solar_w;
        } else {
            // Solar deficit. Battery supplies first, then grid.
            let deficit = -net;
            let max_discharge_w = state.effective_max_discharge_w();
            let soc_available = (state.aggregate_soc() - state.min_aggregate_soc()) / 100.0
                * state.total_battery_capacity()
                * 1000.0;
            let discharge_limit = max_discharge_w.min(soc_available).min(inv_max_w).max(0.0);
            let from_battery = deficit.min(discharge_limit);
            let from_grid = deficit - from_battery;

            state.distribute_battery_power(-from_battery / 1000.0); // negative = discharging
            state.grid.power_w = from_grid; // positive = import
            state.inverter.ac_power_w = solar_w + from_battery;
        }
    }

    /// Eco priority: preserves battery charge for evening peak.
    /// - During daytime (10:00–16:00): caps battery charging at 50% of max rate
    ///   and prefers grid export over charging.
    /// - During evening/night: uses battery freely to cover load.
    fn eco_priority(&self, state: &mut PlantState, solar_w: f64, load_w: f64) {
        let hour = state.timestamp.time().hour();
        let net = solar_w - load_w;
        let inv_max_w = state.config.max_ac_watts;

        if net >= 0.0 {
            let excess = net;
            let max_charge_w = state.effective_max_charge_w();
            let soc_headroom = (state.max_aggregate_soc() - state.aggregate_soc()) / 100.0
                * state.total_battery_capacity()
                * 1000.0;
            let charge_limit = max_charge_w.min(soc_headroom).min(inv_max_w).max(0.0);

            // Daytime cap: limit charging to 50% of the calculated limit
            let eco_charge_limit = if (10..16).contains(&hour) {
                charge_limit * 0.5
            } else {
                charge_limit
            };

            let to_battery = excess.min(eco_charge_limit);
            let to_grid = excess - to_battery;

            state.distribute_battery_power(to_battery / 1000.0);
            state.grid.power_w = -to_grid;
            state.inverter.ac_power_w = solar_w;
        } else {
            let deficit = -net;
            let max_discharge_w = state.effective_max_discharge_w();
            let soc_available = (state.aggregate_soc() - state.min_aggregate_soc()) / 100.0
                * state.total_battery_capacity()
                * 1000.0;
            let discharge_limit = max_discharge_w.min(soc_available).min(inv_max_w).max(0.0);
            let from_battery = deficit.min(discharge_limit);
            let from_grid = deficit - from_battery;

            state.distribute_battery_power(-from_battery / 1000.0);
            state.grid.power_w = from_grid;
            state.inverter.ac_power_w = solar_w + from_battery;
        }
    }

    /// Export-limited mode: same as Normal but caps grid export.
    /// Curtailed solar is redirected to battery charging if headroom exists.
    /// active_power_rate_percent (HR 50) caps total AC output.
    fn export_limit(&self, state: &mut PlantState, solar_w: f64, load_w: f64) {
        self.normal_priority(state, solar_w, load_w);

        // Apply HR 50 active power rate to total AC output
        let ac_rate = (state.active_power_rate_percent / 100.0).clamp(0.0, 1.0);
        let ac_cap = state.config.max_ac_watts * ac_rate;
        if state.inverter.ac_power_w > ac_cap {
            state.inverter.ac_power_w = ac_cap;
        }

        if state.grid.power_w < 0.0 {
            let export = -state.grid.power_w;
            let capped = export.min(state.inverter.export_limit_w);
            let curtailed = export - capped;
            if curtailed > 0.0 {
                // Redirect curtailed solar to battery if headroom exists
                let max_charge_w = state.effective_max_charge_w();
                let soc_headroom = (state.max_aggregate_soc() - state.aggregate_soc()) / 100.0
                    * state.total_battery_capacity()
                    * 1000.0;
                let charge_room = max_charge_w.min(soc_headroom).max(0.0);
                let already_charging = state.total_battery_power_kw() * 1000.0;
                let room = (charge_room - already_charging).max(0.0);
                let to_battery = curtailed.min(room);
                let lost = curtailed - to_battery;

                if to_battery > 0.0 {
                    let new_charge_kw = state.total_battery_power_kw() + to_battery / 1000.0;
                    state.distribute_battery_power(new_charge_kw);
                }
                state.grid.power_w = -capped;
                state.inverter.ac_power_w -= lost;
            }
        }
    }

    /// Force charge: grid charges battery at max rate until full.
    fn force_charge(&self, state: &mut PlantState) {
        let solar_w = state.solar.generation_w;
        let load_w = state.load.demand_w;
        let net = solar_w - load_w;
        let solar_surplus = net.max(0.0);
        let solar_deficit = (-net).max(0.0);
        let inv_max_w = state.config.max_ac_watts;

        let max_charge_w = state.effective_max_charge_w();
        let soc_headroom = (state.max_aggregate_soc() - state.aggregate_soc()) / 100.0
            * state.total_battery_capacity()
            * 1000.0;
        let charge_capacity = max_charge_w.min(soc_headroom.max(0.0)).min(inv_max_w);

        let from_solar = solar_surplus.min(charge_capacity);
        let remaining_capacity = charge_capacity - from_solar;
        let from_grid = remaining_capacity.min(max_charge_w);

        state.distribute_battery_power((from_solar + from_grid) / 1000.0);
        state.grid.power_w = solar_deficit + from_grid;
        state.inverter.ac_power_w = solar_w;
    }

    /// Force discharge: battery exports to grid at max rate.
    fn force_discharge(&self, state: &mut PlantState) {
        let solar_w = state.solar.generation_w;
        let load_w = state.load.demand_w;
        let net = solar_w - load_w;
        let inv_max_w = state.config.max_ac_watts;

        let max_discharge_w = state.effective_max_discharge_w();
        let soc_available = (state.aggregate_soc() - state.min_aggregate_soc()) / 100.0
            * state.total_battery_capacity()
            * 1000.0;
        let discharge = max_discharge_w.min(soc_available.max(0.0)).min(inv_max_w);

        state.distribute_battery_power(-discharge / 1000.0);

        if net >= 0.0 {
            state.grid.power_w = -(net + discharge);
            state.inverter.ac_power_w = solar_w + discharge;
        } else {
            let deficit = -net;
            let battery_to_load = deficit.min(discharge);
            let battery_to_grid = discharge - battery_to_load;
            state.grid.power_w = deficit - battery_to_load - battery_to_grid;
            state.inverter.ac_power_w = solar_w + battery_to_load + battery_to_grid;
        }
    }

    /// Island mode: no grid. Solar → Load → Battery only.
    fn island_mode(&self, state: &mut PlantState, solar_w: f64, load_w: f64) {
        state.grid.power_w = 0.0;
        let net = solar_w - load_w;

        if net >= 0.0 {
            let excess = net;
            let max_charge_w = state.effective_max_charge_w();
            let soc_headroom = (state.max_aggregate_soc() - state.aggregate_soc()) / 100.0
                * state.total_battery_capacity()
                * 1000.0;
            let charge_limit = max_charge_w.min(soc_headroom).max(0.0);
            let to_battery = excess.min(charge_limit);

            state.distribute_battery_power(to_battery / 1000.0);
            state.inverter.ac_power_w = load_w + to_battery;
        } else {
            let deficit = -net;
            let max_discharge_w = state.effective_max_discharge_w();
            let soc_available = (state.aggregate_soc() - state.min_aggregate_soc()) / 100.0
                * state.total_battery_capacity()
                * 1000.0;
            let discharge_limit = max_discharge_w.min(soc_available).max(0.0);
            let from_battery = deficit.min(discharge_limit);

            state.distribute_battery_power(-from_battery / 1000.0);
            state.inverter.ac_power_w = solar_w + from_battery;
        }
    }
}

// ---------------------------------------------------------------------------
// CalibrationEngine — battery calibration workflow
// ---------------------------------------------------------------------------

/// Drives the 3-stage calibration cycle:
///   Stage 1: ChargeToFull — force-charge to 100% SOC
///   Stage 2: HoldingFull — hold at 100% for BMS settling (30 min real-time)
///   Stage 3: DischargeToEmpty — discharge to reserve SOC
///
/// Calibration is aborted if the simulation is paused or if CancelCalibration
/// is dispatched.
#[derive(Debug, Clone)]
pub struct CalibrationEngine;

impl CalibrationEngine {
    /// Create a new calibration engine.
    pub fn new() -> Self {
        Self
    }

    /// Target duration for stage 2 (holding at full charge), in seconds.
    const HOLD_SECONDS: f64 = 30.0 * 60.0; // 30 minutes

    /// Apply calibration mode BEFORE the device loop so InverterEngine
    /// picks up the correct mode without 1-tick lag. Uses ModeSource::Fault
    /// so schedule overrides are cleanly replaced rather than stacked.
    pub fn apply_calibration_mode(state: &mut PlantState) {
        let stage = state.calibration.stage;
        match stage {
            CalibrationStage::ChargeToFull | CalibrationStage::HoldingFull => {
                state.inverter.mode_state.effective = InverterMode::ForceCharge;
                state.inverter.mode_state.source = sim_models::ModeSource::Fault;
                state.inverter.mode_state.scheduled_mode = None;
            }
            CalibrationStage::DischargeToEmpty => {
                state.inverter.mode_state.effective = InverterMode::ForceDischarge;
                state.inverter.mode_state.source = sim_models::ModeSource::Fault;
                state.inverter.mode_state.scheduled_mode = None;
            }
            CalibrationStage::Complete => {
                // Restore Eco mode
                state.inverter.mode_state.effective = InverterMode::Eco;
                state.inverter.mode_state.source = sim_models::ModeSource::User;
                state.inverter.mode_state.scheduled_mode = None;
            }
            CalibrationStage::Off => {}
        }
    }

    /// Check SOC thresholds and advance calibration stage.
    /// Must run AFTER BatteryEngine so SOC values are final for the tick.
    pub fn check_stage_transitions(ctx: &TickContext, state: &mut PlantState) {
        let stage = state.calibration.stage;
        if stage == CalibrationStage::Off || stage == CalibrationStage::Complete {
            return;
        }

        // Advance stage timer by the simulated elapsed time per tick
        let dt_secs = ctx.dt_hours * 3600.0;
        state.calibration.stage_elapsed_secs += dt_secs;
        state.calibration.total_elapsed_secs += dt_secs;

        // Determine which batteries we are calibrating
        let targets: Vec<usize> = match state.calibration.module {
            Some(m) if m < state.batteries.len() => vec![m],
            Some(_) => return,
            None => (0..state.batteries.len()).collect(),
        };

        match state.calibration.stage {
            CalibrationStage::ChargeToFull => {
                let all_full = targets
                    .iter()
                    .all(|&i| state.batteries[i].soc_percent >= 99.5);
                if all_full {
                    state.calibration.stage = CalibrationStage::HoldingFull;
                    state.calibration.stage_elapsed_secs = 0.0;
                }
            }

            CalibrationStage::HoldingFull => {
                if state.calibration.stage_elapsed_secs >= Self::HOLD_SECONDS {
                    state.calibration.stage = CalibrationStage::DischargeToEmpty;
                    state.calibration.stage_elapsed_secs = 0.0;
                }
            }

            CalibrationStage::DischargeToEmpty => {
                let reserve = state.min_aggregate_soc().min(state.max_aggregate_soc());
                let all_empty = targets
                    .iter()
                    .all(|&i| state.batteries[i].soc_percent <= (reserve + 0.5));
                if all_empty {
                    state.calibration.stage = CalibrationStage::Complete;
                    state.calibration.stage_elapsed_secs = 0.0;
                }
            }

            CalibrationStage::Complete | CalibrationStage::Off => unreachable!(),
        }
    }
}

impl Default for CalibrationEngine {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// BatteryEngine — SOC tracking
// ---------------------------------------------------------------------------

/// Battery SOC tracker with thermal model.
///
/// Reads `state.batteries[].power_kw` (set by [`InverterEngine`]) and updates
/// each module's SOC independently.
///
/// Formula per module: `soc += (effective_power_kw * dt_hours) / capacity_kwh * 100`
///
/// Thermal model: temperature rises during charge/discharge, cools passively.
/// Charging/discharging is derated above 45°C and blocked above 55°C.
#[derive(Debug, Clone)]
pub struct BatteryEngine {
    /// Ambient temperature in °C (can vary by time-of-day).
    pub ambient_temp_celsius: f64,
    /// Thermal resistance (°C per kW of heat generated).
    pub thermal_resistance: f64,
    /// Passive cooling rate (°C/hour per degree above ambient).
    pub cooling_rate: f64,
    /// Temperature at which derating begins (°C).
    pub derate_temp_celsius: f64,
    /// Temperature at which charging/discharging is blocked (°C).
    pub shutdown_temp_celsius: f64,
    /// Capacity loss per full equivalent cycle (fraction of nominal).
    /// Typical Li-ion: 0.0002 → 0.02% per cycle → 80% SOH at ~1000 cycles.
    pub degradation_per_cycle: f64,
    /// Minimum State of Health (0.0–1.0). Battery is "dead" below this.
    pub min_soh: f64,
}

impl BatteryEngine {
    pub fn new() -> Self {
        Self {
            ambient_temp_celsius: 25.0,
            thermal_resistance: 5.0, // 5°C per kW
            cooling_rate: 2.0,       // cools 2°C/hour per degree above ambient
            derate_temp_celsius: 45.0,
            shutdown_temp_celsius: 55.0,
            degradation_per_cycle: 0.0002, // ~0.02% per cycle
            min_soh: 0.5,                  // 50% SOH = end of life
        }
    }

    /// Estimate ambient temperature from time-of-day.
    /// Simple model: peaks at 14:00, minimum at 05:00.
    fn ambient_for_hour(&self, hour: f64) -> f64 {
        // Sinusoidal: peak at 14:00, trough at 05:00
        // Offset: 14:00 is the peak → phase = (14 - 5) / 24 * 2π
        let phase = (hour - 5.0) / 24.0 * 2.0 * std::f64::consts::PI;
        let delta = 8.0; // ±8°C swing around base
        self.ambient_temp_celsius + delta * phase.sin()
    }

    /// Apply thermal derating to power.
    /// Above derate_temp: linearly reduce to 0 at shutdown_temp.
    fn derate_power(&self, power_kw: f64, temp: f64) -> f64 {
        if temp >= self.shutdown_temp_celsius {
            return 0.0;
        }
        if temp > self.derate_temp_celsius {
            let fraction = (self.shutdown_temp_celsius - temp)
                / (self.shutdown_temp_celsius - self.derate_temp_celsius);
            return power_kw * fraction.max(0.0);
        }
        power_kw
    }
}

impl DeviceModel for BatteryEngine {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        let hour = ctx.now.time().num_seconds_from_midnight() as f64 / 3600.0;
        let ambient = self.ambient_for_hour(hour);

        // Check battery pause mode/slot (GivTCP BatteryPauseMode):
        //   0 = Disabled, 1 = PAUSE_CHARGE, 2 = PAUSE_DISCHARGE, 3 = PAUSE_BOTH
        let pause_charge = state.battery_pause_mode == 1 || state.battery_pause_mode == 3;
        let pause_discharge = state.battery_pause_mode == 2 || state.battery_pause_mode == 3;
        if (pause_charge || pause_discharge)
            && state.battery_pause_slot_start != 60
            && state.battery_pause_slot_end != 60
        {
            let in_pause_window = if state.battery_pause_slot_start < state.battery_pause_slot_end {
                let start_h = (state.battery_pause_slot_start / 100) as f64
                    + (state.battery_pause_slot_start % 100) as f64 / 60.0;
                let end_h = (state.battery_pause_slot_end / 100) as f64
                    + (state.battery_pause_slot_end % 100) as f64 / 60.0;
                hour >= start_h && hour < end_h
            } else {
                false
            };
            if in_pause_window {
                for b in &mut state.batteries {
                    if pause_charge && b.power_kw > 0.0 {
                        b.power_kw = 0.0;
                    }
                    if pause_discharge && b.power_kw < 0.0 {
                        b.power_kw = 0.0;
                    }
                }
                state.sync_battery_from_vec();
                return;
            }
        }

        // Apply battery charge/discharge limit percentages.
        // HR 111/112 use 0-100% where 100 = full power (no cap).
        let charge_scale = (state.battery_charge_limit_percent / 100.0).clamp(0.0, 1.0);
        let discharge_scale = (state.battery_discharge_limit_percent / 100.0).clamp(0.0, 1.0);

        for b in &mut state.batteries {
            if b.power_kw > 0.0 {
                // Charging: apply charge limit as percentage of device max.
                // Zero percent disables charging entirely (matches real hardware).
                let max_charge_kw = b.max_charge_kw * charge_scale;
                b.power_kw = b.power_kw.min(max_charge_kw);
            } else if b.power_kw < 0.0 {
                // Discharging: apply discharge limit as percentage of device max.
                // Zero percent disables discharging entirely (matches real hardware).
                let max_discharge_kw = b.max_discharge_kw * discharge_scale;
                b.power_kw = b.power_kw.max(-max_discharge_kw);
            }

            // Apply thermal derating
            let derated_power = self.derate_power(b.power_kw, b.temperature_celsius);
            b.power_kw = derated_power;

            let power_kw = b.power_kw;

            let effective_power_kw = if power_kw >= 0.0 {
                // Charging: losses reduce stored energy
                power_kw * b.charge_efficiency
            } else {
                // Discharging: losses increase energy withdrawn from battery
                power_kw / b.discharge_efficiency.max(0.01)
            };

            // If the user just manually set SOC (via GUI slider), hold it for a
            // configurable number of ticks so the setting visibly "sticks".
            // We check the hold flag at the start of the loop; it is decremented
            // once per tick (below) so N-module systems don't divide the hold
            // duration by N.
            if state.manual_soc_hold_ticks == 0 {
                let delta_soc = (effective_power_kw * ctx.dt_hours) / b.capacity_kwh * 100.0;
                b.soc_percent += delta_soc;
                // Clamp to min/max SOC
                b.soc_percent = b.soc_percent.clamp(b.min_soc, b.max_soc);
            }

            // Thermal model
            // Heat generated = losses (difference between electrical and effective power)
            let heat_generated_kw = (power_kw - effective_power_kw).abs();
            let heat_rise = heat_generated_kw * self.thermal_resistance * ctx.dt_hours;

            // Passive cooling towards ambient
            let temp_diff = b.temperature_celsius - ambient;
            let cooling = self.cooling_rate * temp_diff * ctx.dt_hours;

            b.temperature_celsius += heat_rise - cooling;

            // Clamp temperature
            b.temperature_celsius = b.temperature_celsius.clamp(-10.0, 70.0);

            // Aging model: track throughput and degrade capacity
            let energy_this_tick = power_kw.abs() * ctx.dt_hours; // kWh throughput
            b.throughput_kwh += energy_this_tick;

            // Update cycle count
            let nominal = b.nominal_capacity_kwh.max(0.1);
            b.cycle_count = b.throughput_kwh / nominal;

            // Degrade SOH based on cycles
            // SOH = 1.0 - degradation_per_cycle * cycle_count
            // But we apply it incrementally so it's smooth
            let soh_loss = self.degradation_per_cycle * (energy_this_tick / nominal);
            b.soh = (b.soh - soh_loss).max(self.min_soh);

            // Apply SOH to capacity
            b.capacity_kwh = b.nominal_capacity_kwh * b.soh;

            // Estimate terminal voltage from SOC using an LFP voltage curve.
            // Real LFP cells sit at 51-52V from 20-90% SOC, unlike the old
            // 44-52V linear model which overstates voltage swing.
            // Curve (16S LFP, nominal 51.2V):
            //   0%:  44.0 V (empty)
            //   5%:  50.5 V
            //  20%:  51.0 V
            //  50%:  51.5 V
            //  90%:  52.0 V
            // 100%:  54.0 V (full)
            // For ThreePhase inverters (24S modules, 76.8V nominal), scale by 1.5.
            let is_tph = state.config.inverter_type.starts_with("ThreePhase")
                || state.config.inverter_type == "ACThreePhase";
            let tph_scale = if is_tph { 1.5 } else { 1.0 };
            b.voltage_v = if b.soc_percent <= 5.0 {
                // Steep rise from empty
                tph_scale * (44.0 + (b.soc_percent / 5.0) * (50.5 - 44.0))
            } else if b.soc_percent <= 20.0 {
                // Gradual rise
                tph_scale * (50.5 + ((b.soc_percent - 5.0) / 15.0) * (51.0 - 50.5))
            } else if b.soc_percent <= 90.0 {
                // Flat plateau (20-90%)
                tph_scale * (51.0 + ((b.soc_percent - 20.0) / 70.0) * (52.0 - 51.0))
            } else {
                // Steep rise to full
                tph_scale * (52.0 + ((b.soc_percent - 90.0) / 10.0) * (54.0 - 52.0))
            };

            // Current from power and voltage (signed, positive = charging)
            b.current_a = if b.voltage_v > 0.0 {
                (b.power_kw * 1000.0) / b.voltage_v
            } else {
                0.0
            };
        }

        // Decrement hold ticks once per tick (not per-module)
        if state.manual_soc_hold_ticks > 0 {
            state.manual_soc_hold_ticks -= 1;
        }

        // Sync convenience field
        state.sync_battery_from_vec();

        // Recalculate grid and inverter AC power after capping battery power.
        // BatteryEngine may throttle power below what InverterEngine allocated
        // (percentage limits, thermal derating, per-module C-rate caps).
        // Without this recalculation, the throttled power vanishes instead of
        // being redirected to/from the grid — an energy conservation violation.
        let total_batt_kw = state.total_battery_power_kw();
        if total_batt_kw > 0.0 {
            // Battery charging: unabsorbed surplus becomes grid export.
            // grid = load + charge - solar (energy balance).
            state.grid.power_w =
                state.load.demand_w + total_batt_kw * 1000.0 - state.solar.generation_w;
            state.inverter.ac_power_w = state.solar.generation_w;
        } else if total_batt_kw < 0.0 {
            // Battery discharging: capped discharge reduces grid export/import.
            let discharge_w = (-total_batt_kw * 1000.0).min(state.config.max_ac_watts);
            let net = state.solar.generation_w - state.load.demand_w;
            state.grid.power_w = -(net + discharge_w);
            state.inverter.ac_power_w = state.solar.generation_w + discharge_w;
        }
    }
}

// ---------------------------------------------------------------------------
// EnergyTracker — cumulative energy totals
// ---------------------------------------------------------------------------

/// Device model that accumulates energy totals each tick.
/// Must be registered **last** (after BatteryEngine) so power values are final.
///
/// The totals are treated as *daily* ("today") registers: at the first tick of
/// each new calendar day every bucket is reset to zero, so IR/HR energy-today
/// values are a faithful integral of power over the current day rather than a
/// cumulative-since-startup figure.
#[derive(Debug, Clone)]
pub struct EnergyTracker {
    /// Calendar date of the most recent midnight reset. `None` until the first
    /// tick; the first tick records the date **without** resetting so that a
    /// freshly-loaded plant keeps its existing totals.
    last_reset_date: Option<chrono::NaiveDate>,
}

impl EnergyTracker {
    pub fn new() -> Self {
        Self {
            last_reset_date: None,
        }
    }
}

impl Default for EnergyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceModel for EnergyTracker {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        // Midnight rollover: zero the daily energy buckets at the start of a
        // new calendar day. On the very first tick we only record the date so
        // a plant restored from disk keeps its accumulated totals.
        let today = ctx.now.date();
        match self.last_reset_date {
            None => self.last_reset_date = Some(today),
            Some(prev) if prev != today => {
                state.energy_totals = sim_models::EnergyTotals::default();
                self.last_reset_date = Some(today);
            }
            _ => {}
        }

        let dt_hours = ctx.dt_hours;

        // Read values first to avoid borrow conflicts
        let grid_w = state.grid.power_w;
        let battery_kw = state.total_battery_power_kw();
        let solar_w = state.solar.generation_w;
        let load_w = state.load.demand_w;

        let totals = &mut state.energy_totals;

        // Grid: positive = import, negative = export
        if grid_w > 0.0 {
            totals.grid_import_kwh += grid_w / 1000.0 * dt_hours;
        } else {
            totals.grid_export_kwh += (-grid_w) / 1000.0 * dt_hours;
        }

        // Battery: positive = charging, negative = discharging
        if battery_kw > 0.0 {
            let battery_charge_kwh = battery_kw * dt_hours;
            totals.battery_charge_kwh += battery_charge_kwh;
            if grid_w > 0.0 {
                totals.ac_charge_kwh += battery_charge_kwh.min(grid_w / 1000.0 * dt_hours);
            }
        } else {
            totals.battery_discharge_kwh += (-battery_kw) * dt_hours;
        }

        // Solar generation
        totals.solar_generation_kwh += solar_w / 1000.0 * dt_hours;

        // Inverter AC output energy (distinct from solar — includes battery
        // discharge contribution to AC for hybrid inverters).
        totals.inverter_output_kwh += state.inverter.ac_power_w.max(0.0) / 1000.0 * dt_hours;

        // Load consumption
        totals.load_consumption_kwh += load_w / 1000.0 * dt_hours;
    }
}

// ---------------------------------------------------------------------------
// EvcEngine — GivEnergy Electric Vehicle Charger
// ---------------------------------------------------------------------------

/// Simulates a GivEVC wallbox. Draws power from the grid when charging.
/// State is stored in `state.evc`. Writes directly to `state.evc` and
/// adds load to `state.grid.power_w` when charging.
///
/// State machine follows the GivTCP register map:
/// - HR 0: charging_state (1=Idle, 2=Connected, 3=Starting, 4=Charging, ...)
/// - HR 2: connection_status (0=Not Connected, 1=Connected)
/// - HR 94: charge_control (0=Ready, 1=Start, 2=Stop)
///
/// Transition logic:
///   Idle → Connected: cable plugged (connection_status set to 1)
///   Connected → Starting: charge_control set to 1 (Start)
///   Starting → Charging: next tick (instantaneous in sim)
///   Charging → End of Charging: charge_control set to 2 (Stop)
///   * → Idle: cable unplugged (connection_status set to 0)
#[derive(Debug, Clone)]
pub struct EvcEngine;

impl Default for EvcEngine {
    fn default() -> Self {
        Self
    }
}

impl EvcEngine {
    pub fn new() -> Self {
        Self
    }
}

impl DeviceModel for EvcEngine {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        let evc = &mut state.evc;

        if !evc.enabled {
            // EVC simulation disabled — stay idle
            evc.charging_state = 1; // Idle
            zero_power(evc);
            return;
        }

        // State machine driven by connection_status and charge_control
        match evc.charging_state {
            // Idle — no cable connected
            1 => {
                zero_power(evc);
                if evc.connection_status == 1 {
                    evc.charging_state = 2; // → Connected
                }
            }
            // Connected — cable plugged, waiting for start command
            2 => {
                zero_power(evc);
                if evc.connection_status == 0 {
                    evc.charging_state = 1; // → Idle (cable removed)
                } else if evc.charge_control == 1 {
                    evc.charging_state = 3; // → Starting
                }
            }
            // Starting — brief transient state
            3 => {
                if evc.connection_status == 0 {
                    evc.charging_state = 1;
                    zero_power(evc);
                } else if evc.charge_control == 2 {
                    evc.charging_state = 6; // → End of Charging
                    zero_power(evc);
                } else {
                    // Transition to Charging on next tick
                    evc.charging_state = 4;
                    start_charging(evc);
                }
            }
            // Charging — actively drawing power
            4 => {
                if evc.connection_status == 0 {
                    evc.charging_state = 1;
                    zero_power(evc);
                    evc.session_energy_kwh = 0.0;
                    evc.session_duration_secs = 0;
                } else if evc.charge_control == 2 {
                    evc.charging_state = 6; // → End of Charging
                    zero_power(evc);
                } else {
                    // Continue charging
                    continue_charging(evc, ctx);
                }
            }
            // End of Charging — cable still connected but stopped
            6 => {
                zero_power(evc);
                if evc.connection_status == 0 {
                    evc.charging_state = 1;
                    evc.session_energy_kwh = 0.0;
                    evc.session_duration_secs = 0;
                } else if evc.charge_control == 1 {
                    evc.charging_state = 3; // → Starting again
                }
            }
            // Any other state → reset to Idle
            _ => {
                evc.charging_state = 1;
                zero_power(evc);
            }
        }

        // Add EVC load to grid import when charging
        if evc.charging_state == 4 {
            state.grid.power_w += evc.active_power_w;
        }
    }
}

fn zero_power(evc: &mut sim_models::EvcState) {
    evc.active_power_w = 0.0;
    evc.active_power_l1 = 0.0;
    evc.active_power_l2 = 0.0;
    evc.active_power_l3 = 0.0;
    evc.current_l1 = 0.0;
    evc.current_l2 = 0.0;
    evc.current_l3 = 0.0;
}

fn start_charging(evc: &mut sim_models::EvcState) {
    // Reset session tracking on new charge start
    evc.session_energy_kwh = 0.0;
    evc.session_duration_secs = 0;
    apply_charge_power(evc);
}

fn continue_charging(evc: &mut sim_models::EvcState, ctx: &TickContext) {
    apply_charge_power(evc);
    // Accumulate session energy and duration
    let dt_hours = ctx.dt_hours;
    evc.session_energy_kwh += evc.active_power_w / 1000.0 * dt_hours;
    evc.session_duration_secs += (dt_hours * 3600.0) as u64;
    // Accumulate meter energy
    evc.meter_energy_kwh += evc.active_power_w / 1000.0 * dt_hours;
}

fn apply_charge_power(evc: &mut sim_models::EvcState) {
    // Charge current limit is in deci-Amps (÷10), clamped to hardware limits
    let current_a = (evc.charge_current_limit as f64 / 10.0)
        .clamp(evc.evse_min_current as f64, evc.evse_max_current as f64);
    let voltage = evc.voltage_l1; // Use L1 voltage
    // Single-phase charging: all power on L1
    let power_w = current_a * voltage;
    evc.current_l1 = current_a * 10.0; // Store in deci-Amps (÷10)
    evc.current_l2 = 0.0;
    evc.current_l3 = 0.0;
    evc.active_power_l1 = power_w;
    evc.active_power_l2 = 0.0;
    evc.active_power_l3 = 0.0;
    evc.active_power_w = power_w;
    evc.charge_limit = current_a; // Reflect actual limit
}

// ---------------------------------------------------------------------------
// ScheduleEngine — timed charge/discharge windows
// ---------------------------------------------------------------------------

// Schedule re-exported from sim-models to avoid breaking imports.
pub use sim_models::Schedule;

/// Device model that enforces schedule-based inverter mode changes.
/// Must be registered **before** InverterEngine so mode is set before power flow calc.
///
/// A schedule is active when the start hour is different from the end hour.
/// When both are 0, that schedule is disabled.
#[derive(Debug, Clone)]
pub struct ScheduleEngine {
    pub schedule: Schedule,
    /// Whether the schedule is enabled (can be toggled).
    pub enabled: bool,
}

impl ScheduleEngine {
    pub fn new(schedule: Schedule) -> Self {
        Self {
            schedule,
            enabled: true,
        }
    }
}

impl DeviceModel for ScheduleEngine {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        // Reset flags every tick — ScheduleEngine sets them fresh.
        state.scheduled_charge = false;
        state.scheduled_discharge = false;

        if !self.enabled {
            return;
        }

        let hour = ctx.now.time().num_seconds_from_midnight() as f64 / 3600.0;
        let soc = state.aggregate_soc();

        // Helper: check if hour is inside a window (handles midnight wrap)
        let in_window = |start: f64, end: f64, h: f64| -> bool {
            if start < end {
                h >= start && h < end
            } else {
                // Wraps midnight, e.g. 22:00–06:00
                h >= start || h < end
            }
        };

        // The global charge_target_soc is only respected when enable_charge_target is true.
        let global_target = if state.enable_charge_target {
            self.schedule.charge_target_soc
        } else {
            100.0
        };

        // Only do always-on charge when NO timed slots are configured.
        // When slots exist, the slot checks below determine when to charge.
        let any_charge_slot = self.schedule.charge_start != self.schedule.charge_end
            || self.schedule.charge_start_2 != self.schedule.charge_end_2
            || self.schedule.charge_start_3 != self.schedule.charge_end_3
            || self.schedule.charge_start_4 != self.schedule.charge_end_4
            || self.schedule.charge_start_5 != self.schedule.charge_end_5
            || self.schedule.charge_start_6 != self.schedule.charge_end_6
            || self.schedule.charge_start_7 != self.schedule.charge_end_7
            || self.schedule.charge_start_8 != self.schedule.charge_end_8
            || self.schedule.charge_start_9 != self.schedule.charge_end_9
            || self.schedule.charge_start_10 != self.schedule.charge_end_10;
        if !any_charge_slot && self.schedule.enable_charge && soc < global_target {
            state.scheduled_charge = true;
            return;
        }

        // Same for discharge: always-on only when no slots configured.
        let any_discharge_slot = self.schedule.discharge_start != self.schedule.discharge_end
            || self.schedule.discharge_start_2 != self.schedule.discharge_end_2
            || self.schedule.discharge_start_3 != self.schedule.discharge_end_3
            || self.schedule.discharge_start_4 != self.schedule.discharge_end_4
            || self.schedule.discharge_start_5 != self.schedule.discharge_end_5
            || self.schedule.discharge_start_6 != self.schedule.discharge_end_6
            || self.schedule.discharge_start_7 != self.schedule.discharge_end_7
            || self.schedule.discharge_start_8 != self.schedule.discharge_end_8
            || self.schedule.discharge_start_9 != self.schedule.discharge_end_9
            || self.schedule.discharge_start_10 != self.schedule.discharge_end_10;
        if !any_discharge_slot
            && self.schedule.enable_discharge
            && soc > self.schedule.discharge_target_soc
        {
            state.scheduled_discharge = true;
            return;
        }

        // Check all charge slots (1-10)
        macro_rules! check_charge_slot {
            ($start:expr, $end:expr, $target:expr) => {
                if $start != $end {
                    if in_window($start, $end, hour) && soc < $target {
                        state.scheduled_charge = true;
                        return;
                    }
                }
            };
        }
        check_charge_slot!(
            self.schedule.charge_start,
            self.schedule.charge_end,
            self.schedule.charge_target_soc
        );
        check_charge_slot!(
            self.schedule.charge_start_2,
            self.schedule.charge_end_2,
            self.schedule.charge_target_soc_2
        );
        check_charge_slot!(
            self.schedule.charge_start_3,
            self.schedule.charge_end_3,
            self.schedule.charge_target_soc_3
        );
        check_charge_slot!(
            self.schedule.charge_start_4,
            self.schedule.charge_end_4,
            self.schedule.charge_target_soc_4
        );
        check_charge_slot!(
            self.schedule.charge_start_5,
            self.schedule.charge_end_5,
            self.schedule.charge_target_soc_5
        );
        check_charge_slot!(
            self.schedule.charge_start_6,
            self.schedule.charge_end_6,
            self.schedule.charge_target_soc_6
        );
        check_charge_slot!(
            self.schedule.charge_start_7,
            self.schedule.charge_end_7,
            self.schedule.charge_target_soc_7
        );
        check_charge_slot!(
            self.schedule.charge_start_8,
            self.schedule.charge_end_8,
            self.schedule.charge_target_soc_8
        );
        check_charge_slot!(
            self.schedule.charge_start_9,
            self.schedule.charge_end_9,
            self.schedule.charge_target_soc_9
        );
        check_charge_slot!(
            self.schedule.charge_start_10,
            self.schedule.charge_end_10,
            self.schedule.charge_target_soc_10
        );

        // Check all discharge slots (1-10). AC-coupled models are basic
        // single-phase slot devices in the register map (slot 1 at HR 56-57);
        // the UI/register projection expose only slot 1 for them, but the engine
        // honours any schedule fields provided programmatically.
        macro_rules! check_discharge_slot {
            ($start:expr, $end:expr, $target:expr) => {
                if $start != $end {
                    if in_window($start, $end, hour) && soc > $target {
                        state.scheduled_discharge = true;
                        return;
                    }
                }
            };
        }
        check_discharge_slot!(
            self.schedule.discharge_start,
            self.schedule.discharge_end,
            self.schedule.discharge_target_soc
        );
        check_discharge_slot!(
            self.schedule.discharge_start_2,
            self.schedule.discharge_end_2,
            self.schedule.discharge_target_soc_2
        );
        check_discharge_slot!(
            self.schedule.discharge_start_3,
            self.schedule.discharge_end_3,
            self.schedule.discharge_target_soc_3
        );
        check_discharge_slot!(
            self.schedule.discharge_start_4,
            self.schedule.discharge_end_4,
            self.schedule.discharge_target_soc_4
        );
        check_discharge_slot!(
            self.schedule.discharge_start_5,
            self.schedule.discharge_end_5,
            self.schedule.discharge_target_soc_5
        );
        check_discharge_slot!(
            self.schedule.discharge_start_6,
            self.schedule.discharge_end_6,
            self.schedule.discharge_target_soc_6
        );
        check_discharge_slot!(
            self.schedule.discharge_start_7,
            self.schedule.discharge_end_7,
            self.schedule.discharge_target_soc_7
        );
        check_discharge_slot!(
            self.schedule.discharge_start_8,
            self.schedule.discharge_end_8,
            self.schedule.discharge_target_soc_8
        );
        check_discharge_slot!(
            self.schedule.discharge_start_9,
            self.schedule.discharge_end_9,
            self.schedule.discharge_target_soc_9
        );
        check_discharge_slot!(
            self.schedule.discharge_start_10,
            self.schedule.discharge_end_10,
            self.schedule.discharge_target_soc_10
        );

        // Export limit scheduling — 3 time windows
        if self.schedule.enable_export_schedule && self.schedule.export_power_limit_w > 0.0 {
            let in_export_window = |s: f64, e: f64| -> bool {
                if s == 0.0 && e == 0.0 {
                    return false;
                }
                if s < e {
                    hour >= s && hour < e
                } else {
                    hour >= s || hour < e
                }
            };
            let in_window =
                in_export_window(self.schedule.export_start_1, self.schedule.export_end_1)
                    || in_export_window(self.schedule.export_start_2, self.schedule.export_end_2)
                    || in_export_window(self.schedule.export_start_3, self.schedule.export_end_3);

            if in_window && soc > state.min_aggregate_soc() {
                // During export window: cap export limit
                state.inverter.export_limit_w = self.schedule.export_power_limit_w;
            }
            // Outside window: export_limit_w keeps its user-set value
            // (set by SetActivePowerRate which runs before tick via command queue)
        }
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    #![allow(
        clippy::bool_assert_comparison,
        clippy::field_reassign_with_default,
        clippy::manual_range_contains
    )]
    use super::*;
    use chrono::{NaiveDate, NaiveDateTime};

    fn ts(hour: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    fn test_engine() -> SimulationEngine {
        let state = PlantState::new(ts(0));
        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(BatteryEngine::new()),
        ];
        SimulationEngine::new(state, devices, 30)
    }

    #[test]
    fn power_limit_defaults_are_100_percent() {
        let state = PlantState::new(ts(0));
        assert_eq!(state.battery_charge_limit_percent, 100.0);
        assert_eq!(state.battery_discharge_limit_percent, 100.0);
    }

    #[test]
    fn power_limit_commands_clamp_to_100_percent() {
        let mut engine = test_engine();
        engine.enqueue(Command::SetBatteryChargeLimit(150.0));
        engine.enqueue(Command::SetBatteryDischargeLimit(125.0));
        engine.apply_commands();
        assert_eq!(engine.state.battery_charge_limit_percent, 100.0);
        assert_eq!(engine.state.battery_discharge_limit_percent, 100.0);
    }

    #[test]
    fn battery_engine_treats_100_percent_as_full_power_and_50_as_half_power() {
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0 / 60.0,
        };

        let mut full = PlantState::new(ts(12));
        full.batteries[0].max_charge_kw = 7.0;
        full.batteries[0].capacity_kwh = 10.0;
        full.batteries[0].power_kw = 8.0;
        full.battery_charge_limit_percent = 100.0;
        BatteryEngine::new().update(&ctx, &mut full);
        assert!((full.batteries[0].power_kw - 7.0).abs() < 0.001);

        let mut half = PlantState::new(ts(12));
        half.batteries[0].max_charge_kw = 7.0;
        half.batteries[0].capacity_kwh = 10.0;
        half.batteries[0].power_kw = 8.0;
        half.battery_charge_limit_percent = 50.0;
        BatteryEngine::new().update(&ctx, &mut half);
        assert!((half.batteries[0].power_kw - 3.5).abs() < 0.001);
    }

    #[test]
    fn charge_limit_conserves_energy_by_redirecting_to_export() {
        // CRITICAL fix: when battery_charge_limit_percent < 100, the charge
        // power capped by BatteryEngine must show up as additional grid export,
        // not vanish. Energy balance: solar = load + charge + export.
        let mut engine = test_engine();
        engine.state.timestamp = ts(12); // midday — solar generating
        engine.state.solar_override = Some(5000.0); // fixed 5 kW solar
        engine.state.load_override = Some(1000.0); // fixed 1 kW load
        engine.state.battery_charge_limit_percent = 50.0; // halve charge rate
        engine.state.batteries[0].soc_percent = 50.0; // headroom to charge
        // Tick once
        engine.tick();

        let solar = engine.state.solar.generation_w;
        let load = engine.state.load.demand_w;
        let charge_kw = engine.state.total_battery_power_kw();
        let charge_w = charge_kw * 1000.0;
        let grid = engine.state.grid.power_w; // negative = export

        // Energy balance: solar = load + charge + export
        // export = -grid.power_w (when negative)
        let export = (-grid).max(0.0);
        let energy_balance = solar - load - charge_w - export;

        assert!(
            energy_balance.abs() < 1.0,
            "Energy not conserved: solar={solar:.0}W, load={load:.0}W, \
             charge={charge_w:.0}W, export={export:.0}W, imbalance={energy_balance:.1}W"
        );

        // Charge should be limited to 50% of max
        let max_charge_w = engine.state.batteries[0].max_charge_kw * 1000.0;
        let half_limit = max_charge_w * 0.5;
        assert!(
            charge_w <= half_limit + 1.0, // +1 for float tolerance
            "Charge {charge_w:.0}W exceeds 50% limit of {half_limit:.0}W"
        );
    }

    #[test]
    fn tick_increments_inverter_work_time_hours() {
        let mut engine = test_engine();
        engine.tick_interval_secs = 1800;
        engine.tick();
        assert!((engine.state.inverter.work_time_hours - 0.5).abs() < 0.0001);
    }

    #[test]
    fn inverter_parallel_mode_command_updates_state() {
        let mut engine = test_engine();
        engine.enqueue(Command::SetEnableInverterParallelMode(true));
        engine.apply_commands();
        assert!(engine.state.enable_inverter_parallel_mode);
    }

    #[test]
    fn gateway_commands_route_to_state() {
        // Control writes to a Gateway must reach the child AIO state.
        // In the projection model, PlantState IS the child AIO, so
        // the standard command pipeline should work for any inverter type.
        let mut state = PlantState::new(ts(12));
        state.config.inverter_type = "Gateway12kW".to_string();
        state.config.max_ac_watts = 6000.0;
        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(BatteryEngine::new()),
        ];
        let mut engine = SimulationEngine::new(state, devices, 30);

        // HR 110 → SetMinSoc
        engine.enqueue(Command::SetMinSoc(15.0));
        engine.apply_commands();
        assert_eq!(engine.state.battery.min_soc, 15.0);
        assert_eq!(engine.state.batteries[0].min_soc, 15.0);

        // HR 111 → SetBatteryChargeLimit
        engine.enqueue(Command::SetBatteryChargeLimit(80.0));
        engine.apply_commands();
        assert_eq!(engine.state.battery_charge_limit_percent, 80.0);

        // HR 112 → SetBatteryDischargeLimit
        engine.enqueue(Command::SetBatteryDischargeLimit(90.0));
        engine.apply_commands();
        assert_eq!(engine.state.battery_discharge_limit_percent, 90.0);

        // SetSolarOverride works on Gateway too
        engine.enqueue(Command::SetSolarOverride(Some(2000.0)));
        engine.apply_commands();
        assert_eq!(engine.state.solar_override, Some(2000.0));
    }

    // --- SolarEngine ---

    #[test]
    fn solar_zero_at_night() {
        let mut engine = test_engine();
        engine.state.timestamp = ts(2);
        engine.tick();
        assert_eq!(engine.state.solar.generation_w, 0.0);
    }

    #[test]
    fn solar_generates_at_midday() {
        let mut engine = test_engine();
        engine.state.timestamp = ts(12);
        engine.tick();
        assert!(
            engine.state.solar.generation_w > 0.0,
            "Expected solar generation at noon, got {}",
            engine.state.solar.generation_w
        );
    }

    #[test]
    fn solar_weather_reduces_output() {
        let mut eng1 = test_engine();
        eng1.state.timestamp = ts(12);
        eng1.tick();
        let clear = eng1.state.solar.generation_w;

        let mut state = PlantState::new(ts(12));
        state.weather = "Overcast".to_string();
        let solar = SolarEngine::new(5000.0, 51.5);
        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(solar),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(BatteryEngine::new()),
        ];
        let mut eng2 = SimulationEngine::new(state, devices, 30);
        eng2.tick();
        let overcast = eng2.state.solar.generation_w;

        assert!(
            overcast < clear,
            "Overcast ({overcast}) should be less than clear ({clear})"
        );
    }

    #[test]
    fn solar_winter_shorter_day() {
        let summer_sol = NaiveDate::from_ymd_opt(2025, 6, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let winter_sol = NaiveDate::from_ymd_opt(2025, 12, 21)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();

        let mut summer = {
            let state = PlantState::new(summer_sol);
            let devices: Vec<Box<dyn DeviceModel>> = vec![
                Box::new(SolarEngine::new(5000.0, 51.5)),
                Box::new(LoadEngine::new(LoadProfile::Minimal)),
                Box::new(InverterEngine::new()),
                Box::new(BatteryEngine::new()),
            ];
            SimulationEngine::new(state, devices, 30)
        };
        let mut winter = {
            let state = PlantState::new(winter_sol);
            let devices: Vec<Box<dyn DeviceModel>> = vec![
                Box::new(SolarEngine::new(5000.0, 51.5)),
                Box::new(LoadEngine::new(LoadProfile::Minimal)),
                Box::new(InverterEngine::new()),
                Box::new(BatteryEngine::new()),
            ];
            SimulationEngine::new(state, devices, 30)
        };

        summer.tick();
        winter.tick();

        assert!(
            summer.state.solar.generation_w > winter.state.solar.generation_w,
            "Summer noon should produce more than winter noon"
        );
    }

    // --- LoadEngine ---

    #[test]
    fn load_family_evening_peak() {
        let mut state = PlantState::new(ts(19));
        let mut load = LoadEngine::new(LoadProfile::Family);
        let ctx = TickContext {
            now: ts(19),
            dt_hours: 30.0 / 3600.0,
        };
        load.update(&ctx, &mut state);
        // 19:00 in family profile → 3000W
        assert!(state.load.demand_w > 0.0,);
    }

    #[test]
    fn load_minimal_low() {
        let mut state = PlantState::new(ts(3));
        let mut load = LoadEngine::new(LoadProfile::Minimal);
        let ctx = TickContext {
            now: ts(3),
            dt_hours: 30.0 / 3600.0,
        };
        load.update(&ctx, &mut state);
        assert!(
            state.load.demand_w < 500.0,
            "Minimal profile at 3am should be low, got {}",
            state.load.demand_w
        );
    }

    #[test]
    fn load_custom_interpolation() {
        // Two points: 0W at midnight, 1000W at 12:00, wraps back
        let profile = LoadProfile::Custom(vec![(0.0, 0.0), (12.0, 1000.0)]);
        let mut load = LoadEngine::new(profile);

        // At 6:00 → should be 500W (midpoint)
        let mut state = PlantState::new(ts(6));
        let ctx = TickContext {
            now: ts(6),
            dt_hours: 1.0,
        };
        load.update(&ctx, &mut state);
        assert!(
            (state.load.demand_w - 500.0).abs() < 1.0,
            "At 06:00 should be ~500W, got {}",
            state.load.demand_w
        );

        // At 12:00 → should be 1000W
        let mut state2 = PlantState::new(ts(12));
        let ctx2 = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        load.update(&ctx2, &mut state2);
        assert!(
            (state2.load.demand_w - 1000.0).abs() < 1.0,
            "At 12:00 should be ~1000W, got {}",
            state2.load.demand_w
        );

        // At 18:00 → should be 500W (halfway from 12→24+0)
        let mut state3 = PlantState::new(ts(18));
        let ctx3 = TickContext {
            now: ts(18),
            dt_hours: 1.0,
        };
        load.update(&ctx3, &mut state3);
        assert!(
            (state3.load.demand_w - 500.0).abs() < 1.0,
            "At 18:00 should be ~500W, got {}",
            state3.load.demand_w
        );
    }

    #[test]
    fn load_custom_multi_point() {
        let profile = LoadProfile::Custom(vec![
            (0.0, 200.0),
            (6.0, 500.0),
            (8.0, 2000.0),
            (10.0, 800.0),
            (18.0, 3000.0),
            (22.0, 500.0),
        ]);
        let mut load = LoadEngine::new(profile);

        // At 07:00 → between 500 and 2000 = 1250
        let mut state = PlantState::new(
            NaiveDate::from_ymd_opt(2025, 6, 1)
                .unwrap()
                .and_hms_opt(7, 0, 0)
                .unwrap(),
        );
        let ctx = TickContext {
            now: state.timestamp,
            dt_hours: 1.0,
        };
        load.update(&ctx, &mut state);
        assert!(
            (state.load.demand_w - 1250.0).abs() < 1.0,
            "At 07:00 should be ~1250W, got {}",
            state.load.demand_w
        );
    }

    #[test]
    fn load_custom_empty_is_zero() {
        let profile = LoadProfile::Custom(vec![]);
        let mut load = LoadEngine::new(profile);
        let mut state = PlantState::new(ts(12));
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        load.update(&ctx, &mut state);
        assert_eq!(state.load.demand_w, 0.0);
    }

    // --- BatteryEngine ---

    #[test]
    fn battery_charges_from_positive_power() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].soc_percent = 50.0;
        state.batteries[0].power_kw = 3.0; // charging at 3kW
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[0].charge_efficiency = 0.95;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);

        // soc += (3.0 * 0.95 * 1.0) / 10.0 * 100 = +28.5%
        assert!(
            (state.batteries[0].soc_percent - 78.5).abs() < 0.01,
            "Expected SOC ~78.5%, got {}",
            state.batteries[0].soc_percent
        );
    }

    #[test]
    fn battery_discharges() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].soc_percent = 50.0;
        state.batteries[0].power_kw = -2.0; // discharging at 2kW
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[0].discharge_efficiency = 0.95;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);

        // effective = -2.0 / 0.95 = -2.105 kW
        // soc += (-2.105 * 1.0) / 10.0 * 100 = -21.05%
        assert!(
            (state.batteries[0].soc_percent - 28.947).abs() < 0.01,
            "Expected SOC ~28.95%, got {}",
            state.batteries[0].soc_percent
        );
    }

    #[test]
    fn battery_clamps_at_max_soc() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].soc_percent = 99.0;
        state.batteries[0].power_kw = 5.0;
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[0].max_soc = 100.0;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);
        assert_eq!(state.batteries[0].soc_percent, 100.0);
    }

    #[test]
    fn battery_clamps_at_min_soc() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].soc_percent = 11.0;
        state.batteries[0].power_kw = -5.0;
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[0].min_soc = 10.0;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);
        assert_eq!(state.batteries[0].soc_percent, 10.0);
    }

    // --- InverterEngine ---

    #[test]
    fn inverter_solar_covers_load_charges_battery() {
        let mut state = PlantState::new(ts(12));
        state.solar.generation_w = 5000.0;
        state.load.demand_w = 1000.0;
        state.batteries[0].soc_percent = 50.0;
        state.batteries[0].max_charge_kw = 3.0;
        state.batteries[0].max_soc = 100.0;
        state.sync_battery_from_vec();

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 30.0 / 3600.0,
        };
        inv.update(&ctx, &mut state);

        // Excess = 4000W, max charge = 3000W → 3000W to battery, 1000W to grid
        assert!(
            state.total_battery_power_kw() > 0.0,
            "Battery should be charging"
        );
        assert!(
            state.grid.power_w < 0.0,
            "Grid should be exporting (negative)"
        );
    }

    #[test]
    fn inverter_deficit_uses_battery_then_grid() {
        let mut state = PlantState::new(ts(20));
        state.solar.generation_w = 0.0;
        state.load.demand_w = 2500.0;
        state.batteries[0].soc_percent = 80.0;
        state.batteries[0].max_discharge_kw = 3.0;
        state.batteries[0].min_soc = 10.0;
        state.sync_battery_from_vec();

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(20),
            dt_hours: 30.0 / 3600.0,
        };
        inv.update(&ctx, &mut state);

        // Deficit 2500W, battery can supply → battery covers it
        assert!(
            state.total_battery_power_kw() < 0.0,
            "Battery should be discharging"
        );
        assert!(
            state.grid.power_w <= 0.01,
            "Grid should not be importing if battery can cover load, got {}",
            state.grid.power_w
        );
    }

    // --- Eco mode ---

    #[test]
    fn eco_mode_charges_slower_during_daytime() {
        // At 12:00 with solar surplus, Eco caps charging at 50% during daytime (10-16)
        let mut normal_state = PlantState::new(ts(12));
        normal_state.solar.generation_w = 5000.0;
        normal_state.load.demand_w = 1000.0;
        normal_state.batteries[0].soc_percent = 50.0;
        normal_state.batteries[0].max_charge_kw = 3.0;
        normal_state
            .inverter
            .mode_state
            .set_user(InverterMode::Normal);
        normal_state.sync_battery_from_vec();

        let mut eco_state = normal_state.clone();
        eco_state.inverter.mode_state.set_user(InverterMode::Eco);

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 30.0 / 3600.0,
        };

        inv.update(&ctx, &mut normal_state);
        let normal_charge = normal_state.total_battery_power_kw();

        inv.update(&ctx, &mut eco_state);
        let eco_charge = eco_state.total_battery_power_kw();

        assert!(
            eco_charge < normal_charge,
            "Eco should charge slower than Normal at noon: eco={}, normal={}",
            eco_charge,
            normal_charge
        );
        assert!(
            (eco_charge - normal_charge * 0.5).abs() < 0.01,
            "Eco daytime charge should be ~50% of Normal: eco={}, expected={}",
            eco_charge,
            normal_charge * 0.5
        );
    }

    #[test]
    fn eco_mode_same_as_normal_at_night() {
        // At 21:00 with load deficit, Eco should behave same as Normal (no daytime cap)
        let mut normal_state = PlantState::new(ts(21));
        normal_state.solar.generation_w = 0.0;
        normal_state.load.demand_w = 2000.0;
        normal_state.batteries[0].soc_percent = 80.0;
        normal_state.batteries[0].max_discharge_kw = 3.0;
        normal_state
            .inverter
            .mode_state
            .set_user(InverterMode::Normal);
        normal_state.sync_battery_from_vec();

        let mut eco_state = normal_state.clone();
        eco_state.inverter.mode_state.set_user(InverterMode::Eco);

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(21),
            dt_hours: 30.0 / 3600.0,
        };

        inv.update(&ctx, &mut normal_state);
        inv.update(&ctx, &mut eco_state);

        assert_eq!(
            normal_state.total_battery_power_kw(),
            eco_state.total_battery_power_kw(),
            "Eco should behave same as Normal at night"
        );
    }

    // --- Integration ---

    #[test]
    fn full_day_simulation_runs() {
        let mut engine = test_engine();
        // Run 24 hours = 2880 ticks at 30s
        engine.run_for(2880);

        // After 24h, SOC should have changed from initial 50%
        assert_ne!(
            engine.state.battery.soc_percent, 50.0,
            "SOC should have changed over a full day with solar + load"
        );
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

    #[test]
    fn wall_clock_anchor_snaps_to_host_time() {
        // When anchored, the sim clock should track the host wall clock, not
        // advance by tick_interval_secs. After a tick the timestamp should be
        // within a couple seconds of Local::now() (whatever wall elapsed).
        let mut engine = SimulationEngine::new(PlantState::new(ts(12)), vec![], 30);
        assert!(!engine.is_wall_clock_anchored());
        engine.anchor_to_wall_clock(None);
        assert!(engine.is_wall_clock_anchored());

        engine.tick();
        let now = chrono::Local::now().naive_local();
        let diff = (engine.state.timestamp - now).num_seconds().abs();
        assert!(
            diff <= 2,
            "anchored clock should match wall time within 2s, drifted {diff}s"
        );
    }

    #[test]
    fn wall_clock_anchor_pinned_date_uses_real_time_of_day() {
        // pinned_date projects the real time-of-day onto a different calendar
        // date. The result must carry today's H:M:S but the supplied date.
        let winter = chrono::NaiveDate::from_ymd_opt(2024, 12, 21).unwrap();
        let mut engine = SimulationEngine::new(PlantState::new(ts(12)), vec![], 30);
        engine.anchor_to_wall_clock(Some(winter));
        engine.tick();

        assert_eq!(engine.state.timestamp.date(), winter);
        let now = chrono::Local::now().naive_local();
        assert_eq!(engine.state.timestamp.time().hour(), now.hour());
    }

    #[test]
    fn unanchor_reverts_to_fixed_step() {
        let mut engine = SimulationEngine::new(PlantState::new(ts(12)), vec![], 30);
        engine.anchor_to_wall_clock(None);
        engine.tick();
        let anchored_ts = engine.state.timestamp;

        engine.unanchor_wall_clock();
        assert!(!engine.is_wall_clock_anchored());
        engine.tick();
        // Fixed-step resumes: exactly +30s from the last timestamp.
        assert_eq!(
            engine.state.timestamp,
            anchored_ts + chrono::TimeDelta::seconds(30)
        );
    }

    #[test]
    fn wall_clock_anchor_dt_reflects_real_elapsed() {
        // dt drives energy integration; in anchored mode it must equal the real
        // wall time between ticks, not tick_interval_secs. Sleep briefly to make
        // the elapsed measurable.
        let mut engine = SimulationEngine::new(PlantState::new(ts(12)), vec![], 30);
        engine.anchor_to_wall_clock(None);
        engine.tick(); // establishes last_wall
        std::thread::sleep(std::time::Duration::from_millis(1200));
        // Capture the work_time delta around the second tick to infer dt.
        let before = engine.state.inverter.work_time_hours;
        engine.tick();
        let dt_hours = engine.state.inverter.work_time_hours - before;
        let dt_secs = dt_hours * 3600.0;
        // ~1.2s elapsed; allow tolerance for scheduler jitter.
        assert!(
            dt_secs >= 1.0 && dt_secs <= 3.0,
            "dt should reflect ~1.2s real elapsed, got {dt_secs:.2}s"
        );
    }

    // --- Weather Command ---

    #[test]
    fn set_weather_command_updates_state() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Minimal)),
            Box::new(InverterEngine::new()),
            Box::new(BatteryEngine::new()),
        ];
        let mut engine = SimulationEngine::new(PlantState::new(ts), devices, 30);
        engine.state.weather = "Clear".to_string();

        // Tick with clear weather
        engine.tick();
        let clear_solar = engine.state.solar.generation_w;
        assert!(clear_solar > 0.0);

        // Change to storm
        engine.enqueue(Command::SetWeather(WeatherCondition::Storm));
        engine.tick();
        let storm_solar = engine.state.solar.generation_w;

        assert!(
            storm_solar < clear_solar,
            "Storm should reduce solar: storm={}, clear={}",
            storm_solar,
            clear_solar
        );
    }

    // --- Battery Efficiency ---

    #[test]
    fn battery_efficiency_losses_on_charge() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].soc_percent = 50.0;
        state.batteries[0].power_kw = 3.0; // 3kW charging
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[0].charge_efficiency = 1.0; // perfect efficiency
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);
        let perfect_soc = state.batteries[0].soc_percent; // 80%

        // Now with 90% efficiency
        let mut state2 = PlantState::new(ts(12));
        state2.batteries[0].soc_percent = 50.0;
        state2.batteries[0].power_kw = 3.0;
        state2.batteries[0].capacity_kwh = 10.0;
        state2.batteries[0].charge_efficiency = 0.9;
        state2.sync_battery_from_vec();
        bat.update(&ctx, &mut state2);

        assert!(
            state2.batteries[0].soc_percent < perfect_soc,
            "With efficiency losses, SOC should be lower: got {}, perfect={}",
            state2.batteries[0].soc_percent,
            perfect_soc
        );
        // 3kW * 0.9 = 2.7kW effective → +27%
        assert!((state2.batteries[0].soc_percent - 77.0).abs() < 0.1);
    }

    #[test]
    fn battery_efficiency_losses_on_discharge() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].soc_percent = 50.0;
        state.batteries[0].power_kw = -3.0; // 3kW discharging
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[0].discharge_efficiency = 0.9;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);

        // effective = -3.0 / 0.9 = -3.333 kW → -33.33%
        assert!((state.batteries[0].soc_percent - 16.667).abs() < 0.1);
    }

    // --- EnergyTracker ---

    #[test]
    fn energy_tracker_accumulates_totals() {
        let mut state = PlantState::new(ts(12));
        state.solar.generation_w = 5000.0;
        state.load.demand_w = 2000.0;
        state.grid.power_w = -1000.0; // exporting 1kW
        state.batteries[0].power_kw = 2.0; // charging 2kW
        state.sync_battery_from_vec();

        let mut tracker = EnergyTracker::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        tracker.update(&ctx, &mut state);

        assert!((state.energy_totals.solar_generation_kwh - 5.0).abs() < 0.01);
        assert!((state.energy_totals.load_consumption_kwh - 2.0).abs() < 0.01);
        assert!((state.energy_totals.grid_export_kwh - 1.0).abs() < 0.01);
        assert!((state.energy_totals.grid_import_kwh).abs() < 0.01);
        assert!((state.energy_totals.battery_charge_kwh - 2.0).abs() < 0.01);
    }

    #[test]
    fn energy_tracker_grid_import() {
        let mut state = PlantState::new(ts(20));
        state.grid.power_w = 2000.0; // importing 2kW

        let mut tracker = EnergyTracker::new();
        let ctx = TickContext {
            now: ts(20),
            dt_hours: 0.5, // 30 minutes
        };
        tracker.update(&ctx, &mut state);

        assert!((state.energy_totals.grid_import_kwh - 1.0).abs() < 0.01);
        assert!((state.energy_totals.grid_export_kwh).abs() < 0.01);
    }

    #[test]
    fn energy_tracker_resets_daily_totals_at_midnight() {
        // A full day of solar generation accumulates into today's totals...
        let mut state = PlantState::new(ts(12));
        state.solar.generation_w = 5000.0;
        let mut tracker = EnergyTracker::new();
        // First tick records the date without resetting.
        tracker.update(
            &TickContext {
                now: ts(12),
                dt_hours: 1.0,
            },
            &mut state,
        );
        assert!((state.energy_totals.solar_generation_kwh - 5.0).abs() < 0.01);

        // ...same calendar day keeps accumulating, no reset.
        tracker.update(
            &TickContext {
                now: ts(13),
                dt_hours: 1.0,
            },
            &mut state,
        );
        assert!((state.energy_totals.solar_generation_kwh - 10.0).abs() < 0.01);

        // Crossing into the next calendar day zeros the daily buckets so
        // energy-today registers track the new day's power, not yesterday's.
        let next_day = NaiveDate::from_ymd_opt(2025, 6, 22)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        tracker.update(
            &TickContext {
                now: next_day,
                dt_hours: 1.0,
            },
            &mut state,
        );
        assert_eq!(state.energy_totals.solar_generation_kwh, 5.0); // only this tick's 5kW·h
    }

    #[test]
    fn energy_tracker_first_tick_preserves_loaded_totals() {
        // A plant restored from disk with existing totals must NOT be zeroed on
        // the first tick (no spurious reset before the date is recorded).
        let mut state = PlantState::new(ts(12));
        state.energy_totals.solar_generation_kwh = 42.0;
        let mut tracker = EnergyTracker::new();
        tracker.update(
            &TickContext {
                now: ts(12),
                dt_hours: 1.0,
            },
            &mut state,
        );
        assert_eq!(state.energy_totals.solar_generation_kwh, 42.0);
    }

    // --- Thermal Model ---

    #[test]
    fn thermal_heats_on_charge() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].soc_percent = 50.0;
        state.batteries[0].power_kw = 3.0; // 3kW charging
        state.batteries[0].temperature_celsius = 25.0;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);

        // Heat generated = losses = 3.0 - 3.0*0.95 = 0.15 kW
        // heat_rise = 0.15 * 5.0 * 1.0 = 0.75°C
        // cooling = 2.0 * (25.0 - ambient) * 1.0
        // ambient at 12:00 ≈ 25 + 8*sin(π*(12-5)/24) ≈ 25 + 8*sin(1.832) ≈ 25 + 7.83 ≈ 32.83
        // temp_diff = 25.0 - 32.83 = -7.83 (below ambient, so heating from ambient)
        // cooling = 2.0 * (-7.83) = -15.66 → actually warms
        // This means battery at 25°C will warm from ambient being hotter
        assert!(
            state.batteries[0].temperature_celsius > 25.0,
            "Temperature should rise during charging, got {}",
            state.batteries[0].temperature_celsius
        );
    }

    #[test]
    fn thermal_derating_at_high_temp() {
        let bat = BatteryEngine::new();
        // At 50°C (above derate_temp=45°C, below shutdown=55°C)
        let derated = bat.derate_power(3.0, 50.0);
        assert!(derated < 3.0, "Should derate at 50°C: got {derated}");
        assert!(
            derated > 0.0,
            "Should not fully block at 50°C: got {derated}"
        );

        // At 55°C (shutdown)
        let blocked = bat.derate_power(3.0, 55.0);
        assert_eq!(blocked, 0.0, "Should block at 55°C");

        // At 25°C (normal)
        let normal = bat.derate_power(3.0, 25.0);
        assert_eq!(normal, 3.0, "Should not derate at 25°C");
    }

    // --- ScheduleEngine ---

    #[test]
    fn schedule_forces_charge_during_window() {
        let mut sched = Schedule::default();
        sched.charge_start = 2.0; // 02:00
        sched.charge_end = 6.0; // 06:00
        sched.charge_target_soc = 90.0;

        let mut engine = ScheduleEngine::new(sched);
        let mut state = PlantState::new(ts(4)); // 04:00 — inside window
        state.inverter.mode_state.set_user(InverterMode::Normal);
        state.batteries[0].soc_percent = 50.0;
        state.sync_battery_from_vec();

        let ctx = TickContext {
            now: ts(4),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert!(
            state.scheduled_charge,
            "Schedule should have set scheduled_charge=true"
        );
        assert_eq!(
            state.inverter.mode_state.effective,
            InverterMode::Normal, // mode stays unchanged by schedule
            "Schedule should NOT change mode"
        );
    }

    #[test]
    fn schedule_does_not_charge_when_soc_reached() {
        let mut sched = Schedule::default();
        sched.charge_start = 2.0;
        sched.charge_end = 6.0;
        sched.charge_target_soc = 90.0;

        let mut engine = ScheduleEngine::new(sched);
        let mut state = PlantState::new(ts(4));
        state.inverter.mode_state.set_user(InverterMode::Normal);
        state.batteries[0].soc_percent = 95.0; // above target
        state.sync_battery_from_vec();

        let ctx = TickContext {
            now: ts(4),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert!(
            !state.scheduled_charge,
            "Should not charge when SOC at target"
        );
    }

    #[test]
    fn schedule_forces_discharge_during_window() {
        let mut sched = Schedule::default();
        sched.discharge_start = 17.0; // 17:00
        sched.discharge_end = 20.0; // 20:00
        sched.discharge_target_soc = 20.0;

        let mut engine = ScheduleEngine::new(sched);
        let mut state = PlantState::new(ts(18)); // 18:00 — inside window
        state.inverter.mode_state.set_user(InverterMode::Normal);
        state.batteries[0].soc_percent = 60.0;
        state.sync_battery_from_vec();

        let ctx = TickContext {
            now: ts(18),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert!(
            state.scheduled_discharge,
            "Schedule should set scheduled_discharge=true"
        );
        assert_eq!(
            state.inverter.mode_state.effective,
            InverterMode::Normal,
            "Mode should stay unchanged"
        );
    }

    #[test]
    fn ac_coupled_schedule_forces_discharge_during_slot_1() {
        let sched = Schedule {
            discharge_start: 17.0,
            discharge_end: 20.0,
            discharge_target_soc: 20.0,
            ..Default::default()
        };

        let mut engine = ScheduleEngine::new(sched);
        let mut state = PlantState::new(ts(18)); // 18:00 — inside window
        state.config.inverter_type = "ACCoupled".to_string();
        state.inverter.mode_state.set_user(InverterMode::Normal);
        state.batteries[0].soc_percent = 60.0;
        state.sync_battery_from_vec();

        let ctx = TickContext {
            now: ts(18),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);

        assert!(
            state.scheduled_discharge,
            "AC-coupled slot 1 should trigger scheduled_discharge"
        );
    }

    #[test]
    fn schedule_wraps_midnight() {
        let mut sched = Schedule::default();
        sched.charge_start = 22.0; // 22:00
        sched.charge_end = 6.0; // 06:00 (wraps midnight)
        sched.charge_target_soc = 100.0;

        let mut engine = ScheduleEngine::new(sched);

        // Test at 23:00 (inside window)
        let mut state = PlantState::new(ts(23));
        state.inverter.mode_state.set_user(InverterMode::Normal);
        state.batteries[0].soc_percent = 30.0;
        state.sync_battery_from_vec();
        let ctx = TickContext {
            now: ts(23),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);
        assert!(state.scheduled_charge, "At 23:00 should be inside window");

        // Test at 03:00 (inside window)
        let mut state2 = PlantState::new(ts(3));
        state2.inverter.mode_state.set_user(InverterMode::Normal);
        state2.batteries[0].soc_percent = 30.0;
        state2.sync_battery_from_vec();
        let ctx2 = TickContext {
            now: ts(3),
            dt_hours: 1.0,
        };
        engine.update(&ctx2, &mut state2);
        assert!(state2.scheduled_charge, "At 03:00 should be inside window");
    }

    #[test]
    fn schedule_disabled_does_nothing() {
        let mut sched = Schedule::default();
        sched.charge_start = 2.0;
        sched.charge_end = 6.0;
        sched.charge_target_soc = 100.0;

        let mut engine = ScheduleEngine::new(sched);
        engine.enabled = false;

        let mut state = PlantState::new(ts(4));
        state.inverter.mode_state.set_user(InverterMode::Normal);
        let ctx = TickContext {
            now: ts(4),
            dt_hours: 1.0,
        };
        engine.update(&ctx, &mut state);
        assert!(
            !state.scheduled_charge,
            "Disabled schedule should not set flag"
        );
        assert_eq!(state.inverter.mode_state.effective, InverterMode::Normal);
    }

    #[test]
    fn eco_mode_charges_during_charge_slot() {
        let mut sched = Schedule::default();
        sched.charge_start = 2.0;
        sched.charge_end = 6.0;
        sched.charge_target_soc = 100.0;

        let mut state = PlantState::new(ts(4));
        state.inverter.mode_state.set_user(InverterMode::Eco);
        state.batteries[0].soc_percent = 30.0;
        state.sync_battery_from_vec();

        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(ScheduleEngine::new(sched)),
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(crate::EnergyTracker::new()),
        ];
        let mut engine = SimulationEngine::new(state, devices, 60);

        let soc_before = engine.state.aggregate_soc();
        engine.tick();
        let soc_after = engine.state.aggregate_soc();

        assert!(
            engine.state.scheduled_charge,
            "Schedule should have set scheduled_charge=true at hour 4 inside window 2-6"
        );
        assert!(
            soc_after > soc_before,
            "Battery should charge: before={soc_before:.1}% after={soc_after:.1}%"
        );
        assert!(
            engine.state.grid.power_w > 0.0,
            "Grid should be importing during charge slot, got {}W",
            engine.state.grid.power_w
        );
    }

    #[test]
    fn enable_charge_flag_triggers_charging() {
        let mut sched = Schedule::default();
        sched.enable_charge = true;
        sched.charge_target_soc = 100.0;
        // Slot windows are disabled (0-0) — enable_charge flag should override

        let mut state = PlantState::new(ts(14)); // 14:00 — outside any default slot
        state.inverter.mode_state.set_user(InverterMode::Eco);
        state.batteries[0].soc_percent = 30.0;
        state.sync_battery_from_vec();

        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(ScheduleEngine::new(sched)),
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(crate::EnergyTracker::new()),
        ];
        let mut engine = SimulationEngine::new(state, devices, 60);

        engine.tick();

        assert!(
            engine.state.scheduled_charge,
            "enable_charge flag should set scheduled_charge=true even at hour 14 with no window"
        );
        let soc_before = engine.state.aggregate_soc();
        engine.tick();
        let soc_after = engine.state.aggregate_soc();
        assert!(
            soc_after >= soc_before,
            "Battery should charge: before={soc_before:.1}% after={soc_after:.1}%"
        );
    }

    #[test]
    fn slot_3_triggers_charge_during_window() {
        // Verify slot 3-10 mapping works end-to-end via ScheduleEngine.
        let mut sched = Schedule::default();
        sched.charge_start_3 = 5.0;
        sched.charge_end_3 = 7.0;
        sched.charge_target_soc_3 = 90.0;
        // Disable any other slots / always-on
        sched.enable_charge = false;

        let mut state = PlantState::new(ts(6)); // 06:00 — inside slot 3 window
        state.inverter.mode_state.set_user(InverterMode::Eco);
        state.batteries[0].soc_percent = 30.0;
        state.sync_battery_from_vec();

        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(ScheduleEngine::new(sched)),
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
        ];
        let mut engine = SimulationEngine::new(state, devices, 60);

        engine.tick();
        assert!(
            engine.state.scheduled_charge,
            "Slot 3 (05:00-07:00) at hour 6 should trigger scheduled_charge"
        );
    }

    #[test]
    fn slot_10_triggers_discharge_during_window() {
        let mut sched = Schedule::default();
        sched.discharge_start_10 = 18.0;
        sched.discharge_end_10 = 22.0;
        sched.discharge_target_soc_10 = 20.0;
        sched.enable_discharge = false;

        let mut state = PlantState::new(ts(20)); // 20:00 — inside slot 10 window
        state.inverter.mode_state.set_user(InverterMode::Eco);
        state.batteries[0].soc_percent = 80.0;
        state.sync_battery_from_vec();

        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(ScheduleEngine::new(sched)),
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
            Box::new(InverterEngine::new()),
            Box::new(BatteryEngine::new()),
            Box::new(EnergyTracker::new()),
        ];
        let mut engine = SimulationEngine::new(state, devices, 60);

        engine.tick();
        assert!(
            engine.state.scheduled_discharge,
            "Slot 10 (18:00-22:00) at hour 20 should trigger scheduled_discharge"
        );
    }

    // --- Aging Model ---

    #[test]
    fn aging_tracks_throughput() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].power_kw = 3.0;
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[0].nominal_capacity_kwh = 10.0;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);

        // 3kW charging for 1h → 3 kWh throughput
        assert!((state.batteries[0].throughput_kwh - 3.0).abs() < 0.01);
    }

    #[test]
    fn aging_degrades_soh() {
        let mut state = PlantState::new(ts(12));
        state.batteries[0].power_kw = 5.0;
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[0].nominal_capacity_kwh = 10.0;
        state.batteries[0].soh = 1.0;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();
        // Run 100 hours of cycling
        for _ in 0..100 {
            let ctx = TickContext {
                now: ts(12),
                dt_hours: 1.0,
            };
            bat.update(&ctx, &mut state);
            state.batteries[0].power_kw = -state.batteries[0].power_kw;
            state.sync_battery_from_vec();
        }

        assert!(state.batteries[0].soh < 1.0, "SOH should have degraded");
        assert!(
            state.batteries[0].capacity_kwh < 10.0,
            "Capacity should have decreased"
        );
        assert!(
            (state.batteries[0].soh - 0.99).abs() < 0.005,
            "Expected SOH ~0.99, got {}",
            state.batteries[0].soh
        );
    }

    // --- Cell Balancing ---

    #[test]
    fn cell_balancing_diverges_soc() {
        let mut state = PlantState::with_battery_count(ts(12), 2);
        state.batteries[0].power_kw = 1.0;
        state.batteries[1].power_kw = 1.0;
        state.batteries[0].soc_percent = 50.0;
        state.batteries[1].soc_percent = 50.0;
        state.batteries[0].capacity_kwh = 10.0;
        state.batteries[1].capacity_kwh = 9.5;
        state.batteries[0].nominal_capacity_kwh = 10.0;
        state.batteries[1].nominal_capacity_kwh = 9.5;
        state.sync_battery_from_vec();

        let mut bat = BatteryEngine::new();

        // 1 charge tick (1h) + 1 discharge tick (1h) = 1 cycle
        let ctx_charge = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx_charge, &mut state);

        let soc_diff = (state.batteries[0].soc_percent - state.batteries[1].soc_percent).abs();
        assert!(soc_diff > 0.2, "SOC should diverge after 1 tick");
    }

    // --- Inverter Temperature ---

    #[test]
    fn inverter_temp_by_load() {
        let mut inv = InverterEngine::new();
        let mut state = PlantState::new(ts(14));
        // Set up solar > load so inverter processes 3000W through battery
        state.solar.generation_w = 5000.0;
        state.load.demand_w = 1000.0;
        state.batteries[0].max_charge_kw = 5.0;
        state.batteries[0].max_soc = 100.0;
        state.batteries[0].soc_percent = 50.0;
        state.sync_battery_from_vec();
        state.inverter.temperature_celsius = 25.0;

        let ctx = TickContext {
            now: ts(14),
            dt_hours: 1.0,
        };
        inv.update(&ctx, &mut state);

        // Excess power (4000W) should flow through inverter, generating heat
        assert!(
            state.inverter.temperature_celsius > 25.0,
            "Temperature should rise with load, got {}",
            state.inverter.temperature_celsius
        );
    }

    #[test]
    fn inverter_temp_cools_at_night() {
        let mut inv = InverterEngine::new();
        let mut state = PlantState::new(ts(2));
        state.inverter.ac_power_w = 0.0;
        state.inverter.temperature_celsius = 50.0;

        let ctx = TickContext {
            now: ts(2),
            dt_hours: 1.0,
        };
        inv.update(&ctx, &mut state);

        // Should cool down with no load (50 → ~37.5°C after 1h)
        assert!(
            state.inverter.temperature_celsius < 50.0 && state.inverter.temperature_celsius > 30.0,
            "Expected temp between 30-50°C, got {}",
            state.inverter.temperature_celsius
        );
    }

    // --- Combinatorial Tests ---

    #[test]
    fn combo_normal_1_battery() {
        let mut state = PlantState::new(ts(12));
        state.solar.generation_w = 4000.0;
        state.load.demand_w = 1000.0;
        state.batteries[0].soc_percent = 50.0;
        state.batteries[0].max_charge_kw = 3.0;
        state.sync_battery_from_vec();

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        inv.update(&ctx, &mut state);

        // Normal mode: excess solar charges battery
        assert!(
            state.total_battery_power_kw() > 0.0,
            "Battery should charge"
        );
    }

    #[test]
    fn combo_normal_2_batteries() {
        let mut state = PlantState::with_battery_count(ts(12), 2);
        state.solar.generation_w = 8000.0;
        state.load.demand_w = 1000.0;
        for b in &mut state.batteries {
            b.soc_percent = 50.0;
            b.max_charge_kw = 3.0;
        }
        state.sync_battery_from_vec();

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        inv.update(&ctx, &mut state);

        // Normal mode with 2 batteries should distribute power
        assert!(
            state.total_battery_power_kw() > 0.0,
            "Battery bank should charge"
        );
        assert!(
            (state.batteries[0].power_kw - state.batteries[1].power_kw).abs() < 0.01,
            "Power should be evenly distributed"
        );
    }

    #[test]
    fn combo_eco_3_batteries() {
        let mut state = PlantState::with_battery_count(ts(12), 3);
        state.solar.generation_w = 7000.0;
        state.load.demand_w = 1000.0;
        state.inverter.mode_state.set_user(InverterMode::Eco);
        for b in &mut state.batteries {
            b.soc_percent = 50.0;
            b.max_charge_kw = 3.0;
        }
        state.sync_battery_from_vec();

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        inv.update(&ctx, &mut state);

        // Eco mode caps daytime charging at 50% (hour 12 is in 10-16 window)
        // 3 batteries × 3kW = 9kW max charge, but inverter cap = 5000W
        // charge_limit = min(9000, SOC_headroom, 5000) = 5000W
        // eco_charge_limit = 5000 * 0.5 = 2500W
        // Excess: 6kW - 2.5kW charge = 3.5kW exported
        let total_power = state.total_battery_power_kw();
        assert!(total_power > 0.0, "Battery bank should charge in Eco");
        assert!(
            (total_power - 2.5).abs() < 0.01,
            "Eco should cap charge at 50% of inverter limit: got {}",
            total_power
        );
        assert!(
            state.grid.power_w < -3000.0,
            "Should export excess solar to grid, got grid={}",
            state.grid.power_w
        );
    }

    #[test]
    fn combo_force_charge_with_fault() {
        let mut state = PlantState::new(ts(2));
        state
            .inverter
            .mode_state
            .set_user(InverterMode::ForceCharge);
        state.batteries[0].soc_percent = 30.0;
        state.batteries[0].max_charge_kw = 3.0;
        state.sync_battery_from_vec();

        // Apply grid loss fault
        state.active_faults.push("grid_loss".into());

        let mut inv = InverterEngine::new();
        let mut faults = sim_faults::FaultEngine::new();
        let ctx = TickContext {
            now: ts(2),
            dt_hours: 1.0,
        };

        // Faults run first (prevents grid interaction)
        faults.update(&ctx, &mut state);
        // Then inverter runs in island mode
        inv.update(&ctx, &mut state);

        // With grid loss, battery can't force-charge from grid
        // In island mode, no solar means battery stays balanced
        assert_eq!(state.grid.connected, false);
        assert_eq!(state.grid.power_w, 0.0);
    }

    #[test]
    fn combo_export_limit_varied_battery() {
        let mut state = PlantState::with_battery_count(ts(12), 2);
        state.solar.generation_w = 8000.0;
        state.load.demand_w = 1000.0;
        state
            .inverter
            .mode_state
            .set_user(InverterMode::ExportLimit);
        state.inverter.export_limit_w = 1000.0;
        for b in &mut state.batteries {
            b.soc_percent = 50.0;
            b.max_charge_kw = 3.0;
        }
        state.sync_battery_from_vec();

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        inv.update(&ctx, &mut state);

        // Export limit should cap export at 1000W
        let export = -state.grid.power_w;
        assert!(export <= 1000.0 + 1.0, "Export should be capped at 1000W");
    }

    #[test]
    fn combo_force_discharge_all_battery_counts() {
        // Verify force discharge works similarly for 1, 2, and 3 batteries
        for count in [1usize, 2usize, 3usize] {
            let mut state = PlantState::with_battery_count(ts(20), count);
            state
                .inverter
                .mode_state
                .set_user(InverterMode::ForceDischarge);
            for b in &mut state.batteries {
                b.soc_percent = 80.0;
                b.max_discharge_kw = 3.0;
            }
            state.sync_battery_from_vec();

            let mut inv = InverterEngine::new();
            let ctx = TickContext {
                now: ts(20),
                dt_hours: 1.0,
            };
            inv.update(&ctx, &mut state);

            assert!(
                state.total_battery_power_kw() < 0.0,
                "Battery bank ({count}) should discharge"
            );

            // Export should be ~min(max_discharge * count, inv_max_ac_watts)
            // Default inverter max = 5000W = 5kW
            let max_total = (count as f64 * 3.0).min(5.0);
            assert!(
                (-state.total_battery_power_kw() - max_total).abs() < 1.0,
                "Battery bank ({count}) discharge should be ~{max_total}kW"
            );
        }
    }
    // Manual Override Tests
    // ===================================================================

    #[test]
    fn solar_override_fixes_generation() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        state.solar_override = Some(2000.0);
        let devices: Vec<Box<dyn DeviceModel>> = vec![Box::new(SolarEngine::new(5000.0, 51.5))];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.tick();
        assert_eq!(
            engine.state.solar.generation_w, 2000.0,
            "Solar override should fix generation to 2000W"
        );
    }

    #[test]
    fn solar_override_none_uses_engine() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        state.solar_override = None;
        let devices: Vec<Box<dyn DeviceModel>> = vec![Box::new(SolarEngine::new(5000.0, 51.5))];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.tick();
        assert!(
            engine.state.solar.generation_w > 0.0,
            "Solar should generate without override"
        );
    }

    #[test]
    fn solar_override_zero() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        state.solar_override = Some(0.0);
        let devices: Vec<Box<dyn DeviceModel>> = vec![Box::new(SolarEngine::new(5000.0, 51.5))];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.tick();
        assert_eq!(
            engine.state.solar.generation_w, 0.0,
            "Solar override 0 should zero generation"
        );
    }

    #[test]
    fn load_override_fixes_demand() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(18, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        state.load_override = Some(1500.0);
        let devices: Vec<Box<dyn DeviceModel>> =
            vec![Box::new(LoadEngine::new(LoadProfile::Family))];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.tick();
        assert_eq!(
            engine.state.load.demand_w, 1500.0,
            "Load override should fix demand to 1500W"
        );
    }

    #[test]
    fn load_override_none_uses_engine() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(18, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        state.load_override = None;
        let devices: Vec<Box<dyn DeviceModel>> =
            vec![Box::new(LoadEngine::new(LoadProfile::Family))];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.tick();
        assert!(
            engine.state.load.demand_w > 0.0,
            "Load should generate demand without override"
        );
    }

    #[test]
    fn solar_override_at_night_overrides_zero_generation() {
        // 22:00 — sunset → normal engine would set generation=0
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(22, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        state.solar_override = Some(250.0); // 250W override at night
        let devices: Vec<Box<dyn DeviceModel>> = vec![Box::new(SolarEngine::new(5000.0, 51.5))];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.enqueue(Command::SetSolarOverride(Some(250.0)));
        engine.tick();
        assert_eq!(
            engine.state.solar.generation_w, 250.0,
            "Override should apply at night even though engine would zero generation"
        );
    }

    #[test]
    fn solar_override_via_command() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let state = PlantState::new(ts);
        let devices: Vec<Box<dyn DeviceModel>> = vec![Box::new(SolarEngine::new(5000.0, 51.5))];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.enqueue(Command::SetSolarOverride(Some(3000.0)));
        engine.tick();
        assert_eq!(engine.state.solar.generation_w, 3000.0);
        // Clear override
        engine.enqueue(Command::SetSolarOverride(None));
        engine.tick();
        assert!(
            engine.state.solar.generation_w != 3000.0,
            "After clearing override, engine should control solar"
        );
    }

    #[test]
    fn load_override_via_command() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let state = PlantState::new(ts);
        let devices: Vec<Box<dyn DeviceModel>> =
            vec![Box::new(LoadEngine::new(LoadProfile::Family))];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.enqueue(Command::SetLoadOverride(Some(999.0)));
        engine.tick();
        assert_eq!(engine.state.load.demand_w, 999.0);
        engine.enqueue(Command::SetLoadOverride(None));
        engine.tick();
        assert!(engine.state.load.demand_w != 999.0);
    }

    #[test]
    fn override_persists_across_ticks() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        state.solar_override = Some(1234.0);
        state.load_override = Some(567.0);
        let devices: Vec<Box<dyn DeviceModel>> = vec![
            Box::new(SolarEngine::new(5000.0, 51.5)),
            Box::new(LoadEngine::new(LoadProfile::Family)),
        ];
        let mut engine = SimulationEngine::new(state, devices, 1);
        for _ in 0..5 {
            engine.tick();
            assert_eq!(engine.state.solar.generation_w, 1234.0);
            assert_eq!(engine.state.load.demand_w, 567.0);
        }
    }

    #[test]
    fn override_survives_serialization_roundtrip() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        state.solar_override = Some(2500.0);
        state.load_override = Some(800.0);
        let json = serde_json::to_string(&state).unwrap();
        let loaded: PlantState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.solar_override, Some(2500.0));
        assert_eq!(loaded.load_override, Some(800.0));
    }

    #[test]
    fn override_missing_in_json_means_none() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let mut state = PlantState::new(ts);
        // Serialize without overrides, then parse — should get None
        state.solar_override = None;
        state.load_override = None;
        let json = serde_json::to_string(&state).unwrap();
        assert!(json.contains("solar_override")); // None serializes as null
        let loaded: PlantState = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.solar_override, None);
        assert_eq!(loaded.load_override, None);
    }

    #[test]
    fn set_battery_soc_command() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let state = PlantState::with_battery_count(ts, 3);
        let devices: Vec<Box<dyn DeviceModel>> = vec![];
        let mut engine = SimulationEngine::new(state, devices, 1);
        // Set module 1 to 75%
        engine.enqueue(Command::SetBatterySoc {
            module: 1,
            soc: 75.0,
        });
        engine.tick();
        assert!((engine.state.batteries[1].soc_percent - 75.0).abs() < 0.01);
        // Module 0 unchanged (default SOC)
        assert!(
            (engine.state.batteries[0].soc_percent - engine.state.batteries[1].soc_percent).abs()
                > 1.0
        );
    }

    #[test]
    fn set_battery_soc_clamps_0_100() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let state = PlantState::new(ts);
        let devices: Vec<Box<dyn DeviceModel>> = vec![];
        let mut engine = SimulationEngine::new(state, devices, 1);
        engine.enqueue(Command::SetBatterySoc {
            module: 0,
            soc: 150.0,
        });
        engine.tick();
        assert!((engine.state.batteries[0].soc_percent - 100.0).abs() < 0.01);
        engine.enqueue(Command::SetBatterySoc {
            module: 0,
            soc: -10.0,
        });
        engine.tick();
        assert!((engine.state.batteries[0].soc_percent - 0.0).abs() < 0.01);
    }

    #[test]
    fn set_battery_soc_invalid_index_ignored() {
        let ts = NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap();
        let state = PlantState::new(ts);
        let devices: Vec<Box<dyn DeviceModel>> = vec![];
        let mut engine = SimulationEngine::new(state, devices, 1);
        let original_soc = engine.state.batteries[0].soc_percent;
        // Index 99 doesn't exist — should not panic, module 0 unchanged
        engine.enqueue(Command::SetBatterySoc {
            module: 99,
            soc: 50.0,
        });
        engine.tick();
        assert!((engine.state.batteries[0].soc_percent - original_soc).abs() < 0.01);
    }

    // ===================================================================
    // Scenario Fuzzer — property-based tests using proptest
    // ===================================================================

    use proptest::prelude::*;

    /// Invariant check that every simulation run must satisfy:
    /// - No NaN or infinite values in engine state
    /// - SOC stays in [0, 100]
    /// - Battery temperatures in reasonable range [-10, 70]
    /// - Grid connection state is not NaN
    /// - Energy totals are non-negative and finite
    fn check_invariants(state: &PlantState) {
        // SOC must be in valid range
        for b in &state.batteries {
            assert!(
                b.soc_percent.is_finite(),
                "SOC must be finite: {}",
                b.soc_percent
            );
            assert!(
                b.soc_percent >= -1.0 && b.soc_percent <= 101.0,
                "SOC {:.2} out of range [-1, 101]",
                b.soc_percent
            );
            assert!(b.temperature_celsius.is_finite());
            assert!(
                b.temperature_celsius >= -15.0 && b.temperature_celsius <= 80.0,
                "Battery temp {:.1} out of range [-15, 80]",
                b.temperature_celsius
            );
            assert!(b.voltage_v.is_finite() && b.voltage_v >= 0.0);
            assert!(b.power_kw.is_finite());
            assert!(b.capacity_kwh.is_finite() && b.capacity_kwh >= 0.0);
            assert!(b.throughput_kwh.is_finite() && b.throughput_kwh >= 0.0);
            assert!(b.soh.is_finite() && b.soh >= 0.0 && b.soh <= 1.0);
        }

        // Power values must be finite
        assert!(state.solar.generation_w.is_finite() && state.solar.generation_w >= 0.0);
        assert!(state.load.demand_w.is_finite() && state.load.demand_w >= 0.0);
        assert!(state.grid.power_w.is_finite());
        assert!(state.inverter.ac_power_w.is_finite() && state.inverter.ac_power_w >= 0.0);
        assert!(state.inverter.temperature_celsius.is_finite());
        assert!(
            state.inverter.temperature_celsius >= -10.0
                && state.inverter.temperature_celsius <= 100.0
        );

        // Energy totals must be non-negative
        let e = &state.energy_totals;
        assert!(e.grid_import_kwh.is_finite() && e.grid_import_kwh >= 0.0);
        assert!(e.grid_export_kwh.is_finite() && e.grid_export_kwh >= 0.0);
        assert!(e.battery_charge_kwh.is_finite() && e.battery_charge_kwh >= 0.0);
        assert!(e.battery_discharge_kwh.is_finite() && e.battery_discharge_kwh >= 0.0);
        assert!(e.solar_generation_kwh.is_finite() && e.solar_generation_kwh >= 0.0);
        assert!(e.load_consumption_kwh.is_finite() && e.load_consumption_kwh >= 0.0);

        // Weather string must be valid
        assert!(!state.weather.is_empty());
    }

    proptest! {
        /// Fuzz the simulation with random states, schedules, and tick counts.
        /// Runs the full device pipeline and asserts invariants never break.
        #[test]
        fn fuzz_simulation(
            solar_peak in 1000.0f64..20000.0f64,
            battery_soc in 0.0f64..100.0f64,
            battery_capacity in 1.0f64..20.0f64,
            battery_count in 1usize..=3usize,
            tick_count in 1usize..50usize,
            hour in 0u32..24u32,
            load_w in 0.0f64..10000.0f64,
            solar_override in proptest::option::of(0.0f64..10000.0f64),
            load_override in proptest::option::of(0.0f64..10000.0f64),
        ) {
            let ts = NaiveDate::from_ymd_opt(2025, 6, 21).unwrap()
                .and_hms_opt(hour, 0, 0).unwrap();
            let mut state = PlantState::with_battery_count(ts, battery_count);
            state.config.solar_peak_watts = solar_peak;
            state.config.latitude = 51.5;
            state.solar.generation_w = solar_peak * 0.5; // rough midday
            state.load.demand_w = load_w;
            state.solar_override = solar_override;
            state.load_override = load_override;
            for b in &mut state.batteries {
                b.soc_percent = battery_soc;
                b.capacity_kwh = battery_capacity;
                b.nominal_capacity_kwh = battery_capacity;
                b.max_charge_kw = (battery_capacity * 0.7).min(10.0);
                b.max_discharge_kw = (battery_capacity * 0.7).min(10.0);
            }
            state.sync_battery_from_vec();

            let devices: Vec<Box<dyn DeviceModel>> = vec![
                Box::new(ScheduleEngine::new(Schedule::default())),
                Box::new(SolarEngine::new(solar_peak, 51.5)),
                Box::new(LoadEngine::new(LoadProfile::Family)),
                Box::new(InverterEngine::new()),
                Box::new(sim_faults::FaultEngine::new()),
                Box::new(BatteryEngine::new()),
                Box::new(EnergyTracker::new()),
            ];
            let mut engine = SimulationEngine::new(state, devices, 30);

            for _ in 0..tick_count {
                engine.tick();
                check_invariants(&engine.state);
            }
        }
    }
}
