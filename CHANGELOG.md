# Changelog

All notable changes to the GivEnergy Plant Simulator are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.10.0] - 2026-06-04

### Added
- CT clamp meter simulation (IR 60-89) with per-phase V/I/P, totals, PF, frequency, energy
- Meter registers served on Modbus slave addresses 0x01-0x08
- MeterState struct derived from PlantState grid data
- 30 meter input register definitions in catalogue
- Export limit scheduling with 3 time windows (HR 2062-2071)
- Scenario fuzzer (proptest-based property testing)

### Fixed
- App renamed consistently to "GivEnergy Plant Simulator" across UI, CLI, and config
- Tauri config version synced (was stale at 0.6.0)
- Footer no longer shows stale hardcoded version
- cargo fmt + clippy clean

## [0.9.0] - 2026-06-03

### Added
- Extended charge/discharge slots 3-10 (HR 246-299) with per-slot target SOCs
  — Schedule model expanded; ScheduleEngine uses macro-driven 10-slot check
- 9 new inverter DTC variants: Gen3Plus (0x2201-2204), ThreePhase8/10kW
  (0x4002-4003), AIOHybrid (0x8201-8203) — all with correct power limits
- HR 166 (enable_rtc), 311 (export_priority), 317 (enable_eps) command routing
- Smart Load slots (HR 554-573) in register catalogue
- High registers (HR 4107-4114) in catalogue with projection

### Changed
- Register catalogue expanded from 177 to 249 entries (+72)
- is_schedule_register updated to cover HR 246-269 and 276-299
- Test count: 216

## [0.8.0] - 2026-06-03

### Added
- Heartbeat (main function 0x01) support in Modbus server — keeps client connections alive
- FC 0x16 (Read Meter Product Registers) support in Modbus server
- Battery pause mode enforcement — battery power zeroed during pause window when mode=1
- Battery charge/discharge limit enforcement (HR 111/112) scaled from 0-50 range
- `enable_charge_target` flag gates global charge target in ScheduleEngine
- New PlantState fields: `battery_discharge_min_power_reserve`, `enable_rtc`, `export_priority`, `enable_eps`
- New commands: `SetEnableRtc`, `SetBatteryDischargeMinPowerReserve`, `SetExportPriority`, `SetEnableEps`
- `scheduled_charge` / `scheduled_discharge` now excluded from JSON persistence (`#[serde(skip)]`)
- AC-coupled inverter detection — discharge slots, pause slot, and charge slot 2 hidden in GUI and disabled in engine
- `project_schedule_for()` method accepting inverter type for AC-coupled-aware register projection
- `apply_schedule_updates()` helper shared between `run_scenario` and `serve_config`

### Fixed
- `create_plant` now resets stored schedule to default — old schedule no longer leaks from previous session
- `serve_config` now passes and updates shared battery state vector — BMS registers read real data
- Pause slot writes (HR 319-320) routed to `SetBatteryPause` in both Tauri drain loops
- BMS reads return battery state in `serve_config` mode (was always empty Vec)
- HR 114 (`battery_discharge_min_power_reserve`) projects from correct PlantState field
- HR 166, 311, 317 added to catalogue with projection
- Duplicate HR 166 catalogue entry removed
- `ScheduleDto::from_state` HHMM conversion now matches register projector — 0.0 → disabled (60)
- `enable_charge`/`enable_discharge` in DTO correctly computed: requires `start > 0.0` to be active
- `run_scenario` now runs a ScheduleEngine and accumulates Modbus schedule writes
- Test count: 216

## [0.7.1] - 2026-06-03

### Added
- Gen 1 Hybrid inverter type (0x1001, 2500W battery limit)
- Dual PV array support with 45/55 power split
- PV2 peak capacity configurable in plant creation
- 12 inverter types with correct datasheet battery limits
- SOH adjustment recalculates charge/discharge limits
- Time sync accumulator for HR 35-40 across drain cycles
- Solar override applies before night check

### Fixed
- Battery charge/discharge capped by inverter max AC power in all modes
- PV2 voltage register (IR 2) returns 350V when PV2 configured
- All clippy warnings resolved (zero warnings from `cargo clippy --all-targets`)
- All formatting issues resolved (`cargo fmt --check` clean)
- Inverter dropdown and presets ordered by DTC hex value

### Changed
- `SolarState` split into `pv1_w` / `pv2_w` (generation_w = total)
- CI pipeline clippy filter improved
- Test count: 211

## [0.7.0]
- Dual PV array support — PV1 and PV2 modelled as independent arrays with 45/55 power split
- PV2 peak capacity configurable in plant creation dialog (0 = disabled)
- Solar override now applies before night check, respecting array split
- Inverter throughput limits corrected for all 12 inverter types per official datasheets
- Gen 1 Hybrid added with 2500W battery charge/discharge limit
- Battery charge/discharge rates capped by inverter max AC power across all modes
- SOH adjustment recalculates battery charge/discharge limits
- Time sync from Modbus client now processed correctly across drain cycles
- Deterministic tick-loop simulation engine with pluggable device models
- Solar PV generation with sinusoidal irradiance, latitude/day-of-year, weather modifiers
- Battery SOC tracking, C-rate limits, charge/discharge efficiency (95%), thermal model with derating
- Inverter with 5 priority modes: Normal, Eco, Force Charge, Force Discharge, Export Limit
- Load profiles: Minimal, Family, EV, HeatPump, Custom
- Multi-battery support (1–3 modules) with even power distribution
- Timed charge/discharge schedules with SOC targets and midnight-wrapping
- Manual solar and load overrides for testing specific scenarios
- Battery SOH (state of health) — degrades with cycling, reduces effective capacity, adjustable per module
- Inverter DC power cap — battery charge/discharge limited by inverter type (Gen3 Hybrid 5 kW, AC Coupled 3 kW)
- Energy totals tracking — cumulative import, export, charge, discharge, solar, consumption kWh
- Fault injection — grid loss, inverter trip, battery over-temperature
- Save/load plant state to JSON with full roundtrip persistence

### Modbus Protocol
- GivEnergy proprietary Modbus TCP server — data-adapter framing (not standard Modbus)
- Read Input Registers (fn 0x04) and Read Holding Registers (fn 0x03)
- Write Single Register (fn 0x06) with command dispatch
- 75+ register catalogue: live readings (0–59), configuration (0–320), internal state (100–705)
- Proper TCP buffering with MBAP length framing
- Register projection from simulation state — registers update every tick

### GUI (Tauri v2)
- Desktop app with real-time dashboard
- Energy flow diagram, battery SOC gauge, power timeline chart, cumulative kWh cards
- Sidebar controls: inverter type, battery module count/capacity/SOH, inverter mode, weather, tick speed, solar/load overrides
- Start/pause/reset simulation controls
- Fault injection and clear buttons
- Battery module capacity dropdowns with float-precision matching
- SOH slider per module (50–100%)
- Save/load plant state buttons
- State sync on load only (doesn't overwrite user input during simulation)

### Headless CLI
- `giv-sim run scenario.yaml` with Modbus server support
- `--battery-count`, `--modbus`, `--output` flags
- Multi-day scenarios with `days: N`
- Exit code 1 on assertion failure (CI-friendly)

### Scenario DSL
- YAML event timeline with timed solar, load, mode, weather, fault events
- 13 assertion types for automated validation
- Multi-day support with daily event repetition

### Testing
- 215 tests across all crates
- Modbus integration tests covering GivEnergy protocol framing
- Persistence serialization tests
- Playwright GUI test scaffolding

### Output Formats
- JSON Lines recording (every tick)
- CSV energy export
- JUnit XML for CI
- JSON summary report

### Examples
- `basic_day.yaml`, `grid_outage.yaml`, `force_charge.yaml`, `weather_change.yaml`, `two_day_clear.yaml`
