//! EVC state-machine smoke test — reproduces the GUI Start button flow.
//!
//! The EVC's `active_power_w` is added to `state.load.demand_w` (the
//! household load bucket) so the InverterEngine / BatteryEngine / grid
//! balance treats EV charging like any other appliance. This means the
//! energy_totals.load_consumption_kwh bucket counts the EV's draw toward
//! today's household load — not toward grid_import_kwh directly.

use sim_core::{EvcEngine, TickContext};
use sim_models::{DeviceModel, PlantState};

fn ts() -> chrono::NaiveDateTime {
    chrono::NaiveDate::from_ymd_opt(2025, 6, 15)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap()
}

#[test]
fn evc_starts_charging_on_start_command_when_plugged() {
    let mut state = PlantState::new(ts());
    let ctx = TickContext {
        now: ts(),
        dt_hours: 1.0 / 60.0,
    };
    let mut eng = EvcEngine::new();

    // GUI: enable + plug cable
    state.evc.enabled = true;
    state.evc.connection_status = 1;

    // Tick 1: Idle (1) → Connected (2)
    eng.update(&ctx, &mut state);
    assert_eq!(
        state.evc.charging_state, 2,
        "Idle+plug should go to Connected"
    );

    // GUI: click ▶ Start → charge_control = 1
    state.evc.charge_control = 1;

    // Tick 2: Connected (2) → Starting (3)
    eng.update(&ctx, &mut state);
    assert_eq!(
        state.evc.charging_state, 3,
        "Connected+start should go to Starting"
    );

    // Tick 3: Starting (3) → Charging (4)
    eng.update(&ctx, &mut state);
    assert_eq!(
        state.evc.charging_state, 4,
        "Starting should go to Charging"
    );
    let charge_power = state.evc.active_power_w;
    assert!(
        charge_power > 0.0,
        "Charging should produce power, got {}",
        charge_power
    );
    // The EV's draw is rolled into state.load.demand_w so the inverter
    // treats EV charging like any other appliance: it sees the inflated
    // load and routes spare solar/battery output to the EV first via the
    // standard `solar - load` priority logic.
    assert!(
        state.load.demand_w > 0.0,
        "Charging should add to household load.demand_w, got {}",
        state.load.demand_w
    );

    // Tick 4: still charging
    eng.update(&ctx, &mut state);
    assert_eq!(state.evc.charging_state, 4, "Should remain Charging");
    assert!(state.evc.active_power_w > 0.0, "Still producing power");
}

#[test]
fn evc_start_does_nothing_when_not_plugged() {
    let mut state = PlantState::new(ts());
    let ctx = TickContext {
        now: ts(),
        dt_hours: 1.0 / 60.0,
    };
    let mut eng = EvcEngine::new();
    state.evc.enabled = true;
    state.evc.charge_control = 1;
    eng.update(&ctx, &mut state);
    assert_eq!(
        state.evc.charging_state, 1,
        "Start with no cable → still Idle"
    );
    assert_eq!(state.evc.active_power_w, 0.0);
    // EV contributes nothing to the household load when not charging.
    assert_eq!(
        state.load.demand_w, 0.0,
        "EVC must not affect load.demand_w when not charging"
    );
}

#[test]
fn evc_stop_returns_to_end_of_charging() {
    // Run a mini device chain (Load → Evc) every tick to mimic the real
    // engine flow. Without LoadEngine overwriting state.load.demand_w on
    // each tick, the EvcEngine's "baseline + ev_draw" reassignment would
    // inherit a stale baseline from the previous tick (which already
    // included the EV's draw), producing a phantom residual load after
    // the EV stops. LoadEngine is what gives us a fresh household
    // baseline each tick.
    use sim_core::{LoadEngine, LoadProfile};
    let mut state = PlantState::new(ts());
    let ctx = TickContext {
        now: ts(),
        dt_hours: 1.0 / 60.0,
    };
    let mut load = LoadEngine::new(LoadProfile::Minimal);
    let mut eng = EvcEngine::new();
    state.evc.enabled = true;
    state.evc.connection_status = 1;
    load.update(&ctx, &mut state);
    eng.update(&ctx, &mut state); // → Connected
    state.evc.charge_control = 1;
    load.update(&ctx, &mut state);
    eng.update(&ctx, &mut state); // → Starting
    load.update(&ctx, &mut state);
    eng.update(&ctx, &mut state); // → Charging
    assert_eq!(state.evc.charging_state, 4);
    let ev_draw = state.evc.active_power_w;
    assert!(
        ev_draw > 0.0,
        "EV should be drawing power while charging, got {ev_draw}"
    );
    // The Minimal load profile at noon ≈ 200 W. With the EV charging,
    // load.demand_w should reflect household + EV.
    let household_baseline = 200.0;
    assert!(
        (state.load.demand_w - (household_baseline + ev_draw)).abs() < 1.0,
        "household load ({household_baseline} W) + EV draw ({ev_draw} W) should equal {} W, got {} W",
        household_baseline + ev_draw,
        state.load.demand_w
    );
    state.evc.charge_control = 2; // Stop
    load.update(&ctx, &mut state);
    eng.update(&ctx, &mut state);
    assert_eq!(
        state.evc.charging_state, 6,
        "Stop should go to End of Charging"
    );
    assert_eq!(state.evc.active_power_w, 0.0);
    // Once the EV stops drawing, its share of load.demand_w disappears
    // too — load.demand_w should equal just the household baseline.
    assert!(
        (state.load.demand_w - household_baseline).abs() < 1.0,
        "EV should no longer contribute to load.demand_w after Stop; expected household baseline {household_baseline} W, got {} W",
        state.load.demand_w
    );
}

#[test]
fn evc_charging_power_routes_through_inverter_load_balance() {
    // End-to-end: an EV charging at ~7.7 kW must be added to the household
    // load bucket so the inverter's solar/battery/grid priority logic
    // treats it like any other appliance. Spare solar/battery output is
    // naturally routed to the EV first via the standard `solar - load`
    // balance; only the residual shortfall falls back to grid import.
    //
    // The GivEVC is wired to the AC bus the inverter shares with the
    // grid, so its draw flows through the inverter's balance logic — not
    // as a raw grid import layered on top of the inverter's own calc.
    use sim_core::{InverterEngine, LoadEngine, LoadProfile, SolarEngine};
    let ctx = TickContext {
        now: ts(),
        dt_hours: 30.0 / 3600.0,
    };
    let mut state = PlantState::new(ts());
    // Pin the household load at zero via override so the only demand on
    // the system is the EV.
    state.load_override = Some(0.0);
    // Zero battery state — we want the inverter to source the EV draw
    // from the grid, not from a battery that could absorb part of it.
    state.batteries[0].soc_percent = 0.0;
    state.batteries[0].min_soc = 0.0;
    state.sync_battery_from_vec();

    // Drive the EV into Charging state directly (the state machine tests
    // above cover the plug+start sequence).
    state.evc.enabled = true;
    state.evc.connection_status = 1;
    state.evc.charge_control = 1;
    state.evc.charging_state = 4; // already charging

    // Run the device chain in the same order the engine uses:
    // Solar → Load → Evc → Inverter
    let mut solar = SolarEngine::new(0.0, 51.5);
    let mut load = LoadEngine::new(LoadProfile::Minimal);
    let mut evc = EvcEngine::new();
    let mut inv = InverterEngine::new();

    solar.update(&ctx, &mut state);
    load.update(&ctx, &mut state);
    let baseline_load = state.load.demand_w; // override → 0
    evc.update(&ctx, &mut state);
    let ev_draw = state.evc.active_power_w; // 32A × 241V ≈ 7712 W

    // After EvcEngine, load.demand_w must equal baseline + EV draw.
    assert!(
        ev_draw > 0.0,
        "EV must be drawing power in Charging state, got {ev_draw} W"
    );
    assert!(
        (state.load.demand_w - (baseline_load + ev_draw)).abs() < 1.0,
        "load.demand_w should be baseline ({baseline_load} W) + EV ({ev_draw} W) = {} W, got {} W",
        baseline_load + ev_draw,
        state.load.demand_w
    );

    inv.update(&ctx, &mut state);

    // With zero solar, zero baseline household load, no battery activity,
    // the inverter must source the entire EV draw from the grid. The
    // combined grid+battery power must equal the EV draw, with no
    // double-counting from EvcEngine ever touching grid.power_w directly.
    let grid_w = state.grid.power_w;
    let charge_kw = state.total_battery_power_kw();
    let total_sourced_w = grid_w + charge_kw * 1000.0;
    let total_demand_w = baseline_load + ev_draw;
    assert!(
        (total_sourced_w - total_demand_w).abs() < 1.0,
        "system must source the full EV demand ({total_demand_w:.1} W) from grid+battery, got {total_sourced_w:.1} W (grid={grid_w:.1} W, charge={charge_kw:.3} kW)"
    );
}

#[test]
fn evc_charging_uses_spare_solar_before_grid_import() {
    // The user-visible requirement: if the inverter has spare solar to
    // export, the EV should self-consume that export first, and only the
    // residual shortfall becomes grid import. Without this, the meter
    // records both an export AND an import — wasteful round-tripping.
    use sim_core::{InverterEngine, LoadEngine, LoadProfile, SolarEngine};
    let ctx = TickContext {
        now: ts(),
        dt_hours: 30.0 / 3600.0,
    };
    let mut state = PlantState::new(ts());
    // 8 kW solar, 0 baseline household load, full battery (so the surplus
    // would otherwise export). EV charges at ~7.7 kW.
    state.solar_override = Some(8000.0);
    state.load_override = Some(0.0);
    state.batteries[0].soc_percent = 100.0;
    state.batteries[0].max_soc = 100.0;
    state.sync_battery_from_vec();

    state.evc.enabled = true;
    state.evc.connection_status = 1;
    state.evc.charge_control = 1;
    state.evc.charging_state = 4; // already charging

    let mut solar = SolarEngine::new(0.0, 51.5);
    let mut load = LoadEngine::new(LoadProfile::Minimal);
    let mut evc = EvcEngine::new();
    let mut inv = InverterEngine::new();
    solar.update(&ctx, &mut state);
    load.update(&ctx, &mut state);
    evc.update(&ctx, &mut state);
    inv.update(&ctx, &mut state);

    let ev_draw = state.evc.active_power_w;
    // The inverter should be exporting the small residual (8 kW solar −
    // 7.7 kW EV draw ≈ 0.3 kW). Critically, the grid import must NOT be
    // a positive 7.7 kW (which would be the bug — EV drawing raw from
    // grid instead of self-consuming the solar surplus).
    let grid_w = state.grid.power_w;
    assert!(
        grid_w < 0.0,
        "with 8 kW solar and ~7.7 kW EV draw, grid should be exporting the small residual (~0.3 kW), got {grid_w} W — EV may not be self-consuming the solar surplus"
    );
    // And the export magnitude must be smaller than the surplus before
    // the EV started (8 kW solar − 0 baseline ≈ 8 kW). It should be
    // reduced by exactly the EV's draw.
    let residual_export = -grid_w;
    let expected_residual = 8000.0 - ev_draw;
    assert!(
        (residual_export - expected_residual).abs() < 1.0,
        "residual export should be solar ({}) − EV draw ({}) = {:.1} W, got {:.1} W",
        8000.0,
        ev_draw,
        expected_residual,
        residual_export
    );
}
