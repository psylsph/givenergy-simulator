# Recording Format

Frames are recorded every tick during simulation and exported in multiple formats.

## JSON Lines (`.jsonl`)
One JSON object per line. Preferred for machine consumption (replay, diff).

```json
{
  "timestamp": "2025-06-01T12:00:30",
  "plant_state": { ... full PlantState ... },
  "register_snapshot": { "100": 0, "200": 75, ... }
}
```

## CSV (`.csv`)
One header row + one data row per tick. Columns:

```
timestamp,aggregate_soc,module1_soc,module2_soc,module3_soc,
total_capacity,total_battery_power_kw,solar_w,load_w,grid_w,
grid_connected,inverter_mode,active_faults,
grid_import_kwh,grid_export_kwh,battery_charge_kwh,
battery_discharge_kwh,solar_kwh,load_kwh
```

Energy totals columns: grid_import_kwh, grid_export_kwh, battery_charge_kwh, battery_discharge_kwh, solar_kwh, load_kwh.

## JUnit XML (`.xml`)
Test-suite format for CI integration. One `<testcase>` per assertion.

```xml
<testsuite name="scenario_name" tests="5" failures="1">
  <testcase name="assertion @ 10:00" classname="scenario_name"/>
  <testcase name="assertion @ 12:00" classname="scenario_name">
    <failure message="soc_gt: expected > 80, got 50"/>
  </testcase>
</testsuite>
```

## JSON Report (`.json`)
Machine-readable scenario result with assertion outcomes.

## Recording Commands

### CLI
```bash
# Run with all outputs
giv-sim run scenario.yaml --output /path/to/dir
# → /path/to/dir/scenario.jsonl, .csv, .xml, _report.json

# Replay a recording
giv-sim replay recording.jsonl

# Diff two recordings
giv-sim replay recording_a.jsonl --diff recording_b.jsonl

# Export as CSV
giv-sim replay recording.jsonl --format csv
```

### Tauri GUI
```
export_recording({ path: "/path/to/file", format: "csv" | "jsonl" | "json" })
```
