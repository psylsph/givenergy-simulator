# Tauri IPC Contracts

## Status
✅ All 10 commands and 4 events implemented in `crates/sim-tauri/`.

## Commands

| Command | Parameters | Returns | Description |
|---|---|---|---|
| `create_plant` | `{ battery_count?, peak_watts?, latitude?, load_profile?, tick_interval? }` | `PlantStateDto` | Initializes simulation engine with config |
| `load_scenario` | `{ path: string }` | `ScenarioInfo` | Parses YAML scenario and returns event list |
| `start_simulation` | `{ speed?, scenario_path? }` | `()` | Runs background tick loop at given speed (ticks/sec) |
| `pause_simulation` | — | `()` | Stops the background tick loop |
| `inject_fault` | `{ fault_id: string }` | `()` | Enqueues fault injection, emits `fault_triggered` |
| `clear_fault` | `{ fault_id: string }` | `()` | Enqueues fault clearance |
| `set_mode` | `{ mode: string }` | `()` | Sets inverter mode (Normal/Eco/ForceCharge/ForceDischarge/ExportLimit) |
| `set_weather` | `{ weather: string }` | `()` | Sets weather (Clear/PartlyCloudy/Overcast/Storm) |
| `export_recording` | `{ path: string, format: string }` | `string` | Saves recording as csv/jsonl/json, emits `recording_saved` |
| `get_current_state` | — | `PlantStateDto` | Returns latest snapshot |

## Events

| Event | Payload | Description |
|---|---|---|
| `state_changed` | `PlantStateDto` | Emitted on every tick during simulation |
| `fault_triggered` | `string` | Fault ID that was injected |
| `scenario_completed` | `()` | All scenario events have been applied |
| `recording_saved` | `string` | Path to saved recording file |

## PlantStateDto (frontend-friendly)

```typescript
interface PlantStateDto {
  timestamp: string;
  inverter_mode: string;
  inverter_ac_power_w: number;
  aggregate_soc: number;
  battery_power_kw: number;
  battery_temperature_celsius: number;
  battery_module_count: number;
  solar_generation_w: number;
  load_demand_w: number;
  grid_power_w: number;
  grid_connected: boolean;
  active_faults: string[];
  weather: string;
  energy_totals: {
    grid_import_kwh: number;
    grid_export_kwh: number;
    battery_charge_kwh: number;
    battery_discharge_kwh: number;
    solar_generation_kwh: number;
    load_consumption_kwh: number;
  };
}
```

## Architecture

The Tauri app uses `lib.rs` as the entry point with a separate `commands` module
(necessary to avoid a Tauri proc-macro namespace collision on rustc 1.95+).

```
main.rs → sim_tauri_lib::run()
  → tauri::Builder
    → AppState (shared state: engine, register_store, recording, running, schedule)
    → 10 IPC command handlers
    → 4 event emitters
```

Device order: [ScheduleEngine] → SolarEngine → LoadEngine → InverterEngine → FaultEngine → BatteryEngine → EnergyTracker
