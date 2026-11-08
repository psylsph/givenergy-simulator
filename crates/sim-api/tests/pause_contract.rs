//! Every model that takes the battery pause actually pauses charging when
//! written the way Home Energy Manager writes it (home-energy-manager #433).
//!
//! The register store decides which of HR 318-320 a model accepts
//! (`sim-registers`), and the battery engine decides what the stored pause
//! does (`sim-core`). They live in different crates, so a model can take the
//! write and still carry on charging: the Gen1 took HR 318 alone, but the
//! engine still waited for a window the Gen1 never gets. This walks the
//! GivTCP contract's rows through both.

use chrono::NaiveDate;
use serde_json::Value;
use sim_core::BatteryEngine;
use sim_models::{DeviceModel, PlantState, TickContext};
use sim_registers::{RegisterStore, default_register_catalogue};

fn contract() -> Value {
    serde_json::from_str(include_str!(
        "../../sim-registers/tests/fixtures/givtcp-model-contract.json"
    ))
    .expect("the contract is valid JSON")
}

/// What HEM does for a row: GivTCP's pause, unless HEM records a difference.
fn hem_pause(row: &Value) -> &str {
    row["hem_deviations"]["pause"]["value"]
        .as_str()
        .or_else(|| row["givtcp"]["pause"].as_str())
        .expect("a pause value")
}

/// A plant of one contract row, charging or discharging at 2 kW, with its
/// register store projected once so HR 21 holds the ARM firmware.
fn plant_for(
    sim: &str,
    arm_fw: u16,
    battery_kw: f64,
    ts: chrono::NaiveDateTime,
) -> (PlantState, RegisterStore) {
    let mut state = PlantState::with_battery_count(ts, 1);
    state.config.inverter_type = sim.to_string();
    state.inverter.arm_firmware_version = arm_fw;
    state.battery_charge_limit_percent = 100.0;
    state.battery_discharge_limit_percent = 100.0;
    state.batteries[0].soc_percent = 50.0;
    state.batteries[0].capacity_kwh = 10.0;
    state.batteries[0].nominal_capacity_kwh = 10.0;
    state.batteries[0].max_charge_kw = 5.0;
    state.batteries[0].max_discharge_kw = 5.0;
    state.batteries[0].temperature_celsius = 25.0;
    state.batteries[0].power_kw = battery_kw;
    state.sync_battery_from_vec();
    let mut store = RegisterStore::new(default_register_catalogue());
    store.project_from_state(&state);
    (state, store)
}

/// What HEM's pause writes for `mode`: the mode alone, or an all-day window
/// and then the mode (HEM sets the window first so no pause runs with the
/// wrong one).
fn hem_writes(kind: &str, mode: u16) -> Vec<(u16, u16)> {
    match kind {
        "mode" => vec![(318, mode)],
        _ => vec![(319, 0), (320, 2359), (318, mode)],
    }
}

#[test]
fn every_model_that_takes_the_pause_stops_the_battery_with_it() {
    let contract = contract();
    let ts = NaiveDate::from_ymd_opt(2025, 6, 21)
        .unwrap()
        .and_hms_opt(14, 0, 0)
        .unwrap();
    let rows = contract["rows"].as_array().expect("rows");
    let expected = rows
        .iter()
        .filter(|row| row["sim"].is_string() && hem_pause(row) != "none")
        .count();
    assert!(expected > 0, "the contract names no model with a pause");

    // (mode, battery kW in the direction it pauses): 1 charge, 2 discharge,
    // 3 both.
    let cases = [(1u16, 2.0), (2, -2.0), (3, 2.0), (3, -2.0)];
    let mut problems = Vec::new();
    let mut checked = 0;
    for row in rows {
        let Some(sim) = row["sim"].as_str() else {
            continue;
        };
        let kind = hem_pause(row);
        if kind == "none" {
            continue;
        }
        let arm_fw = row["arm_fw"].as_u64().expect("arm_fw") as u16;
        for (mode, battery_kw) in cases {
            let label = format!("{sim} ARM {arm_fw} ({kind}) mode {mode} at {battery_kw} kW");
            let (mut state, mut store) = plant_for(sim, arm_fw, battery_kw, ts);
            let writes = hem_writes(kind, mode);
            let mut accepted = true;
            for &(address, value) in &writes {
                if !store.write(address, value) {
                    problems.push(format!("{label}: the store refused HR {address}"));
                    accepted = false;
                }
            }
            if !accepted {
                continue;
            }
            // The same merge the simulator applies to its command queue:
            // what wasn't written keeps the plant's value.
            let written = |address: u16, fallback: u16| {
                writes
                    .iter()
                    .find(|(a, _)| *a == address)
                    .map_or(fallback, |&(_, v)| v)
            };
            state.battery_pause_mode = written(318, state.battery_pause_mode);
            state.battery_pause_slot_start = written(319, state.battery_pause_slot_start);
            state.battery_pause_slot_end = written(320, state.battery_pause_slot_end);

            let ctx = TickContext {
                now: ts,
                dt_hours: 1.0,
            };
            BatteryEngine::new().update(&ctx, &mut state);
            if state.batteries[0].power_kw != 0.0 {
                problems.push(format!(
                    "{label}: the battery carried on at {} kW with the pause written",
                    state.batteries[0].power_kw
                ));
            }
        }
        checked += 1;
    }
    assert_eq!(checked, expected, "every pause row was walked");
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn a_model_givtcp_gives_no_pause_refuses_every_pause_register() {
    // The other half of the contract: where GivTCP gives the model no pause,
    // the store must refuse it, as the real model does. (GivTCP's column,
    // not HEM's: HEM also withholds the pause on the Gateway, which keeps its
    // own copy of the registers.)
    let contract = contract();
    let ts = NaiveDate::from_ymd_opt(2025, 6, 21)
        .unwrap()
        .and_hms_opt(14, 0, 0)
        .unwrap();
    let mut problems = Vec::new();
    let mut checked = 0;
    for row in contract["rows"].as_array().expect("rows") {
        let Some(sim) = row["sim"].as_str() else {
            continue;
        };
        // A recorded simulator deviation (the AC three-phase) is the one
        // knowing exception; `sim-registers`' contract test owns it.
        if row["givtcp"]["pause"] != "none" || row["sim_deviations"]["pause"].is_object() {
            continue;
        }
        let arm_fw = row["arm_fw"].as_u64().expect("arm_fw") as u16;
        let (_, mut store) = plant_for(sim, arm_fw, 2.0, ts);
        for (address, value) in [(319, 0), (320, 2359), (318, 1)] {
            if store.write(address, value) {
                problems.push(format!("{sim} ARM {arm_fw}: HR {address} was accepted"));
            }
        }
        checked += 1;
    }
    assert!(checked > 0, "the contract names no model without a pause");
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}

#[test]
fn the_store_and_the_engine_agree_where_the_contract_has_no_row() {
    // The Hybrid4600/3600 have no contract row (they are resolved by ARM
    // firmware century), and any model can have its firmware overridden. For
    // each, what the store accepts must be what the engine then pauses:
    // accepted -> paused, refused -> not forced to pause by the mode alone.
    use sim_models::{PauseSupport, pause_support};
    let ts = NaiveDate::from_ymd_opt(2025, 6, 21)
        .unwrap()
        .and_hms_opt(14, 0, 0)
        .unwrap();
    let cases: &[(&str, &[u16])] = &[
        ("Hybrid4600", &[0, 186, 187, 252, 318, 399, 400, 852, 999]),
        ("Hybrid3600", &[0, 186, 187, 252, 318, 852]),
        ("Gen1Hybrid", &[0, 186, 187, 252, 852]),
        ("ACCoupled", &[0, 199, 200, 318]),
        ("ACCoupled2", &[0, 199, 200]),
        ("Gen2Hybrid", &[0, 186, 852]),
        ("Gen3Hybrid", &[0, 252, 318]),
    ];
    let mut problems = Vec::new();
    let mut taken = 0;
    for &(model, firmwares) in cases {
        for &arm_fw in firmwares {
            let support = pause_support(model, arm_fw);
            let label = format!("{model} ARM {arm_fw} ({support:?})");
            let (mut state, mut store) = plant_for(model, arm_fw, 2.0, ts);
            let writes: &[(u16, u16)] = match support {
                PauseSupport::None => &[(318, 1)],
                PauseSupport::ModeOnly => &[(318, 1)],
                PauseSupport::ModeAndWindow => &[(319, 0), (320, 2359), (318, 1)],
            };
            let accepted = writes.iter().all(|&(a, v)| store.write(a, v));
            if accepted != (support != PauseSupport::None) {
                problems.push(format!("{label}: the store accepted = {accepted}"));
                continue;
            }
            if !accepted {
                continue;
            }
            taken += 1;
            for &(a, v) in writes {
                match a {
                    318 => state.battery_pause_mode = v,
                    319 => state.battery_pause_slot_start = v,
                    _ => state.battery_pause_slot_end = v,
                }
            }
            let ctx = TickContext {
                now: ts,
                dt_hours: 1.0,
            };
            BatteryEngine::new().update(&ctx, &mut state);
            if state.batteries[0].power_kw != 0.0 {
                problems.push(format!("{label}: the engine did not pause"));
            }
        }
    }
    assert!(taken > 10, "only {taken} combinations took the pause");
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
