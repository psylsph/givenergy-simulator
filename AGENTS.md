# Agent Guide — GivEnergy Plant Simulator

This file captures project conventions, gotchas, and workflow rules for AI coding agents.

## Golden Rule

**Always run `cargo fmt --check`, `cargo clippy --all-targets`, AND `cargo test` before every commit and push.** Never commit with formatting diffs, linter warnings, or failing tests.

- `cargo fmt --all -- --check` — must be clean (no diff).
- `cargo clippy --all-targets` — must produce **zero** warnings.
- `cargo test` — must be green. The suite is fast (~3s, 405 tests). Don't move on without green tests.

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

**0.17.1** — Timed Discharge (HR 318-320 battery-pause slot) GUI fixes.
The schedule card is renamed **"Pause Slot" → "Timed Discharge"** and two
bugs are fixed: (1) `ScheduleDto::from_state` hard-coded the pause-slot
fields to the disabled sentinels, so Modbus writes to HR 318-320 never
surfaced in the GUI even though `PlantState` was updated correctly — the
DTO now reads them from live state; (2) the card's visibility rule was
inverted, so it now renders only for the AC-output families that serve
the HR 318-320 block (AC-coupled 0x3001/0x3002, AC three-phase 0x60xx,
residential All-in-One 0x80xx) and is hidden for DC hybrids
(20xx/21xx/22xx) and three-phase/HV register-bank families
(40xx/41xx/81xx/82xx). Gen3 Hybrid (0x2001, ARM FW 3xx) is also visible
— confirmed against a physical inverter and givenergy-modbus, which
declares `battery_pause_mode` (HR 318) in the single-phase getter.
Gen1/Gen2 (0x2001, FW 2xx/8xx) stay hidden pending confirmation. Covered
by new Rust unit tests and 13 Playwright tests across inverter families.
See commit log for older changelogs.

**0.17.2** — Timed Discharge pause-window semantics fix. The `BatteryEngine`
pause evaluation only handled normal `[start, end)` windows; when the GivEnergy
portal writes "Timed Discharge" it sets `battery_pause_mode = 2` (PauseDischarge)
and writes the pause window as the **complement** of the desired discharge
slot, which wraps midnight (`start > end`, e.g. 03:00–04:00 discharge →
pause 04:00→03:00). The wrap-around case fell through to a dead `else { false }`
arm, so Timed Discharge had no effect. `sim-core` now honours both windows
(`hour >= start || hour < end` for the wrap case) and treats `start == end` as
disabled (covers our `60` sentinel and givenergy-modbus's `0`). Also fixed:
`modbus_address_to_command(318)` in both `sim-tauri` and `sim-api` returned a
`SetBatteryPause { start: 60, end: 60 }` that clobbered the window, and the
`sim-api` CLI path dropped HR 319/320 writes entirely — both now reconcile
318/319/320 into one `SetBatteryPause` preserving unwritten fields. Confirmed
against `britkat1980/giv_tcp` (`GivLUT.battery_pause_mode`, `baseinverter.py`
HR 318-320 = single `battery_pause_slot_1`). 8 new `sim-core` unit tests.

**0.17.3** — Inverter temperature override (GUI + CLI). Adds a way to pin
the inverter temperature, bypassing the `InverterEngine` thermal model —
useful for holding a fixed temperature to exercise derating / over-
temperature behaviour (e.g. hold 70 °C). New `InverterState.temperature_override:
Option<f64>` (serde `#[serde(default)]`, persists across save/load), driven by a
new `Command::SetInverterTemperature(Option<f64>)`. When `Some(t)` the
thermal model is skipped and `temperature_celsius` is held at `t` (clamped to
the model's [-10, 80] °C range); `None` restores the model. Exposed via:

* **GUI** — a new "Inv Temp °C" Set/Clear row in the sidebar (mirrors the
  ARM/DSP firmware overrides), the live temperature readout beside it, and a
  `🌡️` badge in the override indicator banner. New Tauri command
  `set_inverter_temperature` + REST bridge route (`POST /api/.../set_inverter_temperature`).
* **CLI** — `giv-sim simulate --inverter-temperature <°C>` (omit for the
  thermal model).

Not wired as a Modbus write: IR 41 (`ge_ir_inverter_temperature`) is
input/read-only on real hardware, so there is no HR write route (consistent
with the ARM/DSP firmware overrides). `PlantStateDto` now also exposes
`inverter_temperature_celsius` + `inverter_temperature_override`. 5 new
`sim-core` unit tests.

**0.17.4** — Schedule enable/register persistence fix (issue #2) plus the
validated accumulated WIP release. HR 96/59 now toggle execution without
clearing or implicitly re-enabling charge/discharge slots; exact raw `u16`
time values survive projection and persistence. Schedule write handling is
centralized in `sim-models`. Also includes atomic persistence/export writes,
HTTP request hardening, partial HR 318-320 reconciliation, shared inverter
capability tables, Modbus range-overflow validation, simulation accounting
corrections, and refreshed Playwright coverage.

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

### Battery protocol: LV packs vs HV cluster
There are **two distinct** battery wire protocols. Which one a client uses is decided
by the inverter DTC (family 4 = three-phase, 8 = All-in-One → HV; families 2/3 → LV).
The simulator serves **both** unconditionally; the client only probes the path that
matches the inverter's DTC.

**LV BMS protocol** (single packs, e.g. Gen1/Gen2/Gen3 hybrids):
- Slave `0x32`–`0x37`: one slave per battery module, IR 60–119 each.
- `project_battery_bms(battery, idx)` — 16 cells, validity via SOC.

**HV cluster protocol** (GIV-BAT-HV modular stacks, ThreePhase/AllInOne):
Discovered via a 3-step chain (matches `givenergy-modbus` client.py and giv_tcp
commands.refresh_plant_data):
1. Slave `0xA0` (BMS), IR(60,5) → IR(61) = number of BCUs.
2. Slave `0x70+i` (BCU), IR(60,60) → cluster data; IR(64) = modules; validity via
   `pack_software_version` (IR 60–63) decoding to a non-blank string (gateway_version
   converter).
3. Slave `0x50+m` (BMU), IR(60,60) → per-module 24 cells + serial; validity via
   `serial_number` (IR 114–118) decoding to a non-blank string.

**Per-module SoC is NOT exposed on the HV wire.** BMU data is cells (IR 60–83, milli V)
+ temperatures (IR 90–113, deci °C) + serial (IR 114–118) only — confirmed against both
`givenergy-modbus` (model/hv_bcu.py `Bmu`) and giv_tcp (model/hvbmu.py, read.py). SoC is
**cluster-wide only**, packed at BCU IR(80) as `duint8`: high byte = `battery_soc_max`,
low byte = `battery_soc_min` across the stack. `project_battery_bmu` correctly omits
SoC; `project_battery_bcu` emits IR(80) as the sole SoC signal.

Single-stack model: **1 BMS → 1 BCU → N BMUs** (N = `batteries.len()`), matching the
GIV-BAT-HV datasheet systems (1–6 × GIV-BAT-3.4-HV). A 5-module stack (GIV-BAT-17.0-HV)
needs 1 BCU + 5 BMU = **6 IR(60,60) reads per cycle**. Projectors:
`project_battery_bms_discovery`, `project_battery_bcu`, `project_battery_bmu`.

If battery data comes back empty for an HV inverter, the cluster path (not the LV
`0x32` path) is what needs serving — this was the original gap.

### State sync pattern
`PlantState.battery` (singular) is a convenience field for `batteries[0]`.
Setting `state.battery` directly requires calling `state.sync_vec_from_battery()`.
Setting `state.batteries[i]` directly requires calling `state.sync_battery_from_vec()`.

### Energy totals are DAILY (midnight reset)
`PlantState.energy_totals` buckets (solar/import/export/charge/discharge/load/ac_charge)
are treated as **today** registers. `EnergyTracker` (last device in the update
order) accumulates `power × dt` each tick and **zeros every bucket at the first
tick of a new calendar day**, tracking `last_reset_date`. The first tick after
construction only records the date (no reset) so a plant restored from disk
keeps its totals.

**Plant-creation seed**: `create_plant` (Tauri) and `simulate` (CLI) call
`sim_core::seed_energy_totals_for_time_of_day(now, …)` at the moment a brand-new
plant is built, populating `energy_totals` with realistic values for the elapsed
portion of today so the IR/HR "energy today" registers don't read zero until
the engine has run long enough to climb from zero. The seeder:

- integrates the closed-form solar model from sunrise to `now`,
- integrates the piecewise-linear load profile from 00:00 to `now`,
- replays the inverter `normal_priority` rules per minute from a neutral 50% SoC,
- adds a single bookkeeping entry for the overnight grid-import that brought
  the bank from `min_soc` to 50% (omitted at exactly 00:00).

Callers must construct the `EnergyTracker` with
`EnergyTracker::new().with_last_reset_date(now.date())` so the engine doesn't
clobber the seed on its first tick (it takes the same-day no-op arm instead
of the first-tick record arm). The midnight rollover continues to work normally.

Do **not** seed at any other call site: `run_scenario`, `replay_recording`,
`serve_config`, and `EnergyTracker::update` itself must leave totals alone.
`EnergyTotals::non_zero_test_fixture()` / `seed_for_testing_if_zero()` are
**test-only** helpers and must never be called from runtime paths.

Note: `IR 11-12` (single-phase `ge_ir_pv_total`) and `IR 1374-1375`
(three-phase `tph_ir_e_pv_total`) are **lifetime** solar registers. They
read from `energy_totals.solar_lifetime_kwh`, which is **never** reset at
midnight and is seeded to `SOLAR_LIFETIME_BASELINE_KWH` (12,345 kWh) at
plant creation. Other "total"/"lifetime"-suffixed registers (`IR 21-22`,
`IR 29`, the gateway aggregation bank `IR 1641-1655`) still reuse the daily
buckets and follow the midnight reset — converting those to true lifetime
is a follow-up change.

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

`InverterState.temperature_override: Option<f64>` works the same way for the
inverter thermal model: `Some(t)` pins `temperature_celsius` (clamped to
[-10, 80] °C) and skips the heat/cool integration; `None` restores it.
Command: `SetInverterTemperature`. GUI sidebar "Inv Temp" row + CLI
`--inverter-temperature`. Not Modbus-writable (IR 41 is read-only).

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
| Hybrid4600 | 0x2002 | default¹ | default¹ | — |
| Hybrid3600 | 0x2003 | default¹ | default¹ | — |
| Polar5kW | 0x2101 | default¹ | default¹ | — |
| Polar4600 / Gen3Hybrid10kW² | 0x2102 | 10000W | 10000W | — |
| Polar3600 | 0x2103 | default¹ | default¹ | — |
| Polar6kW | 0x2104 | default¹ | default¹ | — |
| Polar7kW | 0x2105 | default¹ | default¹ | — |
| Gen3Hybrid8kW / Polar8kW² | 0x2106 | 8000W | 8000W | — |
| Gen3Plus6kW | 0x2201 | 5000W | 2600W | 452 |
| Gen3Plus4600 | 0x2202 | 4600W | 2600W | 452 |
| Gen3Plus3600 | 0x2203 | 3600W | 2600W | 452 |
| Gen3Plus6kW2 | 0x2204 | 6000W | 2600W | 452 |
| Gen3Plus7kW | 0x2205 | default¹ | default¹ | — |
| Gen3Plus8kW | 0x2206 | default¹ | default¹ | — |
| PVInverter5kW | 0x2301 | default¹ | N/A | — |
| PVInverter4600 | 0x2302 | default¹ | N/A | — |
| PVInverter3600 | 0x2303 | default¹ | N/A | — |
| PVInverter6kW | 0x2304 | default¹ | N/A | — |
| ACCoupled | 0x3001 | 3000W | 3000W | — |
| ACCoupled2 | 0x3002 | 3000W | 3000W | — |
| ThreePhase | 0x4001 | 6000W | 6000W | — |
| ThreePhase8kW | 0x4002 | 8000W | 8000W | — |
| ThreePhase10kW | 0x4003 | 10000W | 10000W | — |
| ThreePhase11kW | 0x4004 | 11000W | 11000W | — |
| AIOCommercial | 0x4101 | default¹ | default¹ | — |
| EMS | 0x5001 | default¹ | default¹ | — |
| EMSCommercial | 0x5101 | default¹ | default¹ | — |
| ACThreePhase | 0x6001 | default¹ | default¹ | — |
| Gateway12kW | 0x7001 | 6000W | 6000W | — |
| AllInOne6 | 0x8001 | 6000W | 6000W | — |
| AllInOne | 0x8002 | 6000W | 6000W | — |
| AllInOne5 | 0x8003 | 5000W | 5000W | — |
| AIO6kW | 0x8101 | default¹ | default¹ | — |
| AIO8kW | 0x8102 | 8000W | 8000W | — |
| AIO10kW | 0x8103 | 10000W | 10000W | — |
| AIOHybrid6kW | 0x8201 | 6000W | 6000W | — |
| AIOHybrid8kW | 0x8202 | 8000W | 8000W | — |
| AIOHybrid10kW | 0x8203 | 10000W | 10000W | — |
| AIOHybrid12kW | 0x8204 | default¹ | default¹ | — |
| Gen4Hybrid6kW | 0x8304 | default¹ | default¹ | — |

¹ Falls back to `_ =>` default: 5000W AC, 3600W battery.
² Shared DTC — GUI lists one entry, register projection accepts both.

Dropdown and INVERTER_PRESETS are ordered by DTC hex value ascending.

### SolarEngine reads weather from PlantState.weather (string)
Weather is stored as a display string ("Clear", "PartlyCloudy", etc.), not as an enum field.
Set `state.weather = "Overcast".to_string()` to change weather.

### Schedule slots use HHMM encoding, disabled = 60
Charge/discharge slot registers use HHMM format (e.g. 1600 = 16:00, 630 = 06:30).
Value 60 is the "disabled" sentinel (minutes > 59 is invalid).

### Timed Discharge = battery PAUSE window (HR 318-320), wraps midnight
HR 318-320 is the **battery pause** register set (givenergy-modbus
`battery_pause_mode` + single `battery_pause_slot_1` start/end). It is **one**
slot — which is exactly why the portal gives "Timed Discharge" a single window
while Charge/Export get up to 10.

- **HR 318 mode** (0-3): `0`=Disabled, `1`=PauseCharge, `2`=PauseDischarge,
  `3`=PauseBoth (GivTCP `GivLUT.battery_pause_mode`).
- **HR 319/320** = pause-window start/end (HHMM, valid 0-2359).

The portal implements "Timed Discharge HH:MM-HH:MM" as `mode=2` with the pause
window set to the **complement/inverse** of the slot, which **wraps midnight**
(`start > end`). E.g. "Timed Discharge 03:00-04:00" is written as
`mode=2, start=400 (04:00), end=300 (03:00)` → pause everywhere *except*
03:00-04:00, so the battery only discharges in that window.

`BatteryEngine::update` must therefore honour both window shapes:
- `start < end` (normal): pause when `hour ∈ [start, end)`
- `start > end` (wrap-around): pause when `hour >= start || hour < end`
  (i.e. `[start, 24:00) ∪ [00:00, end)`)
- `start == end`: disabled / empty window (covers our `60` sentinel and
  givenergy-modbus's `0`). givenergy-modbus's valid range is `(0, 2359)` and
  it uses `0` to disable; our internal PlantState defaults use `60`.

Modbus writes to HR 318/319/320 are reconciled into one `SetBatteryPause`
command that preserves whichever fields weren't written this cycle (Tauri:
`commands.rs` write loop; CLI: `enqueue_pause_slot_update`). A lone HR 318 write
must NOT clobber the start/end window.

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

### Gateway device simulation (single-AIO projection model)
The Gateway (`Gateway12kW`, DTC `0x7001`) is an AC aggregation / backup-transfer hub,
NOT an inverter. It is simulated as a **projection mode**: the existing `PlantState`
models the child AIO's physics and `RegisterStore::project_gateway_bank()` derives
gateway registers from the same state. Detection: `GW`-prefixed serial (HR 13-17).
Serves IR 1600–1859 aggregation bank (V1 firmware variant `GA000009`). Key invariants:
- **Firmware variant V1**: IR(1603)=9, uint32 totals hi-reg-first, AIO serials at IR 1831+.
- **`p_load` excludes EV charger** — household-only, EVC tracked separately.
- **Battery power sign** follows GE wire convention (+ = discharging).
- Single-AIO topology: `parallel_aio_num = 1`, AIO2/AIO3 stay zero.

Full authoritative map at `docs/gateway-register-reference.md`.

### Register map
Input (IR 0-59) and Holding (HR 0-320) register definitions are in the source:
`crates/sim-registers/src/register_defs.rs`. Key points:
- IR 52 (battery power) and IR 51 (battery current) are **negated** per GE convention.
- HR 0 holds device type (DTC). HR 35-40 = system time. HR 56-57 = discharge slot 1, HR 94-95 = charge slot 1 (all HHMM, 60 = disabled).
- Gateway aggregation at IR 1600–1859 (served only for Gateway12kW).
- Simulator-internal registers at HR 100-705 (inverter, battery, PV, grid, energy, config, schedules).

### Grid port max power output (per-family register)
The wire register that carries the inverter's grid-port max power output
**depends on the inverter family** — there is no single register that
covers every model. The classification lives in
`crates/sim-tauri/src/commands.rs::GridPortPowerFamily::from_inverter_type`
and is mirrored in `ui/index.html::gridPortPowerFamily` so the GUI label,
register hint, and Set-button enabled-state all follow the inverter type
without an extra round trip.

| Inverter family | Wire register | Encoding | Writable? | Notes |
|---|---|---|---|---|
| Single-phase / AC-coupled / Gen1-4 / PV / Polar / Gen3+ / AIO / AIOHybrid | HR 26 (`ge_hr_grid_port_max_power_output`) | `C.uint16` (raw = watts, no scaling) | **Read-only** | givenergy-modbus defines no setter; clients can read but not write. Simulator mirrors `state.config.max_ac_watts`. |
| Three-phase / HV / ACThreePhase | HR 1063 (`p_export_limit`) | `C.deci` (raw = watts × 10, clamped to u16) | Yes | givenergy-modbus `max=6500`. The simulator divides the raw value by 10 when ingesting a write and multiplies by 10 on the way out, so the user-facing unit is always watts. |
| EMS / EmsCommercial / Gateway12kW | HR 2071 (`ems_export_power_limit`) | `C.uint16` (raw = watts, no scaling) | Yes | givenergy-modbus does not set a max — full 16-bit range (0–65535 W) is valid. The schedule engine previously wrote this register from `schedule.export_power_limit_w`; it now mirrors the live `state.inverter.export_limit_w` so user edits take effect immediately. The schedule still drives the value during export windows via `SetExportLimit`. |

All three registers project from `state.inverter.export_limit_w` (or, for
HR 26, `state.config.max_ac_watts`). When the schedule's export window is
active, `sim_core::ScheduleEngine` copies `schedule.export_power_limit_w`
into `state.inverter.export_limit_w` per tick — the same scalar the GUI
displays and writes back, so a set-then-tick round trip is consistent.

The `/api/invoke/{get,set}_grid_port_max_power` Tauri commands wrap the
GUI side of this. Modbus client writes are routed via
`sim-tauri::commands::modbus_address_to_command` (and the `sim-api`
equivalent) — HR 102 → `SetExportLimit(value)`,
HR 1063 → `SetExportLimit(value / 10.0)`, HR 2071 → `SetExportLimit(value)`.

### Inverter fault registers (bit conventions)
Named faults project to different registers by inverter family, using bit tables from
givenergy-modbus (`_inverter_fault_code` / `_inverter_fault_code2`) and giv_tcp.

**Single-phase** (Gen1/2/3 Hybrid, Polar, Gen3Plus, AC-coupled, PV, EMS, AIO, Gateway child):
register **HR(223)–HR(224)** (`inverter_errors`/`inverter_fault_messages`). IR(39)–IR(40)
mirrors as raw hex (no name decoder). Key fault bits:
| Fault | HR word bit | Decodes to |
|-------|-------------|------------|
| `grid_loss` | 7 | "No Utility" |
| `inverter_trip` | 23 | "Consistent Fault" |
| `battery_over_temp` | 0 | "Inverter NTC Fault" |
| `comm_timeout` | 24 | "ARM Comms Fault" |
| `sensor_drift` | 30 | *(reserved — non-zero, no name)* |

Auxiliary: inverter_trip → IR 0 status = 3 (Fault); grid_loss → IR 49 system mode = 1 (off-grid);
battery_over_temp → IR 57 charger_warning_code = 1.

**Three-phase** (`ThreePhase*`, `ACThreePhase`): **IR(1300)–IR(1307)**, eight 16-bit words.
HR(223-224) stays 0. Key bits:
| Fault | Word (IR) | bit | Decodes to |
|-------|-----------|-----|------------|
| `grid_loss` | 1301 | 0 | "No Grid connection" |
| `inverter_trip` | 1305 | 4 | "Relay fault" |
| `battery_over_temp` | 1307 | 9 | "Battery over temperature" |
| `comm_timeout` | 1301 | 15 | "Gateway Comm fault" |
| `sensor_drift` | 1305 | 13 | "NTC open" |

## Running Tests

```bash
# Full suite (399 tests)
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
Battery sizes: `BATTERY_SIZES = [2.6, 2.6, 3.4, 5.2, 6.8, 7.0, 8.2, 9.5, 10.2, 12.8, 13.6, 16.0, 17.0, 19.0, 20.4]` (nearest-value matching). Up to 6 battery modules supported (LV packs at slave 0x32–0x37, or HV stacks).

## Network ports
| Port | Protocol | Purpose |
|------|----------|---------|
| 8899 | GivEnergy proprietary Modbus TCP (with envelope) | Inverter + battery + grid registers |
| 5020 | Standard Modbus TCP (no envelope) | GivEVC wallbox (HR 0-114, configurable in UI or via `GIVSIM_EVC_PORT`) |
| 1420 | HTTP | Tauri dev server (UI, webview only) |
| 8001 | HTTP | Browser GUI — same frontend + REST bridge to all IPC commands (`GIVSIM_WEB_PORT` to override) |

### Dongle heartbeat
The simulator acts as the dongle. It sends heartbeat requests (func 0x01, 8-byte
frame `59 59 00 01 00 02 01 01`) every 3 minutes per TCP connection. The client
must echo the frame back. After 3 unanswered heartbeats the connection is closed.

### EVC port
Port 5020 is the default (non-privileged). The real GivEVC hardware
uses port 502 (which requires root or `CAP_NET_BIND_SERVICE` on Linux).
Set the port in the UI's EVC card, or set `GIVSIM_EVC_PORT=502` env var.

## Slot maps (per `givenergy-modbus` reference)
| Inverter class | Charge slots (start,end) | Discharge slots (start,end) |
|----------------|--------------------------|------------------------------|
| GEN1/GEN2 (2-slot) | (94,95), (31,32) | (56,57), (44,45) |
| AC-coupled (basic 1-slot) | (94,95) | (56,57) |
| EXTENDED/Gen3 (10-slot) | (94,95), **(243,244)**, (246,247), (249,250), (252,253), (255,256), (258,259), (261,262), (264,265), (267,268) | (56,57), (44,45), (276,277), ..., (297,298) |
| THREE_PHASE | (1113,1114), (1115,1116), (246,247), ..., (267,268) | (1118,1119), (1120,1121), (276,277), ..., (297,298) |
| EMS | (2053,2054), (2056,2057), (2059,2060) | (2044,2045), (2047,2048), (2050,2051) |
Target SOC register follows each slot's end register (e.g. HR 248 for charge slot 3).

### Gen3 charge slot 2 register quirk
Gen3 firmware (ARM FW century 3+) stores charge slot 2 at HR 243-244, NOT HR 31-32.
HR 31-32 on Gen3 contains stale/garbage data. The `uses_gen3_extended_slots()` helper
gates this: Gen1Hybrid/Gen2Hybrid → HR 31-32; all others → HR 243-244.
The `project_schedule_for` method writes to the correct address based on inverter type.

## Battery control logic (from `giv_tcp` reference)
Charge/discharge is gated by slot enable registers (HR 96 for charge, HR 59 for discharge)
and the slot timer registers (start/end in HHMM, 60=disabled). `giv_tcp`'s `setChargeSlot`
and `setDischargeSlot` write the start/end pair together in one function. `setEnableCharge`
writes 0 (AC coupling) or 1 (normal) — the actual semantics depend on inverter wiring mode.
Force charge is enabled by writing `battery_force_discharge_enable` / `battery_charge_enable`
in baseinverter.py.
