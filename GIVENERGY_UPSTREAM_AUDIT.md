# GivEnergy upstream compatibility audit — UPDATE 2026-06-03

**Local project:** `/home/stuart/repos/givenergy-simulator`  
**giv_tcp:** commit `b6a3ba8`  
**givenergy-modbus:** commit `c81780b`  
**Test suite:** 216/216 passed

## Status Summary

The previous audit documented 19 findings. **All 19 have been fixed** in the working tree.
In addition, several new gaps were found and fixed as part of this update.

---

## PREVIOUS FINDINGS — ALL FIXED

### Priority 1 — Functional Compatibility Bugs

| # | Issue | Status |
|---|-------|--------|
| 1 | Tauri schedule writes do not enqueue `Command::SetSchedule` | **FIXED** — both Tauri drain loops now enqueue `SetSchedule` |
| 2 | Extended Gen3/TPH/EMS registers missing from catalogue | **FIXED** — all addresses have RegisterDef entries |
| 3 | GivTCP charge slot 2 writes HR 243/244, simulator uses HR 31/32 | **FIXED** — dual write/read aliases |
| 4 | Three-phase force/enable flags absent | **FIXED** — HR 1112, 1122, 1123 catalogued |

### Priority 2 — Dead-Letter Controls

| # | Issue | Status |
|---|-------|--------|
| 5 | Accepted writable registers overwritten by projection | **FIXED** — all project from PlantState fields |
| 6 | HR 29 calibration not routed | **FIXED** — routes to StartCalibration/CancelCalibration |
| 7 | HR 111/112 limits not modelled | **FIXED** — stored in state, projected, commanded, AND enforced in BatteryEngine |
| 8 | HR 163 reboot no side effect | **FIXED** — routes to Command::InverterReboot |

### Priority 3 — Protocol Compatibility

| # | Issue | Status |
|---|-------|--------|
| 9 | Response padding wrong | **FIXED** — 0x8A success, 0x12 error |
| 10 | Error payload shape wrong | **FIXED** — proper inner PDU format |
| 11 | Response serial semantics wrong | **FIXED** — echoes request serial |
| 12 | Read count limit 125 vs 60 | **FIXED** — count > 60 rejected |
| 13 | BMS slave range 0x37 | **FIXED** — constrained to 0x33-0x36 |
| 14 | FC 0x16 / heartbeat unsupported | **FIXED** — both added to server |

### Priority 4 / Addendum

| # | Issue | Status |
|---|-------|--------|
| 15 | `run_scenario` schedule accumulator missing | **FIXED** — added ScheduleEngine + accumulator + `apply_schedule_updates` helper |
| 16 | TPH HR 1109 projected as charge target SOC | **FIXED** — projected from HR 110 reserve |
| 17 | IR 35 misnamed as consumption | **FIXED** — renamed to `ac_charge_today` |
| 18 | Gen1Hybrid DTC 0x1001 wrong | **FIXED** — now 0x2001 with firmware 100 |
| 19 | Missing IR 6-7, 11-12, 21-22, 32-33, 44-46 | **FIXED** — all present and projecting |

---

## NEW FINDINGS — FIXED IN THIS UPDATE

| # | Issue | Fix |
|---|-------|-----|
| 20 | `enable_charge_target` not enforced by ScheduleEngine | Now gates the global charge target: when false, charge to 100% regardless of target |
| 21 | Charge/discharge limits (HR 111/112) not enforced in BatteryEngine | Now applies percentage caps (0-50 range, 50 = full power) to per-module power |
| 22 | Battery pause mode/slot not checked by BatteryEngine | When pause_mode=1 and inside pause slot, battery power forced to zero |
| 23 | `serve_config` passes empty battery state to Modbus server | Now creates a shared `Arc<Mutex<Vec<BatteryState>>>` and updates it each tick |
| 24 | HR 319/320 pause slot writes not routed (Tauri + CLI) | Both drain loops now accumulate pause slot registers and enqueue `SetBatteryPause` preserving unmodified fields |
| 25 | HR 114 projected from wrong field | Now reads from `battery_discharge_min_power_reserve` (new PlantState field) |
| 26 | HR 166, 311, 317 missing from catalogue and projection | Added catalogue entries + projection for all three |
| 27 | HR 166 already existed as `ge_hr_rtc_enable` but had no projection | Added projection entry matching existing catalogue name |
| 28 | `SetActivePowerRate` routing in sim-api + Tauri | Already existed — verified correct |

## REMAINING GAPS (not fixed — scope deliberate)

| Severity | Issue | Reason Skipped |
|----------|-------|----------------|
| MEDIUM | Extended charge/discharge slots 3-10 (HR 246-299) | Requires Schedule model expansion across 5+ files |
| MEDIUM | Full single-phase IR bank (IR 3, 4, 10, 14, 15, 16, etc.) | ~25 missing IRs, cosmetic — clients work with existing subset |
| MEDIUM | Three-phase IR 1000-1413 bank | ~145 registers; feature addition, not a bug fix |
| MEDIUM | Gateway IR 1600-1859 bank | ~103 registers; feature addition |
| MEDIUM | EMS IR 2040-2094 bank | ~54 registers; feature addition |
| LOW | BMS IR 90-95, 101-106, 115-119 gaps | Populated from defaults, not from distinct state fields |
| LOW | Newer DTC variants (2200+, 8200+, 8304) not in dropdown | Frontend + backend update needed |
| LOW | Smart Load slots HR 554-573 | Not polled by any upstream client |
