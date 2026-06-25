//! Energy-balance regression test for the EVC integration.
//!
//! Reproduces the user-reported scenario: Solar 3.5 kW, Grid ?, Home 12.7 kW
//! (5 kW household + 7.7 kW EV), Battery ?, EV 7.7 kW. The bug being pinned
//! is that the engine was producing an unbalanced system because the EV's
//! draw was not flowing through the inverter's solar/battery/grid balance.
//!
//! Correct behaviour with EvcEngine ordered between LoadEngine and
//! InverterEngine:
//! * `state.load.demand_w` = 12.7 kW (household 5 kW + EV 7.7 kW)
//! * `state.grid.power_w` ≈ +6.2 kW (import the residual after battery
//!   discharges its 3 kW Gen3Hybrid hardware limit)
//! * `state.total_battery_power_kw()` ≈ -3 kW (discharging at the hardware
//!   cap, not the user's reported -1.5 kW)
//!
//! Energy balance must hold exactly:
//!   solar + grid_import + battery_discharge = load
//!   3.5 + 6.2 + 3.0 = 12.7 kW

use sim_core::{
    BatteryEngine, EnergyTracker, EvcEngine, InverterEngine, LoadEngine, LoadProfile,
    ScheduleEngine, SolarEngine, TickContext,
};
use sim_models::{DeviceModel, PlantState, Schedule};

fn ts() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2025, 6, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
}

fn run_full_tick(state: &mut PlantState) {
    // Match the production device order in crates/sim-tauri/src/commands.rs:
    // Schedule → Solar → Load → EvcEngine → Inverter → Faults → Battery → EnergyTracker
    let mut sched = ScheduleEngine::new(Schedule::default());
    let mut solar = SolarEngine::new(state.config.solar_peak_watts, state.config.latitude);
    let mut load = LoadEngine::new(LoadProfile::Minimal);
    let mut evc = EvcEngine::new();
    let mut inv = InverterEngine::new();
    let mut batt = BatteryEngine::new();
    let mut tracker = EnergyTracker::new().with_last_reset_date(state.timestamp.date());

    let ctx = TickContext {
        now: state.timestamp,
        dt_hours: 1.0 / 60.0,
    };
    sched.update(&ctx, state);
    solar.update(&ctx, state);
    load.update(&ctx, state);
    evc.update(&ctx, state);
    inv.update(&ctx, state);
    batt.update(&ctx, state);
    tracker.update(&ctx, state);
}

#[test]
fn user_reported_scenario_balances_correctly() {
    // Exact numbers from the user: Solar 3.5 kW, Home 12.7 kW, EV 7.7 kW.
    // The reported Grid=0 W + Battery=-1.5 kW was an UNBALANCED state (only
    // 5 kW sourced for 12.7 kW of demand). The correct balanced state is:
    // Grid +6.2 kW, Battery -3 kW (Gen3Hybrid hardware discharge cap).
    let now = ts();
    let mut state = PlantState::new(now);
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.config.max_ac_watts = 5000.0;
    // Pin solar to 3.5 kW (mid-day generation).
    state.solar_override = Some(3500.0);
    // Pin baseline household load to 5 kW (so total load = 5 + 7.7 = 12.7).
    state.load_override = Some(5000.0);
    // Battery at 50% SOC so it can discharge.
    state.batteries[0].soc_percent = 50.0;
    state.batteries[0].min_soc = 5.0;
    state.sync_battery_from_vec();
    // EV charging.
    state.evc.enabled = true;
    state.evc.connection_status = 1;
    state.evc.charge_control = 1;
    state.evc.charging_state = 4; // already in Charging state

    run_full_tick(&mut state);

    let solar_w = state.solar.generation_w;
    let load_w = state.load.demand_w;
    let ev_w = state.evc.active_power_w;
    let grid_w = state.grid.power_w;
    let batt_kw = state.total_battery_power_kw();
    let batt_w = batt_kw * 1000.0;

    // Print the actual values for diagnostics. Tolerances in the asserts
    // below account for the EV's exact current × voltage draw (32A × 241V =
    // 7712 W, not exactly 7700) and floating-point rounding.
    eprintln!(
        "solar={solar_w:.0}W load={load_w:.0}W EV={ev_w:.0}W grid={grid_w:.0}W batt={batt_w:.0}W"
    );

    // Pin the exact values the user observed so this regression test fails
    // loudly if the device order is ever broken again. Tolerances of ±50 W
    // account for the EV's exact current × voltage draw (32A × 241V = 7712 W,
    // not exactly 7700) and floating-point rounding.
    assert!(
        (solar_w - 3500.0).abs() < 1.0,
        "solar should be 3500 W, got {solar_w:.1}"
    );
    // The EV draws 32A × 241V = 7712 W, so total load = 5000 + 7712 = 12712 W.
    assert!(
        (load_w - 12712.0).abs() < 5.0,
        "load.demand_w must include EV: expected 12712 W (5000 + 7712), got {load_w:.1}"
    );
    assert!(
        (ev_w - 7712.0).abs() < 5.0,
        "EV should draw ~7712 W (32A × 241V), got {ev_w:.1}"
    );
    // Battery must discharge at its hardware cap (3 kW for Gen3Hybrid).
    // The user reported -1.5 kW which was the unbalance symptom.
    assert!(
        (batt_w - (-3000.0)).abs() < 50.0,
        "battery should discharge ~3 kW (Gen3Hybrid hardware cap), got {batt_w:.1} W — \
         if you see ~-1500 W here, the inverter is seeing only household load \
         (5 kW), not the combined household + EV load (12.7 kW), meaning \
         EvcEngine ran AFTER InverterEngine in the device chain"
    );
    // Grid must import the residual deficit after the battery hits its cap.
    // deficit = load - solar = 12712 - 3500 = 9212 W. Battery covers 3000 W.
    // Grid covers 9212 - 3000 = 6212 W.
    assert!(
        grid_w > 5000.0,
        "grid must import >5 kW to balance the EV demand, got {grid_w:.1} W — \
         if you see 0 W here, the inverter computed the balance with only \
         the household load (5 kW) and ignored the EV draw entirely"
    );
    assert!(
        (grid_w - 6212.0).abs() < 50.0,
        "grid import should be ~6.2 kW, got {grid_w:.1} W"
    );

    // Energy balance: source == sink.
    // Source = solar + grid_import + battery_discharge
    let grid_import_w = grid_w.max(0.0);
    let batt_discharge_w = (-batt_w).max(0.0);
    let total_source_w = solar_w + grid_import_w + batt_discharge_w;
    // Sink = load (household + EV) + battery_charge + grid_export
    let grid_export_w = (-grid_w).max(0.0);
    let batt_charge_w = batt_w.max(0.0);
    let total_sink_w = load_w + batt_charge_w + grid_export_w;
    let imbalance = total_source_w - total_sink_w;
    eprintln!("source = {total_source_w:.1} W");
    eprintln!("sink   = {total_sink_w:.1} W");
    eprintln!("imbalance = {imbalance:.2} W");
    assert!(
        imbalance.abs() < 1.0,
        "energy balance must close: source={total_source_w:.2} W, sink={total_sink_w:.2} W, \
         imbalance={imbalance:.2} W"
    );
}

#[test]
fn spare_solar_routes_to_ev_before_exporting() {
    // With 8 kW solar, 0 kW household load, full battery, the EV should
    // self-consume the surplus before any export reaches the grid.
    let now = ts();
    let mut state = PlantState::new(now);
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.config.max_ac_watts = 5000.0;
    state.solar_override = Some(8000.0);
    state.load_override = Some(0.0);
    state.batteries[0].soc_percent = 100.0;
    state.batteries[0].max_soc = 100.0;
    state.sync_battery_from_vec();
    state.evc.enabled = true;
    state.evc.connection_status = 1;
    state.evc.charge_control = 1;
    state.evc.charging_state = 4;

    run_full_tick(&mut state);

    let solar_w = state.solar.generation_w;
    let load_w = state.load.demand_w;
    let ev_w = state.evc.active_power_w;
    let grid_w = state.grid.power_w;

    eprintln!("=== Spare-solar scenario ===");
    eprintln!("solar  = {solar_w:.0} W");
    eprintln!("load   = {load_w:.0} W");
    eprintln!("EV     = {ev_w:.0} W");
    eprintln!("grid   = {grid_w:.0} W (expected ~+300 export residual)");

    // Total load should be just the EV draw (~7.7 kW).
    assert!(
        (load_w - ev_w).abs() < 1.0,
        "load must equal EV draw when household override is 0, got {load_w:.1} W"
    );
    // Solar (8 kW) − load (7.7 kW) = small residual export.
    // Grid should be EXPORTING (negative) the ~300 W residual.
    let expected_export = -(solar_w - ev_w);
    assert!(
        (grid_w - expected_export).abs() < 50.0,
        "grid should export the small residual (~{:.0} W), got {:.1} W",
        expected_export,
        grid_w
    );
    assert!(
        grid_w < 0.0,
        "with spare solar and an EV load, grid should be exporting, got {grid_w:.1} W"
    );
}

#[test]
fn idle_ev_does_not_affect_load_or_grid() {
    // When EVC isn't charging, EvcEngine must be a no-op. load.demand_w must
    // match the household baseline, and grid/solar/battery must behave as if
    // the EV weren't there at all.
    let now = ts();
    let mut state = PlantState::new(now);
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.config.max_ac_watts = 5000.0;
    state.solar_override = Some(3500.0);
    state.load_override = Some(2000.0); // 2 kW household
    state.batteries[0].soc_percent = 50.0;
    state.sync_battery_from_vec();
    // EVC enabled but cable unplugged — stays Idle.
    state.evc.enabled = true;
    state.evc.connection_status = 0;
    state.evc.charge_control = 0;
    state.evc.charging_state = 1; // Idle

    run_full_tick(&mut state);

    let load_w = state.load.demand_w;
    let ev_w = state.evc.active_power_w;
    assert!(ev_w.abs() < 0.01, "idle EV must draw 0 W, got {ev_w:.3}");
    assert!(
        (load_w - 2000.0).abs() < 1.0,
        "idle EV must not affect load.demand_w: expected 2000 W, got {load_w:.1}"
    );
}

#[test]
fn stopping_ev_restores_baseline_load() {
    // Charging → Stop → load must return to baseline (no stale EV contribution).
    let now = ts();
    let mut state = PlantState::new(now);
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.config.max_ac_watts = 5000.0;
    state.solar_override = Some(3500.0);
    state.load_override = Some(2000.0);
    state.batteries[0].soc_percent = 50.0;
    state.sync_battery_from_vec();
    state.evc.enabled = true;
    state.evc.connection_status = 1;
    state.evc.charge_control = 1;
    state.evc.charging_state = 4; // charging

    // Tick 1: should be charging.
    run_full_tick(&mut state);
    let load_while_charging = state.load.demand_w;
    let ev_while_charging = state.evc.active_power_w;
    assert!(
        (load_while_charging - (2000.0 + ev_while_charging)).abs() < 1.0,
        "while charging, load must be household (2000) + EV ({ev_while_charging}), got {load_while_charging}"
    );

    // User clicks Stop.
    state.evc.charge_control = 2;

    // Tick 2: state machine transitions Charging → End of Charging (6),
    // and EvcEngine must restore load.demand_w to the baseline.
    run_full_tick(&mut state);
    let load_after_stop = state.load.demand_w;
    let ev_after_stop = state.evc.active_power_w;
    assert_eq!(
        state.evc.charging_state, 6,
        "expected End of Charging (6) after Stop, got {}",
        state.evc.charging_state
    );
    assert!(
        ev_after_stop.abs() < 0.01,
        "EV must draw 0 W after Stop, got {ev_after_stop:.3}"
    );
    assert!(
        (load_after_stop - 2000.0).abs() < 1.0,
        "after Stop, load must return to household baseline (2000), got {load_after_stop:.1} — \
         if you see ~9700 here, the previous tick's EV contribution was carried forward"
    );
}

/// Run the BROKEN device order (Load → Inverter → Battery → EvcEngine) to
/// prove the regression test catches the imbalance. This is the order that
/// existed in `lib.rs` and `sim-api/src/main.rs` until the fix: EvcEngine
/// was placed AFTER BatteryEngine. With this order, the inverter and
/// battery balance only sees the household load (5 kW), then EvcEngine
/// bumps load.demand_w to 12.7 kW but no one rebalances — so the displayed
/// numbers are solar=3500, load=12712, grid=0, battery=-1500. UNBALANCED:
/// only 5 kW sourced for 12.7 kW of demand. This is exactly the user's
/// report.
#[test]
fn broken_device_order_produces_unbalanced_state() {
    let now = ts();
    let mut state = PlantState::new(now);
    state.config.inverter_type = "Gen3Hybrid".to_string();
    state.config.max_ac_watts = 5000.0;
    state.solar_override = Some(3500.0);
    state.load_override = Some(5000.0);
    state.batteries[0].soc_percent = 50.0;
    state.batteries[0].min_soc = 5.0;
    state.sync_battery_from_vec();
    state.evc.enabled = true;
    state.evc.connection_status = 1;
    state.evc.charge_control = 1;
    state.evc.charging_state = 4;

    // BROKEN order: EvcEngine AFTER BatteryEngine — the original bug.
    // The inverter and battery see load=5kW, then EvcEngine bumps load to
    // 12.7kW but no device rebalances after that.
    let ctx = TickContext {
        now: state.timestamp,
        dt_hours: 1.0 / 60.0,
    };
    let mut solar = SolarEngine::new(state.config.solar_peak_watts, state.config.latitude);
    let mut load = LoadEngine::new(LoadProfile::Minimal);
    let mut inv = InverterEngine::new();
    let mut batt = BatteryEngine::new();
    let mut evc = EvcEngine::new();
    let mut tracker = EnergyTracker::new().with_last_reset_date(now.date());
    solar.update(&ctx, &mut state);
    load.update(&ctx, &mut state);
    inv.update(&ctx, &mut state); // inverter sees load = 5 kW (household only)
    batt.update(&ctx, &mut state); // battery sees grid power = 0
    evc.update(&ctx, &mut state); // EV bumps load to 12.7 kW but no rebalance
    tracker.update(&ctx, &mut state);

    let solar_w = state.solar.generation_w;
    let load_w = state.load.demand_w;
    let grid_w = state.grid.power_w;
    let batt_w = state.total_battery_power_kw() * 1000.0;

    // The inverter ran with load=5kW, so:
    //   net = 3.5 - 5 = -1.5 → deficit 1.5 kW covered by battery (limit 3 kW).
    //   grid = 0, battery = -1.5 kW.
    // EvcEngine then bumped load.demand_w to 12.7 kW AFTER everyone had
    // already balanced — so the displayed numbers match the user's report:
    // solar=3500, load=12712, grid=0, battery=-1500. UNBALANCED: only
    // 5 kW sourced for 12.7 kW of demand.
    let src = solar_w + grid_w.max(0.0) + (-batt_w).max(0.0);
    let sink = load_w + batt_w.max(0.0) + (-grid_w).max(0.0);
    let imbalance = (src - sink).abs();
    eprintln!("=== BROKEN order (Load → Inverter → Battery → EvcEngine) ===");
    eprintln!("solar   = {solar_w:.0} W");
    eprintln!("load    = {load_w:.0} W");
    eprintln!("grid    = {grid_w:.0} W");
    eprintln!("battery = {batt_w:.0} W");
    eprintln!("source  = {src:.0} W, sink = {sink:.0} W, imbalance = {imbalance:.0} W");

    // Verify the broken order actually IS broken (significant imbalance).
    // If this test ever fails, it means a future refactor has accidentally
    // fixed the imbalance in the broken-order path too, and the
    // regression test above is no longer the only thing catching the bug.
    assert!(
        imbalance > 5000.0,
        "expected the broken device order to produce a significant imbalance \
         (>5 kW), got {imbalance:.1} W — if this fails, the broken order is no \
         longer broken, and the regression test for the correct order may no \
         longer be detecting the bug it was written for. Investigate."
    );
    // And confirm the symptoms match the user's report exactly.
    assert!(
        (batt_w - (-1500.0)).abs() < 200.0,
        "broken order should produce ~-1.5 kW battery discharge (user-reported), got {batt_w:.0} W"
    );
    assert!(
        grid_w.abs() < 100.0,
        "broken order should produce ~0 W grid (user-reported), got {grid_w:.0} W"
    );
    assert!(
        (load_w - 12712.0).abs() < 50.0,
        "broken order still shows load as 12.7 kW (because EvcEngine adds to load), got {load_w:.0} W"
    );
}
