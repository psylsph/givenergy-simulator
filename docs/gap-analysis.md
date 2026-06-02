# Gap Analysis: Design Docs vs Implementation

**Generated**: 2026-06-02  
**Workspace**: /home/stuart/repos/givenergy-simulator  
**Actual test count**: 183 (design docs say 82)

| Doc | Feature/Requirement | Status | Notes |
|-----|---------------------|--------|-------|
| **00-master-architecture** | PlantState as single source of truth | **Done** | PlantState struct fully implemented in sim-models |
| | Register banks as projections of state | **Done** | `project_from_state()` in sim-registers |
| | Deterministic execution | **Done** | Same inputs produce same outputs |
| | Headless CLI (sim-api) and Tauri GUI share same SimulationEngine | **Done** | Both use sim-core::SimulationEngine |
| | Device models are pluggable via DeviceModel: Send | **Done** | Trait in sim-models, used throughout |
| | SolarEngine with latitude/day-of-year/weather model | **Done** | Sinusoidal irradiance + weather factor |
| | LoadEngine with 4 built-in profiles + custom time-series | **Done** | Minimal, Family, EV, HeatPump + Custom interpolation |
| | InverterEngine with 5 modes + island mode + thermal model | **Done** | Normal, Eco, ForceCharge, ForceDischarge, ExportLimit + island |
| | BatteryEngine with SOC, thermal model, aging/degradation | **Done** | Full implementation with derating |
| | ScheduleEngine — timed charge/discharge windows | **Done** | In sim-core, wraps midnight |
| | FaultEngine with grid loss, inverter trip, battery over-temp | **Done** | In sim-faults, recovery on clear |
| | EnergyTracker — cumulative kWh totals | **Done** | Last device in chain |
| | RegisterStore — 45 registers across 7 categories | **Partial** | Catalogue has ~100+ registers now (expanded with GE-native IR/HR), far exceeding original 45. Design doc 00 is stale. |
| | ModbusServer with fn 0x03 read and fn 0x06 write | **Done** | Plus fn 0x04 read input registers |
| | ScenarioEngine — YAML DSL parser with assertions | **Done** | In sim-scenarios |
| | Device update order: Solar→Load→Inverter→Faults→Battery→EnergyTracker | **Done** | Confirmed in CLI and Tauri |
| | 82 unit tests | **Partial** | Actual count is 183 — docs severely undercount |
| **01-rust-crate-design** | 10 workspace crates | **Done** | All 10 present: sim-models, sim-core, sim-registers, sim-modbus, sim-scenarios, sim-faults, sim-recording, sim-storage, sim-api, sim-tauri |
| | sim-models: DeviceModel trait, TickContext, PlantState, all sub-state types | **Done** | All types present including ModeState, ModeSource |
| | sim-core: SimulationEngine, Command enum, all device models | **Done** | Plus ScheduleEngine, WeatherCondition |
| | Command enum with 7 variants | **Partial** | Code has 9 variants: adds SetSolarOverride, SetLoadOverride, SetSimulationTime (not in design) |
| | sim-registers: 45-register catalogue | **Partial** | Catalogue now ~100+ registers (23 GE-native Input, ~30 GE-native Holding, ~45 simulator-internal). Design doc is outdated. |
| | sim-modbus: fn 0x03 and 0x06 | **Done** | Plus fn 0x04 |
| | sim-scenarios: YAML DSL with 8 event types, assertion checking | **Done** | All event types and assertions implemented |
| | sim-api: `giv-sim run` and `giv-sim replay` CLI | **Done** | Both subcommands with full flag sets |
| | sim-tauri: 10 IPC commands, 4 events | **Partial** | Code has 15 commands (added set_solar_override, set_load_override, save_plant, has_saved_plant, load_plant) and 4 events. Design doc undercounts. |
| | PlantStateDto (frontend-friendly) | **Partial** | Dto is richer than spec: adds battery_mode, inverter_type, solar_override, load_override, battery_modules (per-module detail), schedule |
| **02-state-model** | PlantState with all documented fields | **Partial** | Code adds solar_override and load_override fields not in design |
| | InverterMode enum (5 variants) | **Done** | Normal, Eco, ForceCharge, ForceDischarge, ExportLimit |
| | InverterState with mode, ac_power_w, export_limit_w, temperature_celsius | **Partial** | Uses ModeState (effective + source) instead of flat mode field — richer than design |
| | BatteryState with all fields (soc, capacity, efficiency, thermal, aging) | **Done** | Plus voltage_v and current_a not in original design |
| | Multi-battery support (1–3 modules) | **Done** | batteries: Vec<BatteryState> |
| | Batch helpers: aggregate_soc, total_battery_capacity, etc. | **Done** | Plus max_aggregate_soc, min_aggregate_soc |
| | Command enum (7 variants) | **Partial** | Code has 9 (extra: SetSolarOverride, SetLoadOverride, SetSimulationTime) |
| | sync_battery_from_vec, sync_vec_from_battery, distribute_battery_power | **Done** | All present |
| **03-register-catalogue-strategy** | 7 register categories | **Partial** | Code has 6 categories (Inverter, Battery, PV, Grid, Configuration, Schedules). "Energy Totals" category is missing — those registers use C::Grid instead |
| | RegisterDef with address, name, category, type, scaling_factor, access | **Partial** | Code adds `space: RegisterSpace` field (Input vs Holding) — not in original design |
| | 45 registers across 7 categories | **Partial** | Now ~100+ registers with GE-native IR/HR addresses. Far exceeds original design |
| | Writable registers: 100, 102, 210, 211, 602 | **Partial** | Code has many more writable registers: GE-native HR 20, 27, 29, 31, 32, 35–40, 44, 45, 50, 56, 57, 59, 94, 95, 96, 110, 111, 112, 116, 163, 318, 319, 320 — all ReadWrite |
| | project_from_state() with scaling factor | **Done** | Plus battery BMS projection (IR 60-119) |
| **04-modbus-emulation** | GivEnergy proprietary MBAP variant | **Done** | Full implementation with CRC-16 |
| | fn 0x03 Read Holding Registers | **Done** | |
| | fn 0x04 Read Input Registers | **Done** | Design doc says "future" but it's implemented |
| | fn 0x06 Write Single Register | **Done** | With access validation and command dispatch |
| | CRC-16/Modbus on inner PDU | **Done** | Using crc crate |
| | Writable register → Command dispatch via mpsc | **Done** | Unbounded channel |
| | Multiple concurrent TCP connections | **Done** | tokio::spawn per connection |
| | Proper TCP buffering for partial reads | **Done** | Pending buffer pattern |
| | Multi-slave addressing for battery BMS (0x32–0x37) | **Done** | Not mentioned in original design; this is an extension |
| | Battery BMS data serving (IR 60-119) | **Done** | project_battery_bms() in sim-registers |
| | Function code 0x10 (Write Multiple Registers) | **Missing** | Listed as "future" in design doc, not implemented |
| | Packet capture comparison against real hardware | **Missing** | Listed as future |
| **05-tauri-ipc-contracts** | create_plant command | **Done** | Plus per-module battery config (battery_modules) not in original spec |
| | load_scenario command | **Done** | Returns ScenarioInfo with event details |
| | start_simulation command | **Done** | With speed and scenario_path params |
| | pause_simulation command | **Done** | |
| | inject_fault command (emits fault_triggered) | **Done** | |
| | clear_fault command | **Done** | |
| | set_mode command | **Done** | |
| | set_weather command | **Done** | |
| | export_recording command (csv/jsonl/json) | **Done** | Emits recording_saved |
| | get_current_state command | **Done** | |
| | set_solar_override command | **Done** | Extra command not in original 10-command design |
| | set_load_override command | **Done** | Extra command not in original 10-command design |
| | save_plant command | **Done** | Extra — persistence feature |
| | has_saved_plant command | **Done** | Extra — persistence feature |
| | load_plant command | **Done** | Extra — persistence feature |
| | state_changed event (every tick) | **Done** | |
| | fault_triggered event | **Done** | |
| | scenario_completed event | **Done** | |
| | recording_saved event | **Done** | |
| | Modbus TCP server on port 8899 | **Done** | Auto-started in Tauri setup |
| | Auto-load saved plant on startup | **Done** | Not in original design |
| **06-solar-engine** | Sinusoidal irradiance, latitude, day-of-year, weather | **Done** | Plus elevation factor for seasonal variation |
| | 4 weather conditions | **Done** | Clear (1.0), PartlyCloudy (0.6), Overcast (0.3), Storm (0.1) |
| | Historical weather import | **Missing** | Listed as future in doc |
| | Longitude input | **Missing** | SolarEngine only takes latitude, not longitude (not needed for basic model) |
| **07-load-engine** | 4 built-in profiles: Minimal, Family, EV, HeatPump | **Done** | All match doc specs |
| | Custom time-series profiles from YAML | **Done** | Linear interpolation, wraps at midnight |
| | CLI --profile flag for custom YAML path | **Done** | Falls back to Family if path invalid |
| **08-battery-algorithms** | SOC formula with charge/discharge efficiency | **Done** | |
| | Thermal model with ambient, heat rise, cooling | **Done** | ambient_for_hour() sinusoidal model |
| | Thermal derating: normal <45°C, linear 45–55°C, blocked >=55°C | **Done** | |
| | Aging model: throughput tracking, SOH degradation | **Done** | degradation_per_cycle=0.0002, min_soh=0.5 |
| | Cell balancing through natural divergence | **Done** | Per-module independent SOC tracking |
| | Multi-battery helpers | **Done** | |
| **09-inverter-behaviour** | Normal mode priority logic | **Done** | Solar→Load→Battery→Grid |
| | Eco mode (halved charge rate 10:00–16:00) | **Done** | |
| | ForceCharge mode | **Done** | Grid charges battery, solar covers load first |
| | ForceDischarge mode | **Done** | Battery exports to grid |
| | ExportLimit mode with curtailment | **Done** | |
| | Island mode (grid disconnected) | **Done** | Excess solar curtailed |
| | Inverter temperature model | **Done** | thermal_resistance=20°C/kW, cooling=0.5 |
| | ScheduleEngine: charge/discharge windows, midnight wrapping | **Done** | |
| **10-scenario-dsl** | YAML DSL with name, days, time-stamped events | **Done** | |
| | All 7 event field types | **Done** | solar, load, fault, clear_fault, mode, export_limit, weather |
| | Assertion types: soc_gt/lt, solar_gt/lt, grid_connected, grid_import/export_gt, battery_charging, no_faults, fault_active | **Done** | |
| | Energy totals assertions: solar_kwh_gt, grid_import_kwh_gt, grid_export_kwh_gt, load_kwh_gt | **Done** | |
| | Multi-day scenarios (days: N) | **Done** | Events repeat daily with date offset |
| | 5 example scenarios | **Partial** | 6 exist: basic_day, force_charge, grid_outage, weather_change, two_day_clear, shift_worker. Design listed 5 (no shift_worker) |
| | examples/profiles/ with custom load profiles | **Done** | shift_worker.yaml present |
| **11-recording-format** | JSON Lines (.jsonl) format | **Done** | With register_snapshot per frame |
| | CSV export with documented columns | **Done** | All columns match spec including energy totals |
| | JUnit XML output | **Done** | testsuite/testcase/failure structure |
| | JSON report | **Done** | ScenarioResult serialized |
| | giv-sim replay command | **Done** | |
| | giv-sim replay --diff for comparison | **Done** | diff_recordings() function |
| | giv-sim replay --format csv | **Partial** | Code supports summary, csv, json formats |
| **12-fault-framework** | 5 fault categories: Communication, Electrical, Sensor, Battery, Inverter | **Done** | FaultCategory enum |
| | 3 trigger types: Manual, Scheduled, Randomised | **Partial** | Types defined in FaultTrigger enum, but Scheduled and Randomised triggers are NOT actually implemented — only Manual injection works |
| | 5 well-known faults: grid_loss, battery_over_temp, inverter_trip, comm_timeout, sensor_drift | **Done** | All defined in catalogue |
| | FaultEngine device model | **Done** | Applies effects: grid disconnect, inverter zero, battery charge block |
| | Recovery on fault clear | **Done** | Grid restore, inverter/battery resume |
| | Scheduled fault triggers | **Missing** | FaultTrigger::Scheduled enum variant exists but no code fires faults at scheduled times |
| | Randomised fault triggers (probability-based) | **Missing** | FaultTrigger::Randomised enum variant exists with probability field but no code rolls dice each tick |
| | comm_timeout fault effect | **Missing** | Defined in catalogue but FaultEngine has no effect for it (empty match arm) |
| | sensor_drift fault effect | **Missing** | Defined in catalogue but FaultEngine has no effect for it (empty match arm) |
| **13-headless-ci-design** | CLI `giv-sim run scenario.yaml` | **Done** | |
| | JUnit XML output | **Done** | |
| | JSON report output | **Done** | |
| | CSV traces output | **Done** | |
| | GitHub Actions regression | **Done** | .github/workflows/ci.yml with test, lint, scenario jobs |
| | scripts/run-ci.sh regression runner | **Done** | Runs all examples/*.yaml scenarios |
| **14-testing-matrix** | 82 unit tests | **Partial** | Actual count is 183 — test matrix doc is severely outdated |
| | sim-core: 42 tests | **Partial** | sim-core now has 52 tests |
| | sim-registers: 6 tests | **Partial** | sim-registers now has 24 tests |
| | sim-modbus: 4 tests | **Partial** | sim-modbus now has 7 tests (includes givenergy_protocol.rs) |
| | sim-scenarios: 11 tests | **Done** | |
| | sim-faults: 3 tests | **Done** | |
| | sim-recording: 1 test | **Partial** | sim-recording now has 3 tests |
| | sim-api: 9 tests | **Done** | |
| | sim-storage: 1 test | **Done** | |
| | Combinatorial tests | **Partial** | Present but count exceeds doc — doc doesn't list all current tests |
| | Register test: input/holding space separation | **Done** | input_and_holding_dont_collide test |
| | GE-native holding register tests | **Done** | system_time, schedule_slots, pause_mode, etc. |
| **15-backlog** | All 7 Epics marked complete | **Done** | All implemented |
| | Future Ideas: thermal, aging, cell balancing, inverter temp, custom profiles, SOH | **Done** | All implemented |
| **16-sequence-diagrams** | Read path (fn 0x03) | **Done** | Matches diagram flow |
| | Write path (fn 0x06) | **Done** | Matches diagram flow |
| | Simulation tick order | **Done** | Matches documented order |
| | Multi-day scenario interaction | **Done** | Events repeat daily |
| | ScheduleEngine interaction | **Done** | Registered before InverterEngine |
| **17-roadmap** | Phase 1: MVP scaffold | **Done** | |
| | Phase 2: Functional simulator | **Done** | |
| | Phase 3: Hardware-accurate Modbus | **Done** | Expanded beyond original scope with GE-native registers |
| | Phase 4: Tauri GUI | **Done** | With energy flow diagram, SOC gauges, power timeline, battery modules view |
| | 10 workspace crates | **Done** | |
| | 45 registers | **Partial** | Now ~100+ — doc is stale |
| | 5 writable registers | **Partial** | Now 30+ writable registers (GE-native + internal) |
| | 5 fault types | **Done** | |
| | 4 load profiles + custom | **Done** | |
| | 10 GUI commands | **Partial** | Now 15 commands |
| | 4 GUI events | **Done** | |
| | 2 Modbus function codes | **Partial** | Now 3 (0x03, 0x04, 0x06) |
| | examples/ directory with 5 scenario YAMLs | **Partial** | Now 6 scenarios + 1 profile |
| | scripts/run-ci.sh | **Done** | |
| | .github/workflows/ci.yml | **Done** | |
| | ui/ web frontend | **Done** | Full single-file HTML/CSS/JS dashboard with energy flow, battery modules, power timeline canvas |

## Summary of Gaps

### Missing Features (design calls for, code doesn't deliver)
1. **Scheduled fault triggers** (doc 12) — enum exists, no implementation
2. **Randomised fault triggers** (doc 12) — enum + probability field exist, no implementation
3. **comm_timeout fault effect** (doc 12) — defined but no-op
4. **sensor_drift fault effect** (doc 12) — defined but no-op
5. **Fn 0x10 Write Multiple Registers** (doc 04) — listed as future
6. **Historical weather import** (doc 06) — listed as future
7. **Longitude parameter in SolarEngine** (doc 06) — listed as input but not used

### Code Exceeds Design (implemented but not documented)
1. **GE-native register sets** — ~60 additional registers (Input + Holding) for real GivEnergy compatibility
2. **Battery BMS data** — IR 60-119 projection with multi-slave addressing (slaves 0x32–0x37)
3. **183 tests** — docs claim 82
4. **15 Tauri commands** — docs claim 10 (added overrides, save/load plant)
5. **9 Command enum variants** — docs claim 7
6. **Plant persistence** — save/load/has_saved_plant commands, auto-load on startup
7. **Solar/load manual overrides** — SetSolarOverride, SetLoadOverride commands
8. **Inverter type support** — Gen3/AC/AIO/3-phase variants with DTC codes
9. **Fn 0x04 Read Input Registers** — docs said "future" but it's implemented
10. **Battery voltage and current estimation** — not in original BatteryState design
11. **ModeState with ModeSource tracking** — User/Schedule/Fault source tracking

### Stale Documentation
1. **Test counts** (docs 00, 14, 17) — 82 claimed, 183 actual
2. **Register count** (docs 00, 01, 03, 17) — 45 claimed, ~100+ actual
3. **Writable register count** (doc 17) — 5 claimed, 30+ actual
4. **Tauri command count** (docs 01, 17) — 10 claimed, 15 actual
5. **Modbus function codes** (doc 17) — 2 claimed, 3 actual
6. **RegisterCategory** (doc 03) — 7 claimed, 6 actual (Energy Totals folded into Grid)
7. **Missing Energy Totals register category** — energy registers use Grid category instead
8. **Doc 04 Future section** says fn 0x04 is future — but it's implemented
