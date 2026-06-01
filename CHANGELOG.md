# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [0.3.0] - 2026-06-01

### Added

- **FaultEngine** device model: applies fault effects every tick
  - `grid_loss` disconnects grid, restored when fault clears
  - `inverter_trip` zeros inverter output and battery power
  - `battery_over_temp` blocks charging, allows discharging
- **Recording during simulation**: captures every tick to JSONL
- **CI output formats**: `--output <dir>` generates JSONL, CSV traces, JUnit XML, JSON report
- **Expanded scenario DSL**: `mode`, `export_limit`, `weather` event fields
- **Expanded assertions**: `solar_gt`, `solar_lt`, `grid_import_gt`, `grid_export_gt`, `battery_charging`, `no_faults`, `fault_active`
- **Named scenarios**: `name:` top-level key in YAML, parsed via `parse_named_scenario()`
- **ScenarioResult / AssertionResult** types for machine-readable test reports
- Example scenarios: `grid_outage.yaml`, `force_charge.yaml`
- Device update order: Solar → Load → Inverter → **Faults** → Battery

### Changed

- CLI exits with code 1 on assertion failures (CI-friendly)
- `FaultEngine` tracks previously-active faults to restore state on clear
- `sim-recording` now depends on `sim-scenarios` for report types

## [0.2.0] - 2026-06-01

### Added

- **SolarEngine**: Real PV generation model using sinusoidal irradiance curve with sunrise/sunset estimation from latitude and day-of-year. Solar elevation angle factor for seasonal variation. Weather modifiers (Clear, PartlyCloudy, Overcast, Storm).
- **BatteryEngine**: SOC tracking with formula `soc += (power_kw × dt_hours) / capacity_kwh × 100`. Min/max SOC clamping. Charge/discharge rate limits.
- **InverterEngine**: Full priority logic (Solar → Load → Battery → Grid) for all 5 modes: Normal, Eco, ForceCharge, ForceDischarge, ExportLimit. Island mode for grid-loss faults.
- **LoadEngine**: Time-of-day load profiles (Minimal, Family, EV, HeatPump) with hourly interpolation.
- `WeatherCondition` enum with irradiance factors.
- `LoadProfile` enum with realistic hourly demand patterns.
- CLI flags: `--peak-watts`, `--latitude`, `--profile`, `--weather`.
- 14 new unit tests for all device models and integration.
- Restructured: state types moved to `sim-models`, engines live in `sim-core`.

### Changed

- `DeviceModel::update` now takes `&mut PlantState` in addition to `&TickContext`.
- `sim-api` uses real device models instead of stubs.
- Crate dependency graph simplified: most crates depend on `sim-models` instead of `sim-core`.

## [0.1.0] - 2026-06-01

### Added

- Workspace with 9 Rust crates (`sim-models`, `sim-core`, `sim-registers`, `sim-faults`, `sim-scenarios`, `sim-recording`, `sim-modbus`, `sim-storage`, `sim-api`)
- `DeviceModel` trait and `TickContext` in `sim-models`
- `PlantState` with sub-system state snapshots (inverter, battery, solar, load, grid)
- `Command` enum for external writes, applied between ticks
- `SimulationEngine` tick scheduler with deterministic execution
- `RegisterDef` catalogue and `RegisterStore` with projection from `PlantState`
- Access-controlled register writes (ReadOnly / ReadWrite)
- Fault framework with 5 categories and 5 well-known fault definitions
- YAML scenario DSL parser with assertions (`soc_gt`, `soc_lt`, `grid_connected`)
- JSON Lines recording format with read, write, and diff support
- Modbus TCP server skeleton (function code 0x03 — Read Holding Registers)
- File-based recording persistence
- Headless CLI: `giv-sim run scenario.yaml [--modbus addr]`
- Example scenario: `examples/basic_day.yaml`
