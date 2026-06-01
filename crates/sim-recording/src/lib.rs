//! Recording engine: JSON Lines, CSV, JUnit XML, JSON report.
//!
//! JSON Lines: `{ "timestamp": ..., "plant_state": ..., "register_snapshot": ... }`
//!
//! CSV: one row per tick with key columns.
//!
//! JUnit XML: test-suite summary of scenario assertions.
//!
//! JSON report: machine-readable scenario result.

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

// ---------------------------------------------------------------------------
// Recording frame
// ---------------------------------------------------------------------------

/// A single recorded frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingFrame {
    pub timestamp: NaiveDateTime,
    pub plant_state: sim_models::PlantState,
    /// Register address → raw value at this instant.
    pub register_snapshot: std::collections::HashMap<u16, u16>,
}

// ---------------------------------------------------------------------------
// JSON Lines
// ---------------------------------------------------------------------------

/// Append a frame to a writer as a JSON line.
pub fn write_frame<W: Write>(writer: &mut W, frame: &RecordingFrame) -> std::io::Result<()> {
    let json = serde_json::to_string(frame)?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n")
}

/// Read all frames from a JSON Lines reader.
pub fn read_frames<R: BufRead>(reader: R) -> Result<Vec<RecordingFrame>, serde_json::Error> {
    let mut frames = Vec::new();
    for line in reader.lines() {
        let line = line.map_err(serde_json::Error::io)?;
        if line.trim().is_empty() {
            continue;
        }
        let frame: RecordingFrame = serde_json::from_str(&line)?;
        frames.push(frame);
    }
    Ok(frames)
}

/// Diff two recordings, returning indices where state diverges.
pub fn diff_recordings(a: &[RecordingFrame], b: &[RecordingFrame]) -> Vec<usize> {
    let min_len = a.len().min(b.len());
    let mut diffs = Vec::new();

    for i in 0..min_len {
        if serde_json::to_string(&a[i].plant_state).unwrap_or_default()
            != serde_json::to_string(&b[i].plant_state).unwrap_or_default()
        {
            diffs.push(i);
        }
    }

    if a.len() != b.len() {
        for i in min_len..a.len().max(b.len()) {
            diffs.push(i);
        }
    }

    diffs
}

// ---------------------------------------------------------------------------
// CSV trace export
// ---------------------------------------------------------------------------

/// Write recorded frames as CSV rows.
pub fn write_csv<W: Write>(writer: &mut W, frames: &[RecordingFrame]) -> std::io::Result<()> {
    writeln!(
        writer,
        "timestamp,soc_percent,battery_power_kw,solar_w,load_w,grid_w,grid_connected,inverter_mode,active_faults"
    )?;
    for frame in frames {
        let faults = frame.plant_state.active_faults.join(";");
        writeln!(
            writer,
            "{},{:.2},{:.2},{:.0},{:.0},{:.0},{},{:?},\"{}\"",
            frame.timestamp,
            frame.plant_state.battery.soc_percent,
            frame.plant_state.battery.power_kw,
            frame.plant_state.solar.generation_w,
            frame.plant_state.load.demand_w,
            frame.plant_state.grid.power_w,
            frame.plant_state.grid.connected as u8,
            frame.plant_state.inverter.mode,
            faults,
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// JUnit XML
// ---------------------------------------------------------------------------

/// Write a JUnit XML test report from a scenario result.
pub fn write_junit_xml<W: Write>(
    writer: &mut W,
    result: &sim_scenarios::ScenarioResult,
) -> std::io::Result<()> {
    writeln!(writer, r#"<?xml version="1.0" encoding="UTF-8"?>"#)?;
    writeln!(
        writer,
        r#"<testsuites><testsuite name="{}" tests="{}" failures="{}">"#,
        xml_escape(&result.name),
        result.total(),
        result.failed,
    )?;

    for assertion in &result.assertions {
        if assertion.passed {
            writeln!(
                writer,
                r#"  <testcase name="assertion @ {}" classname="{}"/>"#,
                xml_escape(&assertion.time),
                xml_escape(&result.name),
            )?;
        } else {
            writeln!(
                writer,
                r#"  <testcase name="assertion @ {}" classname="{}">"#,
                xml_escape(&assertion.time),
                xml_escape(&result.name),
            )?;
            writeln!(
                writer,
                r#"    <failure message="{}">{}</failure>"#,
                xml_escape(&assertion.messages.join("; ")),
                xml_escape(&assertion.messages.join("\n")),
            )?;
            writeln!(writer, "  </testcase>")?;
        }
    }

    writeln!(writer, "</testsuite></testsuites>")?;
    Ok(())
}

/// Write a JSON report.
pub fn write_json_report<W: Write>(
    writer: &mut W,
    result: &sim_scenarios::ScenarioResult,
) -> std::io::Result<()> {
    let json = serde_json::to_string_pretty(result)?;
    writer.write_all(json.as_bytes())?;
    writer.write_all(b"\n")
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sim_models::PlantState;

    fn test_ts(hour: u32) -> NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    #[test]
    fn roundtrip_frames() {
        let mut buf = Vec::new();
        let frame = RecordingFrame {
            timestamp: test_ts(12),
            plant_state: PlantState::new(test_ts(12)),
            register_snapshot: std::collections::HashMap::new(),
        };
        write_frame(&mut buf, &frame).unwrap();

        let frames = read_frames(buf.as_slice()).unwrap();
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].timestamp, test_ts(12));
    }

    #[test]
    fn csv_output_has_header() {
        let frames = vec![RecordingFrame {
            timestamp: test_ts(12),
            plant_state: PlantState::new(test_ts(12)),
            register_snapshot: std::collections::HashMap::new(),
        }];
        let mut buf = Vec::new();
        write_csv(&mut buf, &frames).unwrap();
        let output = String::from_utf8(buf).unwrap();
        let first_line = output.lines().next().unwrap();
        assert!(first_line.starts_with("timestamp"));
        assert!(first_line.contains("soc_percent"));
    }

    #[test]
    fn junit_xml_valid_structure() {
        let result = sim_scenarios::ScenarioResult {
            name: "test_scenario".into(),
            passed: 1,
            failed: 1,
            assertions: vec![
                sim_scenarios::AssertionResult {
                    time: "10:00:00".into(),
                    passed: true,
                    messages: vec![],
                },
                sim_scenarios::AssertionResult {
                    time: "12:00:00".into(),
                    passed: false,
                    messages: vec!["soc_gt: expected > 80, got 50".into()],
                },
            ],
        };
        let mut buf = Vec::new();
        write_junit_xml(&mut buf, &result).unwrap();
        let xml = String::from_utf8(buf).unwrap();
        assert!(xml.contains("<?xml"));
        assert!(xml.contains("<testsuite"));
        assert!(xml.contains("<failure"));
    }
}
