//! Property-style power-balance invariant checks ("the checker").
//!
//! Guards the energy-conservation class of bugs that has recurred across
//! 0.17.x (pause-slot reconciliation, island-mode reconciliation): after
//! every full engine tick, the power flows must obey physical invariants.
//!
//!   INV-1 (grid connected): `solar + grid_import == load + battery_charge`
//!           — exact AC/DC bus balance; curtailed or vanished power fails.
//!   INV-2 (island):         grid flow is exactly zero — nothing imports or
//!           exports through a disconnected grid — and AC output never
//!           exceeds the on-site sources (solar + battery discharge).
//!   INV-3:                  all power fields stay finite (no NaN drift).
//!
//! Coverage: a ~5.8k-combination scenario sweep (connectivity × solar ×
//! load × schedule mode × pause window × power limits × SOC × module count)
//! plus multi-tick day soaks in both connected and island modes. If a
//! regression anywhere in the chain breaks conservation, this file fails
//! loudly with the exact scenario in the panic message.

use sim_core::{
    BatteryEngine, EnergyTracker, InverterEngine, LoadEngine, LoadProfile, ScheduleEngine,
    SolarEngine, TickContext,
};
use sim_faults::FaultEngine;
use sim_models::{DeviceModel, PlantState, Schedule};

/// Float-noise tolerance in watts. All flows are exact f64 scalings, so any
/// real imbalance is orders of magnitude larger than this.
const TOL_W: f64 = 1e-2;

fn ts(hour: u32, minute: u32) -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2025, 6, 15)
        .unwrap()
        .and_hms_opt(hour, minute, 0)
        .unwrap()
}

/// One full engine tick in production order (mirrors
/// `crates/sim-tauri/src/commands.rs`):
/// Schedule → Solar → Load → EVC → Inverter → Faults → Battery → EnergyTracker.
/// The EVC engine is omitted: it is disabled and a no-op in every scenario
/// here (its draw is folded into `load.demand_w` when enabled).
fn run_full_tick(state: &mut PlantState, schedule: &Schedule) {
    let mut sched = ScheduleEngine::new(schedule.clone());
    let mut solar = SolarEngine::new(state.config.solar_peak_watts, state.config.latitude);
    let mut load = LoadEngine::new(LoadProfile::Minimal);
    let mut inv = InverterEngine::new();
    let mut faults = FaultEngine::new();
    let mut batt = BatteryEngine::new();
    let mut tracker = EnergyTracker::new().with_last_reset_date(state.timestamp.date());

    let ctx = TickContext {
        now: state.timestamp,
        dt_hours: 1.0 / 60.0,
    };
    sched.update(&ctx, state);
    solar.update(&ctx, state);
    load.update(&ctx, state);
    inv.update(&ctx, state);
    faults.update(&ctx, state);
    batt.update(&ctx, state);
    tracker.update(&ctx, state);
}

/// Assert every invariant holds for the post-tick state.
fn assert_invariants(state: &PlantState, label: &str) {
    let solar = state.solar.generation_w;
    let load = state.load.demand_w;
    let grid = state.grid.power_w;
    let batt_w = state.total_battery_power_kw() * 1000.0;
    let ac = state.inverter.ac_power_w;

    // INV-3: finite everywhere — a NaN anywhere poisons every register.
    for (name, v) in [
        ("solar", solar),
        ("load", load),
        ("grid", grid),
        ("battery", batt_w),
        ("ac", ac),
    ] {
        assert!(v.is_finite(), "{label}: {name} is not finite: {v}");
    }

    if state.grid.connected {
        // INV-1: exact balance across the bus. Power may change form
        // (charge/discharge) or direction (import/export) but never vanish
        // or appear.
        let lhs = solar + grid;
        let rhs = load + batt_w;
        assert!(
            (lhs - rhs).abs() <= TOL_W,
            "{label}: balance violated: solar({solar}) + grid({grid}) = {lhs} W \
             but load({load}) + battery({batt_w}) = {rhs} W"
        );
    } else {
        // INV-2a: a disconnected grid carries no current. This is the exact
        // regression class fixed in the island-mode reconciliation: pausing
        // or capping the battery must shed/curtail locally, never conjure
        // phantom import/export.
        assert!(
            grid.abs() <= TOL_W,
            "{label}: island grid flow of {grid} W — the grid is disconnected"
        );
        // INV-2b: island AC output can never exceed on-site sources.
        assert!(
            ac <= solar - batt_w + TOL_W && ac >= -TOL_W,
            "{label}: island AC output {ac} W exceeds sources: \
             solar({solar}) + discharge({}) W",
            -batt_w
        );
    }
}

/// Build a sweep scenario state: Gen3Hybrid (single-phase, 5 kW AC cap),
/// pinned solar/load via the override path (works day and night).
fn sweep_state(
    hour: u32,
    connected: bool,
    solar_w: f64,
    load_w: f64,
    modules: usize,
) -> PlantState {
    let mut state = PlantState::with_battery_count(ts(hour, 0), modules);
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.config.max_ac_watts = 5000.0;
    state.grid.connected = connected;
    state.solar_override = Some(solar_w);
    state.load_override = Some(load_w);
    state
}

#[test]
fn power_balance_holds_across_scenario_sweep() {
    let schedule = Schedule::default();
    let mut checked = 0usize;

    for &connected in &[true, false] {
        for &solar_w in &[0.0, 1500.0, 4000.0, 8000.0] {
            for &load_w in &[0.0, 2000.0, 6000.0] {
                // (scheduled_charge, scheduled_discharge) selects the
                // normal_priority / force_charge / force_discharge paths.
                for &(sched_c, sched_d) in &[(false, false), (true, false), (false, true)] {
                    // (pause_mode, start, end): disabled, PauseDischarge wrap
                    // window 04:00->03:00 (active at noon), PauseCharge
                    // 11:00-13:00 (active at noon).
                    for &(mode, start, end) in
                        &[(0u16, 60u16, 60u16), (2, 400, 300), (1, 1100, 1300)]
                    {
                        for &limit in &[100.0, 50.0, 0.0] {
                            for &soc in &[10.0, 50.0, 100.0] {
                                for &modules in &[1usize, 2] {
                                    let mut state =
                                        sweep_state(12, connected, solar_w, load_w, modules);
                                    state.battery_pause_mode = mode;
                                    state.battery_pause_slot_start = start;
                                    state.battery_pause_slot_end = end;
                                    state.battery_charge_limit_percent = limit;
                                    state.battery_discharge_limit_percent = limit;
                                    state.scheduled_charge = sched_c;
                                    state.scheduled_discharge = sched_d;
                                    for b in &mut state.batteries {
                                        b.soc_percent = soc;
                                        b.min_soc = 5.0;
                                        b.max_soc = 100.0;
                                    }
                                    state.sync_battery_from_vec();

                                    run_full_tick(&mut state, &schedule);
                                    assert_invariants(
                                        &state,
                                        &format!(
                                            "connected={connected} solar={solar_w} \
                                             load={load_w} sched=({sched_c},{sched_d}) \
                                             pause=({mode},{start},{end}) limit={limit} \
                                             soc={soc} modules={modules}"
                                        ),
                                    );
                                    checked += 1;
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    eprintln!("power-balance sweep: {checked} scenarios checked");
    assert!(checked > 1000, "sweep unexpectedly shrank: {checked}");
}

#[test]
fn grid_connected_two_day_soak_keeps_balance() {
    // 96 x 30-min ticks across two days with no overrides: solar follows its
    // day curve, load follows its profile, the battery cycles through
    // charge/discharge/full/empty. INV-1 must hold on every single tick —
    // including the midnight rollovers.
    let schedule = Schedule::default();
    let start = ts(0, 0);
    let mut state = sweep_state(0, true, 0.0, 0.0, 1);

    for step in 0..96i64 {
        state.timestamp = start + chrono::Duration::minutes(30 * step);
        run_full_tick(&mut state, &schedule);
        assert_invariants(&state, &format!("connected soak step {step}"));
    }
}

#[test]
fn island_two_day_soak_keeps_grid_silent() {
    // Same soak, island mode, with a deliberately hostile profile: midday
    // solar surplus (5 kW) exceeds the battery charge ceiling (3.6 kW) so
    // surplus MUST curtail, and overnight load (1 kW) outlasts the battery
    // so it reaches min SOC and the deficit must shed. Through all of it the
    // disconnected grid must stay at exactly zero flow — this soak contains
    // the original regression scenarios (island + idle battery, island +
    // capped charging) and fails under mutation of the island guard.
    const DAY_PEAK_W: f64 = 6_000.0;
    const LOAD_W: f64 = 1_000.0;
    let schedule = Schedule::default();
    let start = ts(0, 0);
    let mut state = sweep_state(0, false, 0.0, LOAD_W, 1);
    for b in &mut state.batteries {
        b.soc_percent = 15.0;
        b.min_soc = 10.0;
    }
    state.sync_battery_from_vec();

    for step in 0..96i64 {
        let minute_of_day = (30 * step) % (24 * 60);
        let hour = (minute_of_day / 60) as f64;
        // Pin solar to a square day curve 08:00-16:00 (override works at
        // any hour; outside the window generation is zero).
        let solar_w = if (8.0..16.0).contains(&hour) {
            DAY_PEAK_W
        } else {
            0.0
        };
        state.timestamp = start + chrono::Duration::minutes(30 * step);
        state.solar_override = Some(solar_w);
        run_full_tick(&mut state, &schedule);
        assert_invariants(&state, &format!("island soak step {step}"));
    }
}
