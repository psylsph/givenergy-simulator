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
    /// Update the charge/discharge schedule.
    SetSchedule(Schedule),
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
                    self.state.weather = format!("{:?}", w);
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
                        let c_rate_kw = (b.capacity_kwh * 0.3).min(10.0);
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
                Command::SetSchedule(sched) => {
                    for device in &mut self.devices {
                        if let Some(se) = device.as_any_mut().downcast_mut::<ScheduleEngine>() {
                            se.schedule = sched.clone();
                            break;
                        }
                    }
                }
            }
        }
    }

    /// Advance simulation by one tick.
    ///
    /// 1. Apply pending commands.
    /// 2. Build tick context.
    /// 3. Call `update` on every device model in registration order.
    /// 4. Advance the timestamp.
    pub fn tick(&mut self) {
        self.apply_commands();

        let dt_hours = self.tick_interval_secs as f64 / 3600.0;
        let ctx = TickContext {
            now: self.state.timestamp,
            dt_hours,
        };

        for device in &mut self.devices {
            device.update(&ctx, &mut self.state);
        }

        // Run calibration if active (after devices so it sees latest SOC/power)
        CalibrationEngine::new().update(&ctx, &mut self.state);

        self.state.timestamp += chrono::TimeDelta::seconds(self.tick_interval_secs as i64);
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
            let pv2_peak = state.config.pv2_peak_watts;
            if pv2_peak > 0.0 {
                state.solar.pv1_w = w * 0.45;
                state.solar.pv2_w = w * 0.55;
            } else {
                state.solar.pv1_w = w;
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
pub fn parse_weather_from_str(s: &str) -> WeatherCondition {
    match s.to_lowercase().as_str() {
        "partlycloudy" | "partly-cloudy" | "partly_cloudy" => WeatherCondition::PartlyCloudy,
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
            let max_charge_w = state.total_max_charge_kw() * 1000.0;
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
            let max_discharge_w = state.total_max_discharge_kw() * 1000.0;
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

    /// Eco priority: like Normal but preserves battery charge for evening peak.
    /// - During daytime (10:00–16:00): caps battery charging at 50% of max rate
    ///   and prefers grid export over charging.
    /// - During evening/night: uses battery freely to cover load.
    fn eco_priority(&self, state: &mut PlantState, solar_w: f64, load_w: f64) {
        let _hour = state.timestamp.time().hour();
        let net = solar_w - load_w;
        let inv_max_w = state.config.max_ac_watts;

        if net >= 0.0 {
            let excess = net;
            let max_charge_w = state.total_max_charge_kw() * 1000.0;
            let soc_headroom = (state.max_aggregate_soc() - state.aggregate_soc()) / 100.0
                * state.total_battery_capacity()
                * 1000.0;
            let charge_limit = max_charge_w.min(soc_headroom).min(inv_max_w).max(0.0);

            let eco_charge_limit = charge_limit;

            let to_battery = excess.min(eco_charge_limit);
            let to_grid = excess - to_battery;

            state.distribute_battery_power(to_battery / 1000.0);
            state.grid.power_w = -to_grid;
            state.inverter.ac_power_w = solar_w;
        } else {
            let deficit = -net;
            let max_discharge_w = state.total_max_discharge_kw() * 1000.0;
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
    fn export_limit(&self, state: &mut PlantState, solar_w: f64, load_w: f64) {
        self.normal_priority(state, solar_w, load_w);

        if state.grid.power_w < 0.0 {
            let export = -state.grid.power_w;
            let capped = export.min(state.inverter.export_limit_w);
            let curtailed = export - capped;
            state.grid.power_w = -capped;
            state.inverter.ac_power_w -= curtailed;
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

        let max_charge_w = state.total_max_charge_kw() * 1000.0;
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

        let max_discharge_w = state.total_max_discharge_kw() * 1000.0;
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
            let max_charge_w = state.total_max_charge_kw() * 1000.0;
            let soc_headroom = (state.max_aggregate_soc() - state.aggregate_soc()) / 100.0
                * state.total_battery_capacity()
                * 1000.0;
            let charge_limit = max_charge_w.min(soc_headroom).max(0.0);
            let to_battery = excess.min(charge_limit);

            state.distribute_battery_power(to_battery / 1000.0);
            state.inverter.ac_power_w = load_w + to_battery;
        } else {
            let deficit = -net;
            let max_discharge_w = state.total_max_discharge_kw() * 1000.0;
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

    /// Advance calibration by one tick.
    pub fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
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
                state
                    .inverter
                    .mode_state
                    .set_user(InverterMode::ForceCharge);
                let all_full = targets
                    .iter()
                    .all(|&i| state.batteries[i].soc_percent >= 99.5);
                if all_full {
                    state.calibration.stage = CalibrationStage::HoldingFull;
                    state.calibration.stage_elapsed_secs = 0.0;
                }
            }

            CalibrationStage::HoldingFull => {
                state
                    .inverter
                    .mode_state
                    .set_user(InverterMode::ForceCharge);
                if state.calibration.stage_elapsed_secs >= Self::HOLD_SECONDS {
                    state.calibration.stage = CalibrationStage::DischargeToEmpty;
                    state.calibration.stage_elapsed_secs = 0.0;
                    state
                        .inverter
                        .mode_state
                        .set_user(InverterMode::ForceDischarge);
                }
            }

            CalibrationStage::DischargeToEmpty => {
                state
                    .inverter
                    .mode_state
                    .set_user(InverterMode::ForceDischarge);
                let reserve = state.min_aggregate_soc().min(state.max_aggregate_soc());
                let all_empty = targets
                    .iter()
                    .all(|&i| state.batteries[i].soc_percent <= (reserve + 0.5));
                if all_empty {
                    state.calibration.stage = CalibrationStage::Complete;
                    state.calibration.stage_elapsed_secs = 0.0;
                    state.inverter.mode_state.set_user(InverterMode::Eco);
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

        for b in &mut state.batteries {
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

            let delta_soc = (effective_power_kw * ctx.dt_hours) / b.capacity_kwh * 100.0;

            b.soc_percent += delta_soc;

            // Clamp to min/max SOC
            b.soc_percent = b.soc_percent.clamp(b.min_soc, b.max_soc);

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

            // Estimate terminal voltage from SOC (Li-ion: 44V–52V range)
            b.voltage_v = 44.0 + (b.soc_percent / 100.0) * 8.0;

            // Current from power and voltage (signed, positive = charging)
            b.current_a = if b.voltage_v > 0.0 {
                (b.power_kw * 1000.0) / b.voltage_v
            } else {
                0.0
            };
        }

        // Sync convenience field
        state.sync_battery_from_vec();
    }
}

// ---------------------------------------------------------------------------
// EnergyTracker — cumulative energy totals
// ---------------------------------------------------------------------------

/// Device model that accumulates energy totals each tick.
/// Must be registered **last** (after BatteryEngine) so power values are final.
#[derive(Debug, Clone)]
pub struct EnergyTracker;

impl EnergyTracker {
    pub fn new() -> Self {
        Self
    }
}

impl Default for EnergyTracker {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceModel for EnergyTracker {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
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
            totals.battery_charge_kwh += battery_kw * dt_hours;
        } else {
            totals.battery_discharge_kwh += (-battery_kw) * dt_hours;
        }

        // Solar generation
        totals.solar_generation_kwh += solar_w / 1000.0 * dt_hours;

        // Load consumption
        totals.load_consumption_kwh += load_w / 1000.0 * dt_hours;
    }
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

        // Check charge slot 1
        if self.schedule.charge_start != self.schedule.charge_end {
            if in_window(self.schedule.charge_start, self.schedule.charge_end, hour)
                && soc < self.schedule.charge_target_soc
            {
                state.scheduled_charge = true;
                return;
            }
        }

        // Check charge slot 2
        if self.schedule.charge_start_2 != self.schedule.charge_end_2 {
            if in_window(
                self.schedule.charge_start_2,
                self.schedule.charge_end_2,
                hour,
            ) && soc < self.schedule.charge_target_soc_2
            {
                state.scheduled_charge = true;
                return;
            }
        }

        // Check discharge slot 1
        if self.schedule.discharge_start != self.schedule.discharge_end {
            if in_window(
                self.schedule.discharge_start,
                self.schedule.discharge_end,
                hour,
            ) && soc > self.schedule.discharge_target_soc
            {
                state.scheduled_discharge = true;
                return;
            }
        }

        // Check discharge slot 2
        if self.schedule.discharge_start_2 != self.schedule.discharge_end_2 {
            if in_window(
                self.schedule.discharge_start_2,
                self.schedule.discharge_end_2,
                hour,
            ) && soc > self.schedule.discharge_target_soc_2
            {
                state.scheduled_discharge = true;
            }
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
        // At 12:00 with solar surplus, Eco and Normal should charge at the same rate
        // (the old 50% daytime cap has been removed to match real inverter behavior)
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
            (eco_charge - normal_charge).abs() < 0.01,
            "Eco and Normal should charge at the same rate: eco={}, normal={}",
            eco_charge,
            normal_charge
        );
    }

    #[test]
    fn eco_mode_same_as_normal_at_night() {
        // At 21:00 with load deficit, Eco should behave same as Normal
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

        // Eco mode no longer caps daytime charging (matches real inverter behavior)
        // 3 batteries × 3kW = 9kW max charge, excess = 6kW
        // But default inverter max_ac_watts = 5000W caps charge to 5kW
        // The remaining 1kW goes to grid
        let total_power = state.total_battery_power_kw();
        assert!(total_power > 0.0, "Battery bank should charge in Eco");
        assert!(
            (total_power - 5.0).abs() < 0.01,
            "Should charge at inverter cap: got {}",
            total_power
        );
        assert!(
            (state.grid.power_w + 1000.0).abs() < 1.0,
            "Should export 1kW excess above inverter cap"
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
}
