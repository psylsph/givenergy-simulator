# Agent Guide — GivEnergy Plant Simulator

This file captures project conventions, gotchas, and workflow rules for AI coding agents.

## Golden Rule

**Always run `cargo fmt --check`, `cargo clippy --all-targets`, AND `cargo test` before every commit and push.** Never commit with formatting diffs, linter warnings, or failing tests.

- `cargo fmt --all -- --check` — must be clean (no diff).
- `cargo clippy --all-targets` — must produce **zero** warnings.
- `cargo test` — must be green. The suite is fast (~3s, 245 tests). Don't move on without green tests.

## Workspace

```
crates/
  sim-models/    — DeviceModel trait, PlantState (with solar_override/load_override), all sub-state types
  sim-core/      — SimulationEngine, Command enum (29 variants), all device model implementations
  sim-registers/ — RegisterDef, RegisterStore, RegisterSpace (Input/Holding), 500+ register catalogue
  sim-modbus/    — GivEnergy proprietary Modbus TCP server, CRC-16, transparent-message framing
  sim-scenarios/ — YAML DSL parser, assertion checking engine
  sim-faults/    — Fault definitions, FaultEngine device model
  sim-recording/ — JSON Lines recording, CSV export, JUnit XML, JSON report
  sim-storage/   — File I/O for recording load/save
  sim-api/       — Headless CLI binary (`giv-sim run`, `giv-sim replay`)
  sim-tauri/     — Tauri v2 desktop GUI (30 IPC commands, vanilla JS frontend)
ui/              — Web frontend (Vite + vanilla JS, served by Tauri on port 1420)
```

## Version

**0.14.4** — Three-phase force charge/discharge register fix + missing energy total registers.

**0.14.3** — Battery C-rate raised from 0.3C to 0.7C.

**0.14.2** — Non-zero starter energy totals for testing.

**0.14.1** — ACThreePhase voltage/ARM firmware guard fixes.

## Common Gotchas

### GivEnergy Modbus protocol is NOT standard Modbus TCP
The Wi-Fi dongle wraps all frames in a proprietary envelope:
```
Bytes 0-1:   Transaction ID      — fixed 0x5959
Bytes 2-3:   Protocol ID         — fixed 0x0001
Bytes 4-5:   Length               — bytes after this field
Byte  6:     Unit ID              — fixed 0x01
Byte  7:     Function ID          — 0x02 (transparent message)
Bytes 8-17:  Data-adapter serial  — 10 bytes, Latin-1, space-padded
Bytes 18-25: Padding              — big-endian u64 value 8
Byte  26:    Slave address        — 0x32 (reads), 0x11 (writes)
Byte  27:    Inner function code  — 0x03/0x04 (read), 0x06 (write)
Bytes 28+:   Inner payload
Last 2 bytes: CRC-16/Modbus over bytes 26+
```
Reference: `givenergy-local` project's `src-tauri/src/modbus/framer.rs`.

### Read response payload format
The server must prepend the 10-byte inverter serial to the response payload:
```
serial(10) + base_register(2) + register_count(2) + data(N×2)
```
The client parses this as: skip 10 bytes, read start/count, then register values.
Write responses follow the same pattern: `serial(10) + register(2) + value(2)`.

### Input vs Holding register spaces
Input registers (fn 0x04) and holding registers (fn 0x03) share addresses 0-119.
They are stored in separate internal ranges:
- Input: key = address (0-9999)
- Holding: key = address + 10000 (10000-19999)
Always use `store.read_by_space(addr, RegisterSpace::Input)` for IR and
`store.read_by_space(addr, RegisterSpace::Holding)` for HR.
`store.read(addr)` tries holding first, then input — use only for backward-compat.

### Battery power sign convention
Internal convention: `total_battery_power_kw` positive = charging.
GivEnergy wire convention: raw positive = discharging.
The register projection **negates** the value for IR 52 (battery power) and IR 51 (battery current).
The client decodes: `battery_power = -signed(raw)`, converting back to positive=charging.

### State sync pattern
`PlantState.battery` (singular) is a convenience field for `batteries[0]`.
Setting `state.battery` directly requires calling `state.sync_vec_from_battery()`.
Setting `state.batteries[i]` directly requires calling `state.sync_battery_from_vec()`.

### Device update order (critical)
```
Solar → Load → Inverter → Faults → Battery → EnergyTracker
```
When schedules are active: `ScheduleEngine → Solar → ...`
Never reorder this. BatteryEngine must see finalized power values.

### Manual overrides
`PlantState.solar_override: Option<f64>` and `load_override: Option<f64>`.
When `Some(w)`, the SolarEngine/LoadEngine uses the fixed value instead of computing.
Set to `None` to restore engine control. Commands: `SetSolarOverride`, `SetLoadOverride`.
Survive serialization via `#[serde(default)]`.

### Solar override applies before night check
Override is checked at the top of `SolarEngine::update()`, before the night-time zeroing.
This means `solar_override = Some(3000)` works at midnight.

### Dual PV arrays
When `PlantConfig.pv2_peak_watts > 0`, SolarEngine splits generation 45% PV1 / 55% PV2.
Override also splits 45/55. `SolarState` has `pv1_w` and `pv2_w` (generation_w = total).
PV2 voltage (IR 2) returns 350 V whenever `pv2_peak_watts > 0` so clients detect PV2.

### Inverter throughput caps
All inverter types have `max_ac_watts` and `max_batt_w` limits defined in
`crates/sim-tauri/src/commands.rs` (`max_batt_w` / `max_ac_watts` functions).
Battery charge/discharge in ALL modes is capped by both `inv_max_w` and battery C-rate (0.7C continuous, realistic for LFP modules).
`SetBatterySoH` recalculates limits from new capacity.

### Inverter types (DTC hex order)

0x2001 is a **family code** shared by Gen1/Gen2/Gen3 hybrids. The actual
generation is decided by HR(21) ARM firmware century (fw/100):
  - century 2 → Gen1Hybrid (arm_fw 252)
  - century 3 → Gen3Hybrid (arm_fw 318)
  - century 8/9 → Gen2Hybrid (arm_fw 852)
  - other centuries → Gen1Hybrid (default)

| Inverter | DTC | AC max | Battery limit | ARM FW |
|---|---|---|---|---|
| Gen1Hybrid | 0x2001 | 5000W | 2500W | 252 |
| Gen2Hybrid | 0x2001 | 5000W | 3600W | 852 |
| Gen3Hybrid | 0x2001 | 5000W | 3600W | 318 |
| Gen3Hybrid8kW | 0x2101 | 8000W | 8000W | — |
| Gen3Hybrid10kW | 0x2102 | 10000W | 10000W | — |
| ACCoupled | 0x3001 | 3000W | 3000W | — |
| ACCoupled2 | 0x3002 | 3000W | 3000W | — |
| ThreePhase | 0x4001 | 6000W | 6000W | — |
| ThreePhase8kW | 0x4002 | 8000W | 8000W | — |
| ThreePhase10kW | 0x4003 | 10000W | 10000W | — |
| ThreePhase11kW | 0x4004 | 11000W | 11000W | — |
| AllInOne6 | 0x8001 | 6000W | 6000W | — |
| AllInOne | 0x8002 | 6000W | 6000W | — |
| AllInOne5 | 0x8003 | 5000W | 5000W | — |
| AIO8kW | 0x8102 | 8000W | 8000W | — |
| AIO10kW | 0x8103 | 10000W | 10000W | — |

Dropdown and INVERTER_PRESETS are ordered by DTC hex value ascending.

### SolarEngine reads weather from PlantState.weather (string)
Weather is stored as a display string ("Clear", "PartlyCloudy", etc.), not as an enum field.
Set `state.weather = "Overcast".to_string()` to change weather.

### Schedule slots use HHMM encoding, disabled = 60
Charge/discharge slot registers use HHMM format (e.g. 1600 = 16:00, 630 = 06:30).
Value 60 is the "disabled" sentinel (minutes > 59 is invalid).

### Time sync from Modbus writes (HR 35-40)
Clients write year/month/day/hour/min/sec one register at a time.
The accumulator (`pending_time_regs: [Option<u16>; 6]`) collects across drain cycles.
When all 6 are present, a `SetSimulationTime` command is enqueued and the buffer resets.
This applies in both Tauri (`Arc<Mutex<...>>`) and CLI (local variable).

### #[tauri::command] in lib targets
Tauri v2 proc macros conflict with rustc 1.95+ in lib crates.
All `#[tauri::command]` functions must live in a separate `mod commands {}` block.
The main `lib.rs` only calls `generate_handler![commands::fn_a, commands::fn_b, ...]`.

### Tauri setup hook for async runtime
`tokio::spawn()` panics before Tauri's runtime is active.
Use `tauri::async_runtime::spawn()` inside the `.setup(|app| { ... })` hook.
The `.setup()` closure needs `move` keyword to own captured variables.

### Edition 2024 / resolver 2
Workspace uses `edition = "2024"` and `resolver = "2"`.
The sim-tauri crate overrides to `edition = "2021"` for Tauri compatibility.
Integer→float conversion is explicit: `SolarEngine::new(5000.0, 51.5)` not `new(5000, 51)`.

### Register snapshot uses u32 keys
`RegisterStore::snapshot()` returns `HashMap<u32, u16>` (composite key).
`snapshot_holding()` returns `HashMap<u16, u16>` (holding-only, backward compat).
Tests accessing snapshot must use `10000u32 + address` for holding registers.

### Frontend querySelectorAll returns a STATIC NodeList
Never use `querySelectorAll` in a loop that modifies the DOM — it's not live.
Use `while (container.children.length > count)` with `removeChild` instead.

### Clippy and CI
`cargo clippy --all-targets` must produce zero warnings.
Crate-level `#![allow(clippy::...)]` is used for non-fixable style issues.
CI pipeline: `cargo fmt --check`, `cargo clippy --all-targets`, `cargo test`, scenario regression.

## GivEnergy Register Map

### Input Registers (fn 0x04, slave 0x32) — IR 0-59
| Reg | Name | Scaling | Source |
|-----|------|---------|--------|
| 0 | Status | 1 | Always 1 (normal) |
| 1, 2 | PV1/PV2 voltage | ×0.1 V | 350V if generating |
| 5 | Grid voltage | ×0.1 V | Fixed 240V |
| 8, 9 | PV1/PV2 current | ×0.1 A | generation / 7000 |
| 13 | Grid frequency | ×0.01 Hz | Fixed 50Hz |
| 17, 19 | PV1/PV2 energy today | ×0.1 kWh | solar_kwh / 2 |
| 18, 20 | PV1/PV2 power | W | pv1_w / pv2_w |
| 25, 26 | Export/import today | ×0.1 kWh | energy_totals |
| 30 | Grid power | signed W | grid.power_w |
| 35 | Consumption today | ×0.1 kWh | load_consumption_kwh |
| 36, 37 | Battery charge/discharge today | ×0.1 kWh | energy_totals |
| 41 | Inverter temperature | ×0.1 °C | inverter temp |
| 50 | Battery voltage | ×0.01 V | 44 + SOC×0.08 |
| 51 | Battery current | signed ×0.01 A | **negated** (GE convention) |
| 52 | Battery power | signed W | **negated** (raw+ = discharge) |
| 56 | Battery temperature | ×0.1 °C | battery temp |
| 59 | Battery SOC | % | aggregate_soc() |

### Holding Registers (fn 0x03, slave 0x32) — HR 0-320
| Reg | Name | Access | Source |
|-----|------|--------|--------|
| 0 | Device type | RO | Per inverter DTC |
| 20 | Enable charge target | RW | 0 |
| 27 | Battery power mode | RW | 0=export, 1=eco |
| 29 | Calibration stage | RW | 0 (off) |
| 31-32 | Charge slot 2 start/end | RW (HHMM) | 60 (disabled) |
| 35-40 | System time year/sec | RW | From timestamp |
| 44-45 | Discharge slot 2 start/end | RW (HHMM) | 60 (disabled) |
| 50 | Active power rate | RW | 100% |
| 56-57 | Discharge slot 1 start/end | RW (HHMM) | 60 (disabled) |
| 59 | Enable discharge | RW | bool |
| 94-95 | Charge slot 1 start/end | RW (HHMM) | 60 (disabled) |
| 96 | Enable charge | RW | bool |
| 110 | Battery SOC reserve | RW | min_aggregate_soc |
| 111 | Battery charge limit | RW | 100% |
| 112 | Battery discharge limit | RW | 100% |
| 116 | Charge target SOC | RW | 100% |
| 163 | Inverter reboot | RW | 0 (write 100 to reboot) |
| 318 | Battery pause mode | RW | 0 |
| 319-320 | Pause slot 1 start/end | RW (HHMM) | 60 (disabled) |

### Simulator-Internal Registers (HR 100-705)
| Range | Category |
|-------|----------|
| 100-104 | Inverter (mode, ac_power, export_limit, temp, firmware) |
| 200-214 | Battery (SOC×3, power, voltage, current, temp, capacity, limits, efficiency, count) |
| 300-304 | PV (generation, voltage, current, energy_today, peak) |
| 400-404 | Grid (power, voltage, frequency, connected, load) |
| 500-505 | Energy totals (import, export, charge, discharge, solar, consumption kWh) |
| 600-602 | Config (battery_count, tick_interval, weather) |
| 700-705 | Schedules (charge/discharge start/end, target SOCs) |

## Running Tests

```bash
# Full suite (245 tests)
cargo test

# Single crate
cargo test -p sim-modbus

# Single test
cargo test -p sim-core -- cell_balancing

# Modbus integration tests only
cargo test -p sim-modbus --test givenergy_protocol

# With output
cargo test -- --nocapture
```

## Running the GUI

```bash
cd ui && npm install && cd ..
cd crates/sim-tauri && cargo tauri dev
```

## Build & Smoke Test

```bash
cargo build && cargo test    # should complete in ~5s total
```

## Persistence
Save path: `~/.local/share/com.givenergy.simulator/plant_state.json`
Format: `{ "plant": PlantState, "schedule": Option<Schedule> }`
Battery sizes: `BATTERY_SIZES = [2.6, 3.4, 5.2, 6.8, 7.0, 8.2, 9.5, 10.2, 12.8, 13.6, 16.0, 17.0, 19.0, 20.4]` (nearest-value matching). Up to 6 battery modules supported (LV packs at slave 0x32–0x37, or HV stacks).

## Network ports
| Port | Protocol | Purpose |
|------|----------|---------|
| 8899 | GivEnergy proprietary Modbus TCP (with envelope) | Inverter + battery + grid registers |
| 8898 | Standard Modbus TCP (no envelope) | GivEVC wallbox (HR 0-119) |
| 1420 | HTTP | Tauri dev server (UI) |

## Slot maps (per `givenergy-modbus` reference)
| Inverter class | Charge slots (start,end) | Discharge slots (start,end) |
|----------------|--------------------------|------------------------------|
| SINGLE_PHASE | (94,95), (31,32) | (56,57), (44,45) |
| EXTENDED (10-slot) | (94,95), (31,32), (246,247), (249,250), (252,253), (255,256), (258,259), (261,262), (264,265), (267,268) | (56,57), (44,45), (276,277), ..., (297,298) |
| THREE_PHASE | (1113,1114), (1115,1116), (246,247), ..., (267,268) | (1118,1119), (1120,1121), (276,277), ..., (297,298) |
| EMS | (2053,2054), (2056,2057), (2059,2060) | (2044,2045), (2047,2048), (2050,2051) |
Target SOC register follows each slot's end register (e.g. HR 248 for charge slot 3).

## Test count tracking
- v0.3.0: 37
- v0.3.1: 49
- v0.4.0: 54
- v0.5.0: 59
- v0.6.0: 82
- v0.7.0: 165
- v0.7.1: 216
- v0.8.0: 216
- v0.9.0: 217
- v0.10.0: 217
- v0.11.0: 219
- v0.11.1: 220
- v0.11.2: 223
- v0.12.0: 223
- v0.13.0: 235
- v0.14.0: 243
- v0.14.1: 244
- v0.14.2: 245
- v0.14.3: 245
- v0.14.4: 245
