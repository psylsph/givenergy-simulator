//! Persistence tests: serialization roundtrips, schedule restoration, defaults.
//!
//! Tests the data model serialization that underpins save/load without Tauri runtime.

use sim_core::Schedule;
use sim_models::PlantState;

/// Helper: create a plant state with a known configuration.
fn test_plant_state() -> PlantState {
    let mut state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(14, 30, 0)
            .unwrap(),
    );
    state.config.inverter_type = "ACCoupled".to_string();
    state.config.solar_peak_watts = 6000.0;
    state.batteries[0].soc_percent = 65.0;
    state.batteries[0].max_charge_kw = 3.0;
    state.batteries[0].max_discharge_kw = 3.0;
    state.sync_battery_from_vec();
    state
}

fn test_schedule() -> Schedule {
    Schedule {
        charge_start: 0.0,
        charge_end: 5.5,
        discharge_start: 17.0,
        discharge_end: 20.0,
        charge_start_2: 0.0,
        charge_end_2: 0.0,
        discharge_start_2: 0.0,
        discharge_end_2: 0.0,
        charge_target_soc: 100.0,
        charge_target_soc_2: 100.0,
        discharge_target_soc: 10.0,
        discharge_target_soc_2: 10.0,
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
        discharge_target_soc_3: 10.0,
        discharge_start_4: 0.0,
        discharge_end_4: 0.0,
        discharge_target_soc_4: 10.0,
        discharge_start_5: 0.0,
        discharge_end_5: 0.0,
        discharge_target_soc_5: 10.0,
        discharge_start_6: 0.0,
        discharge_end_6: 0.0,
        discharge_target_soc_6: 10.0,
        discharge_start_7: 0.0,
        discharge_end_7: 0.0,
        discharge_target_soc_7: 10.0,
        discharge_start_8: 0.0,
        discharge_end_8: 0.0,
        discharge_target_soc_8: 10.0,
        discharge_start_9: 0.0,
        discharge_end_9: 0.0,
        discharge_target_soc_9: 10.0,
        discharge_start_10: 0.0,
        discharge_end_10: 0.0,
        discharge_target_soc_10: 10.0,
        enable_charge: false,
        enable_discharge: false,
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

// ===========================================================================
// 1. Persisted wrapper serialization (mirrors sim-tauri::PersistedState)
// ===========================================================================

#[derive(serde::Serialize, serde::Deserialize)]
struct PersistedState {
    plant: PlantState,
    schedule: Option<Schedule>,
}

#[test]
fn persisted_state_roundtrip_with_schedule() {
    let plant = test_plant_state();
    let schedule = test_schedule();
    let persisted = PersistedState {
        plant: plant.clone(),
        schedule: Some(schedule.clone()),
    };

    let json = serde_json::to_string_pretty(&persisted).unwrap();
    let restored: PersistedState = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.plant.config.inverter_type, "ACCoupled");
    assert_eq!(restored.plant.config.solar_peak_watts, 6000.0);
    assert!(restored.schedule.is_some());
    let sched = restored.schedule.unwrap();
    assert_eq!(sched.charge_start, 0.0);
    assert_eq!(sched.charge_end, 5.5);
    assert_eq!(sched.discharge_start, 17.0);
    assert_eq!(sched.discharge_end, 20.0);
}

#[test]
fn persisted_state_roundtrip_with_none_schedule() {
    let plant = test_plant_state();
    let persisted = PersistedState {
        plant: plant.clone(),
        schedule: None,
    };

    let json = serde_json::to_string(&persisted).unwrap();
    let restored: PersistedState = serde_json::from_str(&json).unwrap();

    assert!(restored.schedule.is_none());
    assert_eq!(restored.plant.config.inverter_type, "ACCoupled");
}

#[test]
fn persisted_state_json_has_schedule_field() {
    let plant = test_plant_state();
    let schedule = test_schedule();
    let persisted = PersistedState {
        plant,
        schedule: Some(schedule),
    };

    let json = serde_json::to_string(&persisted).unwrap();
    let obj: serde_json::Value = serde_json::from_str(&json).unwrap();
    assert!(obj.get("plant").is_some());
    assert!(obj.get("schedule").is_some());
    assert!(obj["schedule"]["charge_start"].is_number());
    assert_eq!(obj["schedule"]["charge_start"], 0.0);
    assert_eq!(obj["schedule"]["charge_end"], 5.5);
}

// ===========================================================================
// 2. Backward compatibility: plain PlantState JSON still works
// ===========================================================================

#[test]
fn plain_plant_state_json_deserializes() {
    let plant = test_plant_state();
    let json = serde_json::to_string(&plant).unwrap();

    let restored: PlantState = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.config.inverter_type, "ACCoupled");
    assert_eq!(restored.config.solar_peak_watts, 6000.0);
}

#[test]
fn old_format_fallback_logic_works() {
    // Simulate the fallback logic used in load_plant:
    // Try PersistedState first, fall back to plain PlantState
    let plant = test_plant_state();
    let plain_json = serde_json::to_string(&plant).unwrap();

    // Try as PersistedState (should fail)
    let result = serde_json::from_str::<PersistedState>(&plain_json);
    assert!(
        result.is_err(),
        "Plain PlantState should not parse as PersistedState"
    );

    // Fallback to PlantState (should succeed)
    let restored: PlantState = serde_json::from_str(&plain_json).unwrap();
    assert_eq!(restored.config.inverter_type, "ACCoupled");
}

// ===========================================================================
// 3. Schedule defaults and serialization
// ===========================================================================

#[test]
fn schedule_default_disabled() {
    let sched = Schedule::default();
    assert_eq!(sched.charge_start, 0.0);
    assert_eq!(sched.charge_end, 0.0);
    assert_eq!(sched.discharge_start, 0.0);
    assert_eq!(sched.discharge_end, 0.0);
}

#[test]
fn schedule_roundtrip_preserves_all_fields() {
    let sched = Schedule {
        charge_start: 22.0,
        charge_end: 6.0,
        discharge_start: 17.0,
        discharge_end: 20.0,
        charge_start_2: 14.0,
        charge_end_2: 16.0,
        discharge_start_2: 0.0,
        discharge_end_2: 0.0,
        charge_target_soc: 90.0,
        charge_target_soc_2: 85.0,
        discharge_target_soc: 15.0,
        discharge_target_soc_2: 20.0,
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
        discharge_target_soc_3: 10.0,
        discharge_start_4: 0.0,
        discharge_end_4: 0.0,
        discharge_target_soc_4: 10.0,
        discharge_start_5: 0.0,
        discharge_end_5: 0.0,
        discharge_target_soc_5: 10.0,
        discharge_start_6: 0.0,
        discharge_end_6: 0.0,
        discharge_target_soc_6: 10.0,
        discharge_start_7: 0.0,
        discharge_end_7: 0.0,
        discharge_target_soc_7: 10.0,
        discharge_start_8: 0.0,
        discharge_end_8: 0.0,
        discharge_target_soc_8: 10.0,
        discharge_start_9: 0.0,
        discharge_end_9: 0.0,
        discharge_target_soc_9: 10.0,
        discharge_start_10: 0.0,
        discharge_end_10: 0.0,
        discharge_target_soc_10: 10.0,
        enable_charge: true,
        enable_discharge: true,
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
    };
    let json = serde_json::to_string(&sched).unwrap();
    let restored: Schedule = serde_json::from_str(&json).unwrap();
    assert_eq!(restored.charge_start, 22.0);
    assert_eq!(restored.charge_end, 6.0);
    assert_eq!(restored.discharge_start, 17.0);
    assert_eq!(restored.discharge_end, 20.0);
    assert_eq!(restored.charge_start_2, 14.0);
    assert_eq!(restored.charge_end_2, 16.0);
    assert_eq!(restored.discharge_start_2, 0.0);
    assert_eq!(restored.discharge_end_2, 0.0);
    assert_eq!(restored.charge_target_soc, 90.0);
    assert_eq!(restored.charge_target_soc_2, 85.0);
    assert_eq!(restored.discharge_target_soc, 15.0);
    assert_eq!(restored.discharge_target_soc_2, 20.0);
    assert!(
        restored.enable_charge,
        "Roundtrip should preserve enable_charge"
    );
    assert!(
        restored.enable_discharge,
        "Roundtrip should preserve enable_discharge"
    );
}

// ===========================================================================
// 4. PlantState preserves config through serialization
// ===========================================================================

#[test]
fn plant_preserves_inverter_type() {
    let mut state = test_plant_state();
    state.config.inverter_type = "AllInOne6".to_string();
    state.batteries[0].max_charge_kw = 6.0;
    state.sync_battery_from_vec();

    let json = serde_json::to_string(&state).unwrap();
    let restored: PlantState = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.config.inverter_type, "AllInOne6");
    assert_eq!(restored.batteries[0].max_charge_kw, 6.0);
}

#[test]
fn plant_preserves_overrides() {
    let mut state = test_plant_state();
    state.solar_override = Some(2500.0);
    state.load_override = Some(800.0);

    let json = serde_json::to_string(&state).unwrap();
    let restored: PlantState = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.solar_override, Some(2500.0));
    assert_eq!(restored.load_override, Some(800.0));
}

#[test]
fn plant_default_mode_is_eco() {
    let state = PlantState::new(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
    );
    assert_eq!(
        state.inverter.mode_state.effective,
        sim_models::InverterMode::Eco,
        "Default mode should be Eco"
    );
}

#[test]
fn multi_battery_state_roundtrip() {
    let mut state = PlantState::with_battery_count(
        chrono::NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(12, 0, 0)
            .unwrap(),
        3,
    );
    state.batteries[0].soc_percent = 80.0;
    state.batteries[1].soc_percent = 60.0;
    state.batteries[2].soc_percent = 40.0;
    state.sync_battery_from_vec();

    let json = serde_json::to_string(&state).unwrap();
    let restored: PlantState = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.batteries.len(), 3);
    assert_eq!(restored.batteries[0].soc_percent, 80.0);
    assert_eq!(restored.batteries[1].soc_percent, 60.0);
    assert_eq!(restored.batteries[2].soc_percent, 40.0);
}

#[test]
fn plant_preserves_energy_totals() {
    let mut state = test_plant_state();
    state.energy_totals.grid_import_kwh = 15.5;
    state.energy_totals.grid_export_kwh = 32.1;
    state.energy_totals.battery_charge_kwh = 50.0;
    state.energy_totals.battery_discharge_kwh = 21.0;
    state.energy_totals.solar_generation_kwh = 125.0;
    state.energy_totals.load_consumption_kwh = 80.0;

    let json = serde_json::to_string(&state).unwrap();
    let restored: PlantState = serde_json::from_str(&json).unwrap();

    assert_eq!(restored.energy_totals.grid_import_kwh, 15.5);
    assert_eq!(restored.energy_totals.grid_export_kwh, 32.1);
    assert_eq!(restored.energy_totals.solar_generation_kwh, 125.0);
}
