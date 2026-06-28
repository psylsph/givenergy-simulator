# Changelog

All notable changes to the GivEnergy Plant Simulator are documented here.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/).

## [Unreleased]

### Added

- **Grid Port Max Power Output sidebar control** — a new "Grid Port Max
  Power Output" group in the Controls sidebar that auto-detects the
  relevant wire register from the inverter family (per a manual audit
  of the giv_tcp `model/{baseinverter,threephase,ems,gateway}.py`
  register maps cross-checked against givenergy-modbus):
  - **Single-phase / AC-coupled / Gen1-4 / PV / AIO / Polar / Gen3+** —
    **read-only display** of `ge_hr_grid_port_max_power_output` (HR 26).
    The Set button is disabled because givenergy-modbus defines HR 26
    with no setter. The displayed value comes from
    `PlantState.config.max_ac_watts` (the plant configuration cap).
  - **Three-phase / HV / ACThreePhase** — **read + write** of
    `p_export_limit` (HR 1063, `C.deci` / ×0.1 encoding). The GUI shows
    user-friendly watts; the simulator multiplies by 10 on the way to
    the register (clamped to 65535 dW = 6553.5 W). Backend clamps to
    6500 W (givenergy-modbus `max=6500`) before the encoding step.
  - **EMS / EmsCommercial / Gateway** — **read + write** of
    `ems_export_power_limit` (HR 2071, raw `C.uint16`).
  Two new Tauri commands, `get_grid_port_max_power` and
  `set_grid_port_max_power`, route the user-friendly watt value to the
  correct register via a `GridPortPowerFamily` classifier (3 variants:
  `SinglePhase`, `ThreePhase`, `Ems`). The classifier is mirrored in
  JavaScript so the sidebar label, register hint, and Set-button
  enabled-state follow the inverter type without an extra round trip.

- **Register projections** — three new register defs / projection branches
  in `sim-registers`:
  - `tph_hr_p_export_limit` (HR 1063, Holding, ReadWrite, ×0.1) for
    three-phase / HV / AIO, projected from `state.inverter.export_limit_w`.
  - `ems_export_power_limit` (HR 2071) now projected from the live
    `state.inverter.export_limit_w` instead of `schedule.export_power_limit_w`
    so user edits via the new control take effect immediately. The
    schedule engine still drives the value during export windows via
    `SetExportLimit` (unchanged). The schedule projection no longer
    overwrites HR 2071.
  - `modbus_address_to_command` (both `sim-tauri` and `sim-api`) now
    routes HR 102 → `SetExportLimit`, HR 1063 → `SetExportLimit(value / 10)`,
    and HR 2071 → `SetExportLimit(value)`, so a Modbus client write
    reaches the same state field as the GUI.
  - `set_grid_port_max_power` clamps per family: HR 1063 caps at 6500 W
    (givenergy-modbus `max=6500`); HR 2071 caps at 65535 W (16-bit u16
    register ceiling; givenergy-modbus does not constrain this register).
    The old single-clamp of 6500 W blocked EMS users from setting a
    reasonable high cap and produced a value/range mismatch in the GUI's
    help text. The helpers `GridPortPowerFamily::max_w` and
    `clamp_watts` carry the per-family limits and are unit-tested in
    `sim-tauri::commands::tests`.

### Changed

- **`is_schedule_register`** in `sim-tauri::commands` and `sim-api::main`
  no longer matches HR 2071. The schedule accumulator would otherwise
  race the new direct-projection path. The address mapping
  `2062..=2071` becomes `2062..=2070`.

- **`scripts/run-pi.sh`** — a launcher script that exports the WebKitGTK
  env vars (`WEBKIT_DISABLE_COMPOSITING_MODE=1`,
  `WEBKIT_DISABLE_DMABUF_RENDERER=1`) needed to work around the
  garbled-display issue seen when running the Tauri GUI on Raspberry Pi 4
  with Raspberry Pi OS (Debian trixie). The script accepts `cargo` as the
  first argument to launch `cargo tauri dev` with the same fixes, and a
  `GIVSIM_FORCE_GPU=1` opt-out for users who want GPU compositing back.
  Documented in the README under the "Raspberry Pi / Linux desktop"
  quick-start section.

### Fixed

- **Timed Discharge (HR 318-320 battery-pause slot) GUI.** Two display
  bugs:
  - `ScheduleDto::from_state` hard-coded the pause-slot fields
    (`battery_pause_mode` / `pause_slot_start` / `pause_slot_end`) to the
    disabled sentinels (0 / 60 / 60), so Modbus writes to HR 318-320 never
    surfaced in the GUI even though `PlantState` was updated correctly.
    The DTO now reads them from the live state, so a client write is
    visible after the next refresh.
  - The schedule card (renamed **"Pause Slot" → "Timed Discharge"**) had
    an inverted visibility rule — it appeared for everything *except*
    AC-coupled inverters. It now renders only for the AC-output families
    that actually serve the HR 318-320 block: AC-coupled (0x3001/0x3002),
    AC three-phase (0x60xx) and residential All-in-One (0x80xx). DC
    hybrids (20xx/21xx/22xx), three-phase/HV register-bank families
    (40xx/41xx/81xx/82xx), and others are hidden. Covered by new Rust
    unit tests (`ScheduleDto` pause slot) and 13 Playwright tests across
    inverter families.

## [0.14.4] - 2026-06-06

### Added

- **Three-phase force charge/discharge registers now correct**: HR 1122
  (`FORCE_DISCHARGE_ENABLE`) and HR 1123 (`FORCE_CHARGE_ENABLE`) now set
  inverter mode to `ForceDischarge`/`ForceCharge` instead of being conflated
  with the scheduled charge/discharge flags. HR 1112 (`AC_CHARGE_ENABLE`)
  continues to control scheduled AC charging. Updated register projection
  to reflect inverter mode state.
- **Missing three-phase energy total (lifetime) registers**: Added 30 new
  RegisterDef entries for the three-phase IR 1360-1413 energy block:
  `e_inverter_out_today`/`total` (IR 1360-1363),
  `e_pv1_total` (IR 1368-1369), `e_pv2_total` (IR 1372-1373),
  `e_pv_total` (IR 1374-1375), `e_ac_charge_today`/`total` (IR 1376-1379),
  `e_import_total` (IR 1382-1383), `e_export_total` (IR 1386-1387),
  `e_battery_discharge_total` (IR 1390-1391),
  `e_battery_charge_total` (IR 1394-1395),
  `e_load_total` (IR 1398-1399),
  `e_export2_today`/`total` (IR 1400-1403),
  `e_pv_today` (IR 1412-1413).
  All project from the same cumulative EnergyTotals bucket (the simulator
  has no separate daily-reset counter).

### Fixed
- **Three-phase force charge/discharge register routing**: HR 1122 and
  HR 1123 are no longer treated as schedule registers in the Modbus write
  drain loop. They are now handled by `modbus_address_to_command` in both
  the Tauri and CLI code paths, enqueuing `SetInverterMode(ForceDischarge)`
  / `SetInverterMode(ForceCharge)`. Register projection reads inverter mode
  to set `tph_force_discharge_enable` / `tph_force_charge_enable`.

### Tests
- Updated `threephase_11kw_publishes_live_data_on_tph_input_registers` to
  verify all new energy total registers.
- 245 total (unchanged).

## [0.14.3] - 2026-06-06

### Changed

- **Battery C-rate raised from 0.3C to 0.7C**: The 0.3C continuous-current
  cap was strangling normal configurations — e.g. a 9.5 kWh battery on a
  Gen3 hybrid (0x2001, 3600 W inverter cap) was clamped to ~2850 W
  instead of the inverter's full 3600 W. 0.7C is realistic for LFP
  modules and lets every battery ≥ 5.2 kWh hit the inverter cap on a
  Gen3 hybrid. Updated in all five sites: `create_plant`, `set_battery_soh`,
  `SetBatterySoH` command handler, `BatteryEngine::update` scaling, and
  the `combo_*` test helper.

### Tests
- Updated `battery_engine_treats_100_percent_as_full_power_and_50_as_half_power`
  to use the new c-rate cap (7 kW at 10 kWh × 0.7C).
- 245 total (unchanged).

## [0.14.2] - 2026-06-05

### Added

- **Non-zero starter energy totals for testing**: New plants, saved-plant
  loads, and `giv-sim serve` now boot with small non-zero daily energy
  counters (PV today 8.5 kWh, grid import 1.5 kWh, grid export 2.5 kWh,
  battery charge 3.5 kWh, battery discharge 4.5 kWh, load consumption 6.5 kWh,
  AC charge 0.7 kWh). Lets external clients and tests read realistic values
  from all energy registers (single-phase IR 17/25/26/36/37/44 and the
  three-phase high IR 1366-1397 block) before a full day of simulation has
  run. Real simulated values still take precedence — the fixture only kicks
  in when every energy bucket is exactly zero.
- `EnergyTotals::non_zero_test_fixture()`, `is_all_zero()`, and
  `seed_for_testing_if_zero()` helpers in `sim-models`.

### Tests
- 245 total (244 → 245). New test
  `zero_energy_state_projects_fixture_energy_registers_for_all_inverter_types`
  exercises PV/grid/battery/load energy registers across 24 inverter types,
  including the three-phase high IR energy block.

## [0.14.1] - 2026-06-05

### Fixed

- **ACThreePhase nominal voltage** (HR 55): ACThreePhase (0x6001) now uses
  76.8V battery capacity voltage like other three-phase variants.
- **ACThreePhase ARM firmware** (HR 21, IR 1327): Now defaults to 612
  instead of falling through to the Gen3 hybrid default of 318.
- Three-phase `starts_with("ThreePhase")` guards also check
  `== "ACThreePhase"` for consistency.

### Tests
- 244 total (243 → 244).

This release adds the three-phase **Input Register** block (IR 1001-1413) so
clients can read PV, grid, battery, EPS, firmware and energy-total data from
the correct three-phase addresses. Also fixes the three-phase battery-capacity
nominal voltage and adds CT/meter import/export registers.

### Added

#### Three-phase Input Register block (IR 1001-1413)
3-phase clients read all live data from these high input-register addresses
rather than the single-phase IR 0-59 block:
- **PV**: IR 1001/1002 (voltage), 1009/1010 (current), 1017-1020
  (power, uint32 ×0.1W) — mirrors single-phase IR 1/2/8/9/18/20
- **Grid**: IR 1061-1063 (per-phase voltage 240V), 1064-1066
  (per-phase current), 1067 (frequency), 1068 (power factor),
  1069-1070 (inverter output, int32 ×0.1W), 1073-1074 (apparent power),
  1075 (system mode), 1076 (status)
- **CT / Meter power** (IR 1079-1080 import, 1081-1082 export,
  1240-1241 export alt, 1244-1245 second meter) — positive-only uint32
  split by direction, mirroring single-phase's signed IR 30
- **Load**: IR 1083-1085 (per-phase, each ⅓ of demand), 1089-1090 (total,
  uint32 ×0.1W)
- **Battery**: IR 1124 (DC status), 1128 (inverter temperature),
  1131 (BMS voltage), 1132 (SoC), 1136-1137 (discharge power),
  1138-1139 (charge power), 1140 (battery current)
- **EPS**: IR 1180 (nominal frequency), 1181-1183 (per-phase output
  voltage) — all 0 when EPS inactive
- **Firmware**: IR 1325-1326 (DSP version), 1327 (ARM version) — mirrors
  HR 19/21
- **Energy totals**: IR 1366-1367 (PV1 today), 1370-1371 (PV2 today),
  1380-1381 (import today), 1384-1385 (export today),
  1388-1389 (battery discharge today), 1392-1393 (battery charge today),
  1396-1397 (load today) — all uint32 ×0.1kWh

#### Three-phase register catalogue
- 56 new `RegisterDef` entries covering the IR 1001-1413 three-phase block

### Fixed

- **HR 55 battery capacity Ah**: the `ThreePhase8kW`/`10kW`/`11kW` variants
  now use 76.8V nominal voltage (was falling through to 51.2V single-phase
  default, giving a 50% over-count that could trigger BMS alarms).
  Uses `starts_with("ThreePhase")` guard.

### Tests
- 8 new tests (235 → 243 total): three-phase DTC, phase-count byte, 76.8V
  battery capacity, HR 1108/1110 limit mirrors, HR 1113-1121 schedule
  mirrors, HR 1111 charge-target mirror, comprehensive live-data register
  projection, CT import/export sign convention.

## [0.13.0] - 2026-06-05

This release broadens upstream register coverage (mapped against
`givenergy-modbus`, `giv_tcp`, and `givenergy-local`), corrects the
HR111/HR112 battery power-limit semantics, and adds the missing
register mirrors that 3-phase clients read for the same fields.

### Added

#### Register catalogue
- ~190 additional register definitions across Input / Holding /
  3-phase / High-Energy / Metering address spaces, including:
  - PV totals, alt-format energy registers, combined-generation counters
  - Inverter input/output, charger temperature, EPS voltage & frequency
  - Work-time counter (IR 47-48) projected from a new
    `InverterState.work_time_hours` runtime field that ticks forward
    every simulation step
  - Fault code (IR 39-40) driven by `state.active_faults`
  - Load demand (IR 42), AC power (IR 24/43), alt battery throughput
    registers (IR 180-183, 247-248)
  - High-Energy alt holding block (HR 4107-4142) for solar peak,
    battery charge/discharge, export totals
  - 3-phase metering registers (HR 1000-1099) — voltages, currents,
    active/reactive/apparent power per phase and total
- **HR199** `enable_inverter_parallel_mode` — modelled end-to-end:
  new `Command::SetEnableInverterParallelMode`, new
  `PlantState.enable_inverter_parallel_mode`, projection and Modbus
  write routing (Tauri + CLI)
- **BMS extension registers** (IR 90, 94, 101-102, 115) — charge
  status byte, warning code, design-capacity mirror, USB-inserted
  flag — all projected from `BatteryState`
- **DTC aliases** for `ThreePhase8kW/10kW/11kW`, `ACThreePhase`, plus
  Polar / Plus variants where previously missing

#### UI
- "Reserve" label renamed to **"Minimum SOC"** in the Limits & Control
  card
- New read-only rows in Limits & Control:
  - **Charge Power Limit** (% of max)
  - **Discharge Power Limit** (% of max)
  - **Inverter Max Output** (W)
- New `PlantStateDto` fields feed the new rows:
  `inverter_max_output_w`, `charge_power_limit_percent`,
  `discharge_power_limit_percent`

### Changed

#### Battery power-limit semantics
- **HR111 / HR112 now use 0-100% where 100 = full power.**
  Previously the simulator treated them as 0-50 with 50 as full
  power, which contradicted the UI labels and led to clients
  under-charging / under-discharging after a fresh boot.
  - `PlantState::new()` / `with_battery_count()` defaults are now
    `100.0` (was `50.0`)
  - Serde default for old persisted state is also `100.0`
  - `Command::SetBatteryChargeLimit` / `SetBatteryDischargeLimit`
    clamp to `0..=100` (was `0..=50`)
  - `BatteryEngine` scaling divides by `100.0` (was `50.0`)
- **HR313 / HR314 (AC-coupled) and HR1108 / HR1110 (3-phase) battery
  limit mirrors** now project from the same source field as HR111/112.
  Previously these catalogue entries had no projection rule, so
  3-phase clients reading them saw `0%` and refused to charge or
  discharge at full power.

#### DTO defensive defaulting
- `PlantStateDto` reports charge / discharge power limits as `100%`
  when the underlying state value is `<= 0.0` — protects against stale
  persisted state or transient zero values surfacing in the UI as
  `0%`.
- Frontend `pctOrDefault()` does the same guard for missing / null /
  non-positive payloads.

### Tests
- 12 new tests covering the changes above (223 → 235 total):
  - `sim-core`: defaults at 100%, command clamping, scaling at 100%
    vs 50%, work-time tick increment, parallel-mode command
  - `sim-registers`: HR111/112/313/314/1108/1110 default to 100%,
    new IR projections (24/42/43/47/48/180-183/247-248/39-40),
    HR199 writable, HR4107-4142 alt block, BMS IR 90/94/101-102/115
  - `sim-tauri`: DTO exposes new fields, zero→100% fallback

## [0.12.0] - 2026-06-04

This release closes several gaps in the inverter identification protocol,
adds full DSP firmware reporting, polishes the schedule UI for the
10-slot EXTENDED_SLOTS inverters, and rounds out the GivEVC simulator
that shipped in 0.11.0.

### Added

#### Inverter identification & firmware
- **Gen2Hybrid** inverter type — DTC 0x2001 with ARM firmware 852
  (century 8), 5000W AC / 3600W battery limit. Matches the reference
  refinement logic in `givenergy-modbus` / `giv_tcp`.
- **DSP firmware reporting** (HR 19)
  - `InverterState.dsp_firmware_version` field (serde-defaulted for
    backward compatibility with existing save files)
  - Catalogue entry for HR 19 (read-only Holding register)
  - Live projection from `state.inverter.dsp_firmware_version`
  - Per-inverter-type defaults: Gen1=110, Gen2=230, Gen3=449, Plus=510,
    AC=305, ThreePhase=612, AIO=1010
- **ARM firmware runtime override** (HR 21)
  - `InverterState.arm_firmware_version` (0 = use type default)
  - When non-zero, takes precedence over the per-type century default,
    letting you flip Gen1/Gen2/Gen3 identification against the shared
    0x2001 family DTC at runtime
- **Runtime DSP firmware override** via `set_dsp_firmware(version)`
- **Runtime ARM firmware override** via `set_arm_firmware(version)`
- Firmware info shown live in the GUI 'Limits & Control' card with
  ARM century hint (e.g. "ARM FW: 352 (century 3)")
- Firmware override inputs in the sidebar (ARM FW / DSP FW + Set buttons)

#### Schedule display
- All 10 charge + discharge slots now rendered inline for inverters that
  support EXTENDED_SLOTS (Gen3Hybrid, Gen2Hybrid, Plus, AllInOne, AIO,
  Polar, ThreePhase). Gen1Hybrid and AC-coupled still show 2 slots only.
- Slots 3–10 rendered as color-coded mini-cards with green (charge) /
  orange (discharge) left borders, grouped under labelled headers in a
  4-column grid
- Disabled slots (HHMM 60/60 sentinel) rendered at 35% opacity with an
  empty dot (◯); active slots get a filled dot (●) at full opacity
- Legend in section header: "◯ disabled  ● active"
- Global `enable_charge` / `enable_discharge` flags surfaced as coloured
  badges in the Limits & Control card instead of being incorrectly shown
  as "enabled" on every slot card

### Changed

#### 0x2001 is now a family code (was Gen3-specific)
Per upstream `givenergy-modbus` / `giv_tcp`, the actual generation is
decided by `HR(21) / 100` (the "century"):

| Century (HR(21)/100) | Detected as |
|---|---|
| 2 (200-299) | Gen1Hybrid |
| 3 (300-399) | Gen3Hybrid |
| 4-7 | Gen1Hybrid (default) |
| 8-9 (800-999) | Gen2Hybrid |

- Gen1Hybrid (was 0x1001) → 0x2001 with arm_fw 252 (century 2)
- Gen2Hybrid (new) → 0x2001 with arm_fw 852 (century 8)
- Gen3Hybrid → 0x2001 with arm_fw 352 (century 3)
- Gen3 Plus variants → arm_fw 452 (century 4)

Frontend dropdown labels updated to show the FW century, e.g.
"Gen 2 Hybrid (0x2001, FW 8xx)".

#### Schedule card per-slot state
Slot cards used to share a single `enable_charge` / `enable_discharge`
flag, so writing any charge slot made BOTH slot 1 and slot 2 display as
enabled. Each card now derives state purely from its own window
(● Active / ◯ Idle).

### Fixed
- Slot 3–10 Modbus write routing — HR 246–269 (charge) and HR 276–299
  (discharge) writes are now translated into the Schedule struct fields,
  matching the EXTENDED_SLOTS layout. Previously only the projection was
  correct; client writes were silently dropped.
- All three slot maps now align with `givenergy-modbus` upstream:
  - SINGLE_PHASE: charge (94,95),(31,32); discharge (56,57),(44,45)
  - EXTENDED: adds (246,247)...(267,268) and (276,277)...(297,298)
  - THREE_PHASE: slots 1–2 at HR 1113–1121, slots 3–10 reuse EXTENDED

### Tests
- `slot_3_triggers_charge_during_window`
- `slot_10_triggers_discharge_during_window`
- `gen2_hybrid_shares_family_dtc_but_reports_century_8_firmware`
- `dsp_firmware_projects_from_inverter_state`
- `arm_firmware_override_takes_precedence_over_type_default`
- `arm_firmware_falls_back_to_type_default_when_zero`
- Total: **223 tests** (was 217)

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
