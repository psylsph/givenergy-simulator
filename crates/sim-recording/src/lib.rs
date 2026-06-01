//! Recording engine: JSON Lines format for replay, diffing, regression.
//!
//! Each line: `{ "timestamp": ..., "plant_state": ..., "register_snapshot": ... }`

use chrono::NaiveDateTime;
use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};

/// A single recorded frame.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RecordingFrame {
    pub timestamp: NaiveDateTime,
    pub plant_state: sim_core::PlantState,
    /// Register address → raw value at this instant.
    pub register_snapshot: std::collections::HashMap<u16, u16>,
}

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
        let line = line.map_err(|e| serde_json::Error::io(e))?;
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
        // Compare serialised form for simplicity
        if serde_json::to_string(&a[i].plant_state).unwrap_or_default()
            != serde_json::to_string(&b[i].plant_state).unwrap_or_default()
        {
            diffs.push(i);
        }
    }

    // Extra frames count as diffs
    if a.len() != b.len() {
        for i in min_len..a.len().max(b.len()) {
            diffs.push(i);
        }
    }

    diffs
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sim_core::PlantState;

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
}
