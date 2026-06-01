//! Simulation core: tick scheduler, command queue, real device models.
//!
//! State transitions occur only during simulation ticks.
//! All external writes become [`Command`]s applied between ticks.
//!
//! Device update order: **Solar → Load → Inverter → Battery**.

use chrono::{Datelike, Timelike};
use serde::{Deserialize, Serialize};
use sim_models::DeviceModel;

// Re-export types that consumers need
pub use sim_models::{BatteryState, GridState, InverterMode, InverterState, LoadState, PlantState, SolarState, TickContext};

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
                Command::SetInverterMode(mode) => self.state.inverter.mode = mode,
                Command::SetExportLimit(limit) => self.state.inverter.export_limit_w = limit,
                Command::SetMinSoc(v) => self.state.battery.min_soc = v,
                Command::SetMaxSoc(v) => self.state.battery.max_soc = v,
                Command::InjectFault(id) => {
                    if !self.state.active_faults.contains(&id) {
                        self.state.active_faults.push(id);
                    }
                }
                Command::ClearFault(id) => {
                    self.state.active_faults.retain(|f| f != &id);
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
#[derive(Debug, Clone)]
pub struct SolarEngine {
    /// Total installed panel capacity in watts (peak).
    pub peak_capacity_w: f64,
    /// Site latitude in degrees (positive = north).
    pub latitude: f64,
    /// Current weather condition.
    pub weather: WeatherCondition,
}

impl SolarEngine {
    pub fn new(peak_capacity_w: f64, latitude: f64) -> Self {
        Self {
            peak_capacity_w,
            latitude,
            weather: WeatherCondition::Clear,
        }
    }

    /// Estimate sunrise hour (decimal) for a given day-of-year and latitude.
    fn sunrise_hour(&self, day_of_year: u32) -> f64 {
        let lat_rad = self.latitude.to_radians();
        let declination = 23.45_f64.to_radians()
            * (2.0 * std::f64::consts::PI * (day_of_year as f64 + 284.0) / 365.0).sin();
        let cos_hour_angle =
            (-lat_rad.tan() * declination.tan()).min(1.0).max(-1.0);
        12.0 - cos_hour_angle.acos().to_degrees() / 15.0
    }

    /// Estimate sunset hour (decimal).
    fn sunset_hour(&self, day_of_year: u32) -> f64 {
        24.0 - self.sunrise_hour(day_of_year)
    }
}

impl DeviceModel for SolarEngine {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        let hour = ctx.now.time().num_seconds_from_midnight() as f64 / 3600.0;
        let day_of_year = ctx.now.ordinal();

        let sunrise = self.sunrise_hour(day_of_year);
        let sunset = self.sunset_hour(day_of_year);
        let day_length = sunset - sunrise;

        if day_length <= 0.0 || hour <= sunrise || hour >= sunset {
            state.solar.generation_w = 0.0;
            return;
        }

        // Solar elevation factor: peak irradiance depends on noon elevation angle
        // Noon elevation = 90° - |latitude - declination|
        let declination = 23.45_f64.to_radians()
            * (2.0 * std::f64::consts::PI * (day_of_year as f64 + 284.0) / 365.0).sin();
        let noon_elevation = (std::f64::consts::FRAC_PI_2
            - (self.latitude.to_radians() - declination).abs())
        .max(0.0);
        let elevation_factor = noon_elevation.sin();

        // Sinusoidal irradiance over the day
        let t = (hour - sunrise) / day_length;
        let irradiance = (std::f64::consts::PI * t).sin();

        state.solar.generation_w = self.peak_capacity_w
            * irradiance
            * elevation_factor
            * self.weather.irradiance_factor();

        state.solar.generation_w = state.solar.generation_w.min(self.peak_capacity_w).max(0.0);
    }
}

// ---------------------------------------------------------------------------
// LoadEngine
// ---------------------------------------------------------------------------

/// Pre-built household load profiles.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub enum LoadProfile {
    /// Low baseline ~200-400W.
    Minimal,
    /// Typical family home ~300-3000W with morning/evening peaks.
    Family,
    /// Family + EV charger ~300-7000W, charges overnight.
    EV,
    /// Family + heat pump ~500-5000W, heating in morning/evening.
    HeatPump,
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
        }
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
    fn update(&mut self, _ctx: &TickContext, state: &mut PlantState) {
        let solar_w = state.solar.generation_w;
        let load_w = state.load.demand_w;

        if !state.grid.connected {
            // Grid is down — island mode
            self.island_mode(state, solar_w, load_w);
            return;
        }

        match state.inverter.mode {
            InverterMode::Normal | InverterMode::Eco => {
                self.normal_priority(state, solar_w, load_w);
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
    }
}

impl InverterEngine {
    /// Normal priority: Solar → Load, excess → Battery, surplus → Grid.
    /// Deficit: Battery → Load, then Grid → Load.
    fn normal_priority(&self, state: &mut PlantState, solar_w: f64, load_w: f64) {
        let net = solar_w - load_w;

        if net >= 0.0 {
            // Solar covers load. Excess charges battery.
            let excess = net;
            let max_charge_w = state.battery.max_charge_kw * 1000.0;
            let soc_headroom =
                (state.battery.max_soc - state.battery.soc_percent) / 100.0 * state.battery.capacity_kwh * 1000.0;
            let charge_limit = max_charge_w.min(soc_headroom).max(0.0);
            let to_battery = excess.min(charge_limit);
            let to_grid = excess - to_battery;

            state.battery.power_kw = to_battery / 1000.0;
            state.grid.power_w = -to_grid; // negative = export
            state.inverter.ac_power_w = solar_w;
        } else {
            // Solar deficit. Battery supplies first, then grid.
            let deficit = -net;
            let max_discharge_w = state.battery.max_discharge_kw * 1000.0;
            let soc_available =
                (state.battery.soc_percent - state.battery.min_soc) / 100.0 * state.battery.capacity_kwh * 1000.0;
            let discharge_limit = max_discharge_w.min(soc_available).max(0.0);
            let from_battery = deficit.min(discharge_limit);
            let from_grid = deficit - from_battery;

            state.battery.power_kw = -from_battery / 1000.0; // negative = discharging
            state.grid.power_w = from_grid; // positive = import
            state.inverter.ac_power_w = solar_w + from_battery;
        }
    }

    /// Export-limited mode: same as Normal but caps grid export.
    fn export_limit(&self, state: &mut PlantState, solar_w: f64, load_w: f64) {
        // First do normal priority
        self.normal_priority(state, solar_w, load_w);

        // Then clamp export
        if state.grid.power_w < 0.0 {
            let export = -state.grid.power_w;
            let capped = export.min(state.inverter.export_limit_w);
            let curtailed = export - capped;
            state.grid.power_w = -capped;
            // Curtailed energy is simply lost (inverter throttles panels)
            // Reduce inverter AC output by curtailed amount
            state.inverter.ac_power_w -= curtailed;
        }
    }

    /// Force charge: grid charges battery at max rate until full.
    fn force_charge(&self, state: &mut PlantState) {
        let solar_w = state.solar.generation_w;
        let load_w = state.load.demand_w;

        // Solar still supplies load first
        let net = solar_w - load_w;
        let solar_surplus = net.max(0.0);
        let solar_deficit = (-net).max(0.0);

        // Charge battery: use solar surplus first, then grid
        let max_charge_w = state.battery.max_charge_kw * 1000.0;
        let soc_headroom =
            (state.battery.max_soc - state.battery.soc_percent) / 100.0 * state.battery.capacity_kwh * 1000.0;
        let charge_capacity = max_charge_w.min(soc_headroom.max(0.0));

        let from_solar = solar_surplus.min(charge_capacity);
        let remaining_capacity = charge_capacity - from_solar;
        let from_grid = remaining_capacity.min(max_charge_w);

        state.battery.power_kw = (from_solar + from_grid) / 1000.0;
        state.grid.power_w = solar_deficit + from_grid; // import
        state.inverter.ac_power_w = solar_w;
    }

    /// Force discharge: battery exports to grid at max rate.
    fn force_discharge(&self, state: &mut PlantState) {
        let solar_w = state.solar.generation_w;
        let load_w = state.load.demand_w;

        // Solar supplies load first
        let net = solar_w - load_w;

        // Battery discharges to grid
        let max_discharge_w = state.battery.max_discharge_kw * 1000.0;
        let soc_available =
            (state.battery.soc_percent - state.battery.min_soc) / 100.0 * state.battery.capacity_kwh * 1000.0;
        let discharge = max_discharge_w.min(soc_available.max(0.0));

        state.battery.power_kw = -discharge / 1000.0;

        if net >= 0.0 {
            state.grid.power_w = -(net + discharge); // export both solar excess and battery
            state.inverter.ac_power_w = solar_w + discharge;
        } else {
            let deficit = -net;
            let battery_to_load = deficit.min(discharge);
            let battery_to_grid = discharge - battery_to_load;
            state.grid.power_w = -battery_to_grid + deficit - battery_to_load;
            // Simplify: grid gets (deficit - battery_to_load) as import minus battery_to_grid as export
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
            let max_charge_w = state.battery.max_charge_kw * 1000.0;
            let soc_headroom =
                (state.battery.max_soc - state.battery.soc_percent) / 100.0 * state.battery.capacity_kwh * 1000.0;
            let charge_limit = max_charge_w.min(soc_headroom).max(0.0);
            let to_battery = excess.min(charge_limit);
            // Any excess beyond battery is curtailed (nowhere to go)

            state.battery.power_kw = to_battery / 1000.0;
            state.inverter.ac_power_w = load_w + to_battery;
        } else {
            let deficit = -net;
            let max_discharge_w = state.battery.max_discharge_kw * 1000.0;
            let soc_available =
                (state.battery.soc_percent - state.battery.min_soc) / 100.0 * state.battery.capacity_kwh * 1000.0;
            let discharge_limit = max_discharge_w.min(soc_available).max(0.0);
            let from_battery = deficit.min(discharge_limit);

            state.battery.power_kw = -from_battery / 1000.0;
            state.inverter.ac_power_w = solar_w + from_battery;
        }
    }
}

// ---------------------------------------------------------------------------
// BatteryEngine — SOC tracking
// ---------------------------------------------------------------------------

/// Battery SOC tracker.
///
/// Reads `state.battery.power_kw` (set by [`InverterEngine`]) and updates SOC.
///
/// Formula: `soc += (power_kw * dt_hours) / capacity_kwh * 100`
#[derive(Debug, Clone)]
pub struct BatteryEngine;

impl BatteryEngine {
    pub fn new() -> Self {
        Self
    }
}

impl DeviceModel for BatteryEngine {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState) {
        let power_kw = state.battery.power_kw; // positive = charging
        let delta_soc =
            (power_kw * ctx.dt_hours) / state.battery.capacity_kwh * 100.0;

        state.battery.soc_percent += delta_soc;

        // Clamp to min/max SOC
        state.battery.soc_percent =
            state.battery.soc_percent.clamp(state.battery.min_soc, state.battery.max_soc);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
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

        let state = PlantState::new(ts(12));
        let mut solar = SolarEngine::new(5000.0, 51.5);
        solar.weather = WeatherCondition::Overcast;
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
        assert!(
            state.load.demand_w > 0.0,
        );
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

    // --- BatteryEngine ---

    #[test]
    fn battery_charges_from_positive_power() {
        let mut state = PlantState::new(ts(12));
        state.battery.soc_percent = 50.0;
        state.battery.power_kw = 3.0; // charging at 3kW
        state.battery.capacity_kwh = 10.0;

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);

        // soc += (3.0 * 1.0) / 10.0 * 100 = +30%
        assert!(
            (state.battery.soc_percent - 80.0).abs() < 0.01,
            "Expected SOC ~80%, got {}",
            state.battery.soc_percent
        );
    }

    #[test]
    fn battery_discharges() {
        let mut state = PlantState::new(ts(12));
        state.battery.soc_percent = 50.0;
        state.battery.power_kw = -2.0; // discharging at 2kW
        state.battery.capacity_kwh = 10.0;

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);

        // soc += (-2.0 * 1.0) / 10.0 * 100 = -20%
        assert!(
            (state.battery.soc_percent - 30.0).abs() < 0.01,
            "Expected SOC ~30%, got {}",
            state.battery.soc_percent
        );
    }

    #[test]
    fn battery_clamps_at_max_soc() {
        let mut state = PlantState::new(ts(12));
        state.battery.soc_percent = 99.0;
        state.battery.power_kw = 5.0;
        state.battery.capacity_kwh = 10.0;
        state.battery.max_soc = 100.0;

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);
        assert_eq!(state.battery.soc_percent, 100.0);
    }

    #[test]
    fn battery_clamps_at_min_soc() {
        let mut state = PlantState::new(ts(12));
        state.battery.soc_percent = 11.0;
        state.battery.power_kw = -5.0;
        state.battery.capacity_kwh = 10.0;
        state.battery.min_soc = 10.0;

        let mut bat = BatteryEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 1.0,
        };
        bat.update(&ctx, &mut state);
        assert_eq!(state.battery.soc_percent, 10.0);
    }

    // --- InverterEngine ---

    #[test]
    fn inverter_solar_covers_load_charges_battery() {
        let mut state = PlantState::new(ts(12));
        state.solar.generation_w = 5000.0;
        state.load.demand_w = 1000.0;
        state.battery.soc_percent = 50.0;
        state.battery.max_charge_kw = 3.0;
        state.battery.max_soc = 100.0;

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(12),
            dt_hours: 30.0 / 3600.0,
        };
        inv.update(&ctx, &mut state);

        // Excess = 4000W, max charge = 3000W → 3000W to battery, 1000W to grid
        assert!(
            state.battery.power_kw > 0.0,
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
        state.battery.soc_percent = 80.0;
        state.battery.max_discharge_kw = 3.0;
        state.battery.min_soc = 10.0;

        let mut inv = InverterEngine::new();
        let ctx = TickContext {
            now: ts(20),
            dt_hours: 30.0 / 3600.0,
        };
        inv.update(&ctx, &mut state);

        // Deficit 2500W, battery can supply → battery covers it
        assert!(
            state.battery.power_kw < 0.0,
            "Battery should be discharging"
        );
        assert!(
            state.grid.power_w <= 0.01,
            "Grid should not be importing if battery can cover load, got {}",
            state.grid.power_w
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
}
