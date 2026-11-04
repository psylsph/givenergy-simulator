//! The simulator's per-model registers against GivTCP's
//! (`tests/fixtures/givtcp-model-contract.json`, a copy of Home Energy
//! Manager's; HEM's `tests/scripts/givtcp-contract.test.sh` checks the two
//! are the same).
//!
//! The simulator stands in for real inverters, and GivTCP's model rules have
//! far more real hardware behind them than anything else, so the registers
//! a model takes here must be GivTCP's. Where they aren't yet, the contract
//! says so under `sim_deviations` with the issue that will fix it. Both an
//! unrecorded difference and a recorded one that has gone away fail.

use std::collections::BTreeMap;

use serde_json::{Value, json};
use sim_models::PlantState;
use sim_registers::{RegisterStore, default_register_catalogue};

fn contract() -> Value {
    serde_json::from_str(include_str!("fixtures/givtcp-model-contract.json"))
        .expect("the contract is valid JSON")
}

/// The store for one contract row, projected once so HR 21 holds its ARM
/// firmware.
fn store_for(sim: &str, arm_fw: u16) -> RegisterStore {
    let ts = chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
        .unwrap()
        .and_hms_opt(12, 0, 0)
        .unwrap();
    let mut state = PlantState::new(ts);
    state.config.inverter_type = sim.to_string();
    state.inverter.arm_firmware_version = arm_fw;
    state.sync_battery_from_vec();
    let mut store = RegisterStore::new(default_register_catalogue());
    store.project_from_state(&state);
    store
}

/// What the simulator does for a row, field by field.
fn sim_facts(sim: &str, arm_fw: u16) -> BTreeMap<String, Value> {
    let mut store = store_for(sim, arm_fw);
    let mode = store.write(318, 1);
    let window = store.write(319, 1200) && store.write(320, 1300);
    let pause = match (mode, window) {
        (true, true) => "mode_and_window",
        (true, false) => "mode",
        (false, false) => "none",
        (false, true) => "window_without_mode",
    };
    let three_phase = !store.holding_range_absent_for_device(1080, 45);
    BTreeMap::from([
        ("pause".to_string(), json!(pause)),
        ("three_phase".to_string(), json!(three_phase)),
    ])
}

#[test]
fn the_simulator_matches_givtcp_except_where_the_contract_says_why() {
    let contract = contract();
    let rows = contract["rows"].as_array().expect("rows");
    assert!(rows.len() >= 19, "the contract lost rows");
    let mut problems = Vec::new();
    let mut checked = 0;
    for row in rows {
        let Some(sim) = row["sim"].as_str() else {
            continue;
        };
        checked += 1;
        let arm_fw = row["arm_fw"].as_u64().expect("arm_fw") as u16;
        let label = format!("{sim} ARM {arm_fw}");
        let actual = sim_facts(sim, arm_fw);
        let deviations = row["sim_deviations"].as_object();
        for (field, wanted) in row["givtcp"].as_object().expect("givtcp") {
            let got = &actual[field];
            match deviations.and_then(|d| d.get(field)) {
                None if got != wanted => problems.push(format!(
                    "{label} {field}: the simulator does {got}, GivTCP {wanted}. Align it, or record it under sim_deviations with an issue"
                )),
                Some(deviation) if deviation["value"] == *wanted => problems.push(format!(
                    "{label} {field}: the recorded deviation is what GivTCP does; drop it"
                )),
                Some(deviation) if *got != deviation["value"] => problems.push(format!(
                    "{label} {field}: the simulator does {got}, not the recorded {} (#{}). If that's fixed, drop the deviation",
                    deviation["value"], deviation["issue"]
                )),
                _ => {}
            }
        }
        if let Some(deviations) = deviations {
            for (field, deviation) in deviations {
                if !actual.contains_key(field) {
                    problems.push(format!("{label}: unknown field {field}"));
                }
                if deviation["issue"].as_u64().unwrap_or(0) == 0
                    || deviation["why"].as_str().unwrap_or("").trim().is_empty()
                {
                    problems.push(format!(
                        "{label} {field}: a deviation needs an issue and a why"
                    ));
                }
            }
        }
    }
    assert!(checked >= 18, "only {checked} rows name a simulator model");
    assert!(problems.is_empty(), "\n{}", problems.join("\n"));
}
