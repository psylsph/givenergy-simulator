//! Scenario DSL: parse YAML scenario files for CI regression tests.
//!
//! Example YAML:
//! ```yaml
//! 08:00:
//!   solar: 3500
//! 09:00:
//!   fault: grid_loss
//! 10:00:
//!   expect:
//!     soc_gt: 50
//! ```

use chrono::NaiveTime;
use serde::Deserialize;
use std::collections::HashMap;

/// A single time-stamped event in a scenario.
#[derive(Debug, Clone, Deserialize)]
pub struct ScenarioEvent {
    /// Override solar generation (watts).
    pub solar: Option<f64>,
    /// Override load demand (watts).
    pub load: Option<f64>,
    /// Inject a fault by ID.
    pub fault: Option<String>,
    /// Clear a fault by ID.
    pub clear_fault: Option<String>,
    /// Assertions to check at this time.
    pub expect: Option<HashMap<String, serde_yaml::Value>>,
}

/// A parsed scenario.
#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: String,
    /// Sorted by time.
    pub events: Vec<(NaiveTime, ScenarioEvent)>,
}

/// Parse a scenario from YAML string.
pub fn parse_scenario(yaml: &str) -> Result<Scenario, Box<dyn std::error::Error>> {
    let raw: HashMap<String, ScenarioEvent> = serde_yaml::from_str(yaml)?;

    let mut events: Vec<(NaiveTime, ScenarioEvent)> = raw
        .into_iter()
        .filter_map(|(time_str, evt)| {
            let t = NaiveTime::parse_from_str(&time_str, "%H:%M").ok()?;
            Some((t, evt))
        })
        .collect();

    events.sort_by_key(|(t, _)| *t);

    Ok(Scenario {
        name: "unnamed".into(),
        events,
    })
}

/// Evaluate assertions from the `expect` map against plant state.
pub fn check_assertions(
    expect: &HashMap<String, serde_yaml::Value>,
    state: &sim_core::PlantState,
) -> Result<(), Vec<String>> {
    let mut failures = Vec::new();

    if let Some(serde_yaml::Value::Number(n)) = expect.get("soc_gt") {
        if let Some(threshold) = n.as_f64() {
            if state.battery.soc_percent <= threshold {
                failures.push(format!(
                    "soc_gt: expected > {}, got {}",
                    threshold, state.battery.soc_percent
                ));
            }
        }
    }

    if let Some(serde_yaml::Value::Number(n)) = expect.get("soc_lt") {
        if let Some(threshold) = n.as_f64() {
            if state.battery.soc_percent >= threshold {
                failures.push(format!(
                    "soc_lt: expected < {}, got {}",
                    threshold, state.battery.soc_percent
                ));
            }
        }
    }

    if let Some(serde_yaml::Value::Number(n)) = expect.get("grid_connected") {
        if let Some(expected) = n.as_f64() {
            let actual = if state.grid.connected { 1.0 } else { 0.0 };
            if actual != expected {
                failures.push(format!(
                    "grid_connected: expected {}, got {}",
                    expected, actual
                ));
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_core::PlantState;
    use chrono::NaiveDate;

    fn test_ts() -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 1).unwrap().and_hms_opt(8, 0, 0).unwrap()
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
    fn assertion_soc_gt_passes() {
        let mut state = PlantState::new(test_ts());
        state.battery.soc_percent = 75.0;
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
}
