# Changelog

All notable changes to the GivEnergy Plant Simulator are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

## [0.11.2] - 2026-06-04

### Added
- **DSP firmware reporting** (HR 19)
  - New `InverterState.dsp_firmware_version` field (serde-defaulted for
    backward compatibility)
  - Per-inverter-type defaults: Gen1=110, Gen2=230, Gen3=449, Plus=510,
    AC=305, ThreePhase=612, AIO=1010
  - Catalogue entry for HR 19 (read-only Holding register)
  - Live projection from `state.inverter.dsp_firmware_version`
- **ARM firmware runtime override** (HR 21)
  - `set_arm_firmware(version)` Tauri command
  - `InverterState.arm_firmware_version` (0 = use type default)
  - When non-zero, takes precedence over the per-type century default
- **DSP firmware runtime override**
  - `set_dsp_firmware(version)` Tauri command
- Firmware info shown live in the GUI 'Limits & Control' card with
  ARM century hint (e.g. "ARM FW: 352 (century 3)")
- Firmware override inputs in the sidebar (ARM FW / DSP FW + Set buttons)

### Tests
- `dsp_firmware_projects_from_inverter_state`
- `arm_firmware_override_takes_precedence_over_type_default`
- `arm_firmware_falls_back_to_type_default_when_zero`
- Total: 223 (was 220)

## [0.11.1] - 2026-06-04

### Added
- **Gen2Hybrid** inverter type (DTC 0x2001, arm_fw 852 → century 8)
  - Dropdown entry: "Gen 2 Hybrid (0x2001, FW 8xx)"
  - 5000W AC max, 3600W battery limit (same DC limit as Gen3)
  - `gen2_hybrid_shares_family_dtc_but_reports_century_8_firmware` test

### Changed
- **0x2001 is now treated as a family code**, not a Gen3-specific DTC,
  matching upstream `givenergy-modbus` / `giv_tcp`:
  - Gen1Hybrid (was 0x1001) → 0x2001 with arm_fw 252 (century 2)
  - Gen2Hybrid (new) → 0x2001 with arm_fw 852 (century 8)
  - Gen3Hybrid → 0x2001 with arm_fw 352 (century 3)
  - Plus variants → arm_fw 452 (century 4)
- Frontend dropdown labels updated to show FW century: "(0x2001, FW 2xx)"
- AGENTS.md inverter table extended with ARM FW column

## [0.11.0] - 2026-06-04

### Added
- **GivEVC (Electric Vehicle Charger) simulation** — full wallbox simulator
  - `EvcState` struct (enabled, charging_state, cable_status, error_code,
    active_power_w, L1/L2/L3 currents, charge_current_setting, charge_control,
    charging_mode, energy_kwh)
  - `EvcEngine` device model — simulates charging state machine, draws from grid
  - **Standard Modbus TCP server** on port 8898 (not proprietary framing)
    - FC 0x03 (read HR), 0x06 (write single), 0x10 (write multiple)
    - Serves HR 0-119 for the EVC wallbox
  - 6 new Tauri commands: `set_evc_enabled`, `set_evc_charge_control`,
    `set_evc_charge_current`, `set_evc_charging_mode`, `set_evc_cable_status`,
    `get_evc_state`
  - EVC control card in frontend (Enable/Plug/Start/Stop/Mode/Amps)
- **Slot 3-10 Modbus write routing** — HR 246-269 (charge) and HR 276-299
  (discharge) writes are now translated into the Schedule struct fields,
  matching the EXTENDED_SLOTS layout from `givenergy-modbus`

### Fixed
- Slot 3-10 schedule accumulator gap — writes to HR 246-299 were not being
  applied to the schedule (only projection was correct)
- All three slot maps align with `givenergy-modbus` upstream:
  - SINGLE_PHASE_SLOTS: charge (94,95),(31,32); discharge (56,57),(44,45)
  - EXTENDED_SLOTS: adds (246,247)...(267,268) and (276,277)...(297,298)
  - THREE_PHASE_SLOTS: slots 1-2 at HR 1113-1121, slots 3-10 reuse EXTENDED

### Tests
- New `slot_3_triggers_charge_during_window` test
- New `slot_10_triggers_discharge_during_window` test
- Total: 219 tests (was 217)

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
