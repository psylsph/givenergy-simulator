# Agent Guide — GivEnergy Plant Simulator

This file captures project conventions, gotchas, and workflow rules for AI coding agents.

## Golden Rule

**Always run `cargo test` after every change.** The suite is fast (~3s, 165 tests). Don't move on without green tests.

## Workspace

```
crates/
  sim-models/    — DeviceModel trait, PlantState (with solar_override/load_override), all sub-state types
  sim-core/      — SimulationEngine, Command enum (12 variants), all device model implementations
  sim-registers/ — RegisterDef, RegisterStore, RegisterSpace (Input/Holding), 75+ register catalogue
  sim-modbus/    — GivEnergy proprietary Modbus TCP server, CRC-16, transparent-message framing
  sim-scenarios/ — YAML DSL parser, assertion checking engine
  sim-faults/    — Fault definitions, FaultEngine device model
  sim-recording/ — JSON Lines recording, CSV export, JUnit XML, JSON report
  sim-storage/   — File I/O for recording load/save
  sim-api/       — Headless CLI binary (`giv-sim run`, `giv-sim replay`)
  sim-tauri/     — Tauri v2 desktop GUI (15 IPC commands, vanilla JS frontend)
ui/              — Web frontend (Vite + vanilla JS, served by Tauri on port 1420)
```

## Version

**0.7.0** — GivEnergy-native Modbus protocol, full register map, manual overrides.

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

### SolarEngine reads weather from PlantState.weather (string)
Weather is stored as a display string ("Clear", "PartlyCloudy", etc.), not as an enum field.
Set `state.weather = "Overcast".to_string()` to change weather.

### Schedule slots use HHMM encoding, disabled = 60
Charge/discharge slot registers use HHMM format (e.g. 1600 = 16:00, 630 = 06:30).
Value 60 is the "disabled" sentinel (minutes > 59 is invalid).

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
| 18, 20 | PV1/PV2 power | W | generation / 2 |
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
| 0 | Device type | RO | 0x2001 (Gen3 Hybrid) |
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
# Full suite (165 tests)
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

## Test count tracking
- v0.3.0: 37
- v0.3.1: 49
- v0.4.0: 54
- v0.5.0: 59
- v0.6.0: 82
- v0.7.0: 165
