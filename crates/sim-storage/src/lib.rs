//! Storage: load/save recordings to disk.
//!
//! Wraps [`sim_recording`] with file I/O.

use std::path::Path;

/// Save recording frames to a JSON Lines file.
pub fn save_recording(
    path: &Path,
    frames: &[sim_recording::RecordingFrame],
) -> std::io::Result<()> {
    let mut file = std::fs::File::create(path)?;
    for frame in frames {
        sim_recording::write_frame(&mut file, frame)?;
    }
    Ok(())
}

/// Load recording frames from a JSON Lines file.
pub fn load_recording(
    path: &Path,
) -> Result<Vec<sim_recording::RecordingFrame>, Box<dyn std::error::Error>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    Ok(sim_recording::read_frames(reader)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;
    use sim_models::PlantState;
    use sim_recording::RecordingFrame;
    use std::collections::HashMap;

    fn test_ts(hour: u32) -> chrono::NaiveDateTime {
        NaiveDate::from_ymd_opt(2025, 6, 1)
            .unwrap()
            .and_hms_opt(hour, 0, 0)
            .unwrap()
    }

    #[test]
    fn roundtrip_file() {
        let dir = std::env::temp_dir().join("givenergy-test-storage");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("test.jsonl");

        let frames = vec![RecordingFrame {
            timestamp: test_ts(12),
            plant_state: PlantState::new(test_ts(12)),
            register_snapshot: HashMap::new(),
        }];

        save_recording(&path, &frames).unwrap();
        let loaded = load_recording(&path).unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].timestamp, test_ts(12));

        std::fs::remove_dir_all(&dir).ok();
    }
}
