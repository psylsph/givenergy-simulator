# Rust Crate Design

## Workspace (10 crates)

```
crates/
  sim-models/    — DeviceModel trait, TickContext, PlantState, all sub-state types
  sim-core/      — SimulationEngine, Command enum, all device model implementations
  sim-registers/ — RegisterDef, RegisterStore, 45-register catalogue, state→register projection
  sim-modbus/    — Modbus TCP server (fn 0x03 read, fn 0x06 write), command dispatch channel
  sim-scenarios/ — YAML DSL parser, assertion checking engine
  sim-faults/    — Fault definitions, FaultEngine device model
  sim-recording/ — JSON Lines recording, CSV export, JUnit XML, JSON report
  sim-storage/   — File I/O for recording load/save
  sim-api/       — Headless CLI binary (`giv-sim run`, `giv-sim replay`)
  sim-tauri/     — Tauri v2 desktop GUI (10 IPC commands, 4 events)
```

## sim-models
```
pub trait DeviceModel: Send {
    fn update(&mut self, ctx: &TickContext, state: &mut PlantState);
}
```
- `PlantState` — timestamp, inverter, batteries (Vec<>, 1–3 modules), solar, load, grid, faults, weather, energy_totals, config
- `PlantConfig` — solar_peak_watts, latitude, tick_interval_secs
- `EnergyTotals` — grid_import/export_kwh, battery_charge/discharge_kwh, solar_generation_kwh, load_consumption_kwh
- `BatteryState` — soc_percent, capacity_kwh, nominal_capacity_kwh, throughput_kwh, soh, cycle_count, efficiency, temperature, power_kw

## sim-core
- `SimulationEngine` — tick scheduler, command queue, device model registry
- `Command` — SetInverterMode, SetExportLimit, SetMinSoc, SetMaxSoc, InjectFault, ClearFault, SetWeather
- `SolarEngine` — sinusoidal irradiance, latitude/day-of-year, weather factor
- `LoadEngine` — 4 profiles + custom time-series (Vec<(hour, watts)>)
- `InverterEngine` — Normal / Eco / ForceCharge / ForceDischarge / ExportLimit / Island modes
- `BatteryEngine` — SOC tracking, charge/discharge efficiency, thermal model, aging/degradation
- `FaultEngine` — grid_loss, inverter_trip, battery_over_temp
- `EnergyTracker` — cumulative energy totals
- `ScheduleEngine` — timed charge/discharge windows with SOC targets

## sim-registers
- 45 registers across 7 categories: Inverter, Battery, PV, Grid, Energy Totals, Configuration, Schedules
- Each `RegisterDef` has address, name, category, type, scaling_factor, access (ReadOnly/ReadWrite)
- `project_from_state()` returns engineering values, applies scaling_factor uniformly
- Addr 100: inverter_mode (ReadWrite), 200: battery_soc, 400: grid_power, 500+: energy totals

## sim-modbus
- TCP server accepting multiple concurrent connections
- Function code 0x03: Read Holding Registers
- Function code 0x06: Write Single Register (validates access, dispatches Command)
- Writes forwarded via `tokio::sync::mpsc` channel to simulation engine

## sim-scenarios
- YAML DSL with `name`, `days`, and time-stamped events
- Events: solar, load, fault, clear_fault, mode, export_limit, weather, expect
- Assertions: soc_gt/lt, solar_gt/lt, grid_import/export_gt, battery_charging, no_faults, fault_active, energy_kwh assertions

## sim-api (CLI)
- `giv-sim run <scenario.yaml>` — with --peak-watts, --latitude, --profile, --weather, --battery-count, --modbus, --output
- `giv-sim replay <recording.jsonl>` — with --diff for comparison, --format (summary/csv/json)

## sim-tauri (GUI)
- Tauri v2 desktop app with web frontend
- 10 IPC commands: create_plant, load_scenario, start_simulation, pause_simulation, inject_fault, clear_fault, set_mode, set_weather, export_recording, get_current_state
- 4 events: state_changed, fault_triggered, scenario_completed, recording_saved
- Real-time canvas-based power timeline, energy flow diagram, SOC gauge
