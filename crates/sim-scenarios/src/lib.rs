//! Scenario DSL: parse YAML scenario files for CI regression tests.
//!
//! Supported event fields:
//! - `solar: <watts>` — override PV generation
//! - `load: <watts>` — override household demand
//! - `fault: <id>` — inject a fault
//! - `clear_fault: <id>` — clear a fault
//! - `mode: <Normal|Eco|ForceCharge|ForceDischarge|ExportLimit>` — set inverter mode
//! - `export_limit: <watts>` — set export limit
//! - `weather: <Clear|PartlyCloudy|Overcast|Storm>` — change weather
//! - `expect:` — assertion block
//!
//! Assertions:
//! - `soc_gt`, `soc_lt` — battery SOC bounds
//! - `grid_connected: 0|1` — grid connection state
//! - `solar_gt`, `solar_lt` — PV generation bounds
//! - `grid_import_gt`, `grid_export_gt` — grid power flow bounds
//! - `battery_charging: true|false` — battery charging check
//! - `no_faults: true` — assert no active faults
//! - `fault_active: <id>` — assert a specific fault is active

use chrono::NaiveTime;
use serde::Deserialize;
use std::collections::HashMap;

/// A single time-stamped event in a scenario.
#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioEvent {
    pub solar: Option<f64>,
    pub load: Option<f64>,
    pub fault: Option<String>,
    pub clear_fault: Option<String>,
    pub mode: Option<String>,
    pub export_limit: Option<f64>,
    pub weather: Option<String>,
    pub expect: Option<HashMap<String, serde_yaml::Value>>,
    /// Number of days to run (for multi-day scenarios). Events repeat daily.
    /// If not set, scenario runs for 1 day.
    #[serde(default)]
    pub days: Option<u32>,
}

/// A parsed scenario.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    /// Events sorted by time.
    pub events: Vec<(NaiveTime, ScenarioEvent)>,
    /// Number of days to simulate. Defaults to 1.
    pub days: u32,
}

/// Parse a scenario from YAML string (no name support).
pub fn parse_scenario(yaml: &str) -> Result<Scenario, Box<dyn std::error::Error>> {
    let raw: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(yaml)?;

    let days = raw.get("days").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

    let mut events: Vec<(NaiveTime, ScenarioEvent)> = raw
        .into_iter()
        .filter_map(|(time_str, value)| {
            let t = NaiveTime::parse_from_str(&time_str, "%H:%M").ok()?;
            let evt: ScenarioEvent = serde_yaml::from_value(value.clone()).ok()?;
            Some((t, evt))
        })
        .collect();

    events.sort_by_key(|(t, _)| *t);

    Ok(Scenario {
        name: "unnamed".into(),
        events,
        days,
    })
}

/// Parse a scenario with an optional `name` top-level key.
pub fn parse_named_scenario(yaml: &str) -> Result<Scenario, Box<dyn std::error::Error>> {
    let raw: HashMap<String, serde_yaml::Value> = serde_yaml::from_str(yaml)?;

    let name = raw
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("unnamed")
        .to_string();

    let days = raw.get("days").and_then(|v| v.as_u64()).unwrap_or(1) as u32;

    let mut events: Vec<(NaiveTime, ScenarioEvent)> = raw
        .iter()
        .filter_map(|(key, value)| {
            let t = NaiveTime::parse_from_str(key, "%H:%M").ok()?;
            let evt: ScenarioEvent = serde_yaml::from_value(value.clone()).ok()?;
            Some((t, evt))
        })
        .collect();

    events.sort_by_key(|(t, _)| *t);

    Ok(Scenario { name, events, days })
}

/// Result of running a full scenario.
#[derive(Debug, Clone, serde::Serialize)]
pub struct ScenarioResult {
    pub name: String,
    pub passed: usize,
    pub failed: usize,
    pub assertions: Vec<AssertionResult>,
}

/// A single assertion outcome.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AssertionResult {
    pub time: String,
    pub passed: bool,
    pub messages: Vec<String>,
}

impl ScenarioResult {
    pub fn total(&self) -> usize {
        self.passed + self.failed
    }

    pub fn is_success(&self) -> bool {
        self.failed == 0
    }
}

/// Evaluate assertions from the `expect` map against plant state.
pub fn check_assertions(
    expect: &HashMap<String, serde_yaml::Value>,
    state: &sim_models::PlantState,
) -> Result<(), Vec<String>> {
    let mut failures = Vec::new();

    macro_rules! check_gt {
        ($key:expr, $actual:expr) => {
            if let Some(serde_yaml::Value::Number(n)) = expect.get($key) {
                if let Some(threshold) = n.as_f64() {
                    if $actual <= threshold {
                        failures.push(format!(
                            "{}: expected > {}, got {:.1}",
                            $key, threshold, $actual
                        ));
                    }
                }
            }
        };
    }

    macro_rules! check_lt {
        ($key:expr, $actual:expr) => {
            if let Some(serde_yaml::Value::Number(n)) = expect.get($key) {
                if let Some(threshold) = n.as_f64() {
                    if $actual >= threshold {
                        failures.push(format!(
                            "{}: expected < {}, got {:.1}",
                            $key, threshold, $actual
                        ));
                    }
                }
            }
        };
    }

    check_gt!("soc_gt", state.aggregate_soc());
    check_lt!("soc_lt", state.aggregate_soc());
    check_gt!("solar_gt", state.solar.generation_w);
    check_lt!("solar_lt", state.solar.generation_w);

    // grid_connected
    if let Some(expected) = expect.get("grid_connected").and_then(|v| v.as_f64()) {
        let actual = if state.grid.connected { 1.0 } else { 0.0 };
        if actual != expected {
            failures.push(format!(
                "grid_connected: expected {}, got {}",
                expected, actual
            ));
        }
    }

    // grid_import_gt (positive power = importing)
    let grid_import = state.grid.power_w.max(0.0);
    check_gt!("grid_import_gt", grid_import);

    // grid_export_gt (negative power = exporting)
    let grid_export = (-state.grid.power_w).max(0.0);
    check_gt!("grid_export_gt", grid_export);

    // battery_charging
    if let Some(serde_yaml::Value::Bool(expected)) = expect.get("battery_charging") {
        let is_charging = state.total_battery_power_kw() > 0.0;
        if is_charging != *expected {
            failures.push(format!(
                "battery_charging: expected {}, got {} (total_power_kw={:.2})",
                expected,
                is_charging,
                state.total_battery_power_kw()
            ));
        }
    }

    // no_faults
    if let Some(serde_yaml::Value::Bool(expected)) = expect.get("no_faults") {
        let has_faults = !state.active_faults.is_empty();
        if *expected && has_faults {
            failures.push(format!(
                "no_faults: expected none, got {:?}",
                state.active_faults
            ));
        }
    }

    // fault_active
    #[allow(clippy::collapsible_if)]
    if let Some(serde_yaml::Value::String(id)) = expect.get("fault_active") {
        if !state.active_faults.contains(id) {
            failures.push(format!(
                "fault_active: expected '{}' to be active, got {:?}",
                id, state.active_faults
            ));
        }
    }

    // ---- Energy totals assertions ----
    let grid_import_kwh = state.energy_totals.grid_import_kwh;
    check_gt!("grid_import_kwh_gt", grid_import_kwh);

    let grid_export_kwh = state.energy_totals.grid_export_kwh;
    check_gt!("grid_export_kwh_gt", grid_export_kwh);

    let solar_kwh = state.energy_totals.solar_generation_kwh;
    check_gt!("solar_kwh_gt", solar_kwh);

    let load_kwh = state.energy_totals.load_consumption_kwh;
    check_gt!("load_kwh_gt", load_kwh);

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sim_models::PlantState;

    fn test_ts() -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(8, 0, 0)
            .unwrap()
    }

    #[test]
    fn parse_basic_scenario() {
        let yaml = r#"
08:00:
  solar: 3500
09:00:
  fault: grid_loss
10:00:
  expect:
    soc_gt: 50
"#;
        let scenario = parse_scenario(yaml).unwrap();
        assert_eq!(scenario.events.len(), 3);
        assert_eq!(scenario.events[0].1.solar, Some(3500.0));
        assert_eq!(scenario.events[1].1.fault.as_deref(), Some("grid_loss"));
    }

    #[test]
    fn parse_named_scenario_works() {
        let yaml = r#"
name: grid outage test
08:00:
  solar: 3500
09:00:
  fault: grid_loss
10:00:
  clear_fault: grid_loss
  expect:
    grid_connected: 1
"#;
        let scenario = parse_named_scenario(yaml).unwrap();
        assert_eq!(scenario.name, "grid outage test");
        assert_eq!(scenario.events.len(), 3);
    }

    #[test]
    fn parse_mode_command() {
        let yaml = r#"
08:00:
  mode: ForceCharge
"#;
        let scenario = parse_scenario(yaml).unwrap();
        assert_eq!(scenario.events[0].1.mode.as_deref(), Some("ForceCharge"));
    }

    #[test]
    fn assertion_soc_gt_passes() {
        let mut state = PlantState::new(test_ts());
        state.battery.soc_percent = 75.0;
        state.sync_vec_from_battery();
        let mut expect = HashMap::new();
        expect.insert("soc_gt".into(), serde_yaml::Value::Number(50.into()));
        assert!(check_assertions(&expect, &state).is_ok());
    }

    #[test]
    fn assertion_soc_gt_fails() {
        let state = PlantState::new(test_ts());
        let mut expect = HashMap::new();
        expect.insert("soc_gt".into(), serde_yaml::Value::Number(80.into()));
        assert!(check_assertions(&expect, &state).is_err());
    }

    #[test]
    fn assertion_battery_charging() {
        let mut state = PlantState::new(test_ts());
        state.battery.power_kw = 2.5;
        state.sync_vec_from_battery();
        let mut expect = HashMap::new();
        expect.insert("battery_charging".into(), serde_yaml::Value::Bool(true));
        assert!(check_assertions(&expect, &state).is_ok());
    }

    #[test]
    fn assertion_no_faults() {
        let state = PlantState::new(test_ts());
        let mut expect = HashMap::new();
        expect.insert("no_faults".into(), serde_yaml::Value::Bool(true));
        assert!(check_assertions(&expect, &state).is_ok());
    }

    #[test]
    fn assertion_fault_active() {
        let mut state = PlantState::new(test_ts());
        state.active_faults.push("grid_loss".into());
        let mut expect = HashMap::new();
        expect.insert(
            "fault_active".into(),
            serde_yaml::Value::String("grid_loss".into()),
        );
        assert!(check_assertions(&expect, &state).is_ok());
    }

    #[test]
    fn assertion_grid_export() {
        let mut state = PlantState::new(test_ts());
        state.grid.power_w = -2000.0;
        let mut expect = HashMap::new();
        expect.insert(
            "grid_export_gt".into(),
            serde_yaml::Value::Number(1000.into()),
        );
        assert!(check_assertions(&expect, &state).is_ok());
    }

    #[test]
    fn parse_multi_day_scenario() {
        let yaml = r#"
name: two day test
days: 2
08:00:
  solar: 3000
20:00:
  expect:
    soc_lt: 50
"#;
        let scenario = parse_named_scenario(yaml).unwrap();
        assert_eq!(scenario.name, "two day test");
        assert_eq!(scenario.days, 2);
        assert_eq!(scenario.events.len(), 2);
    }

    #[test]
    fn energy_totals_assertions() {
        let mut state = PlantState::new(test_ts());
        state.energy_totals.solar_generation_kwh = 30.0;
        state.energy_totals.grid_export_kwh = 15.0;
        let mut expect = HashMap::new();
        expect.insert("solar_kwh_gt".into(), serde_yaml::Value::Number(25.into()));
        expect.insert(
            "grid_export_kwh_gt".into(),
            serde_yaml::Value::Number(10.into()),
        );
        assert!(check_assertions(&expect, &state).is_ok());
    }
}
