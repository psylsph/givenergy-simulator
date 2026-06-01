# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

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
- 14 unit tests, all passing
