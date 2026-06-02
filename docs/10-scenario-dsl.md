# Scenario DSL

Scenarios are YAML files describing time-stamped events and assertions.
Used by the CLI (`giv-sim run`) and Tauri GUI.

## Format
```yaml
name: Scenario Name         # optional, defaults to "unnamed"
days: 2                     # optional, defaults to 1

HH:MM:                      # event time (24-hour)
  solar: <watts>            # override PV generation
  load: <watts>             # override household demand
  fault: <fault_id>         # inject a fault
  clear_fault: <fault_id>   # clear a fault
  mode: <mode>              # set inverter mode
  export_limit: <watts>     # set export limit
  weather: <condition>      # change weather
  expect:                   # assertion block (see below)
    soc_gt: <percent>
```

## Supported Event Fields
| Field | Type | Description |
|---|---|---|
| `solar` | float | Override PV generation (watts) |
| `load` | float | Override household demand (watts) |
| `fault` | string | Fault ID to inject |
| `clear_fault` | string | Fault ID to clear |
| `mode` | string | Inverter mode: Normal, Eco, ForceCharge, ForceDischarge, ExportLimit |
| `export_limit` | float | Export limit in watts |
| `weather` | string | Clear, PartlyCloudy, Overcast, Storm |
| `expect` | map | Assertion conditions to check after event |

## Assertions
| Assertion | Type | Description |
|---|---|---|
| `soc_gt` | float | Aggregate SOC must be greater than this |
| `soc_lt` | float | Aggregate SOC must be less than this |
| `solar_gt` | float | PV generation must be greater than this |
| `solar_lt` | float | PV generation must be less than this |
| `grid_connected` | 0/1 | Grid connection state |
| `grid_import_gt` | float | Grid import power must be greater than this |
| `grid_export_gt` | float | Grid export power must be greater than this |
| `battery_charging` | bool | Battery bank must be charging/discharging |
| `no_faults` | bool | No active faults |
| `fault_active` | string | Specific fault must be active |
| `solar_kwh_gt` | float | Cumulative solar generation must be greater than this |
| `grid_import_kwh_gt` | float | Cumulative grid import must be greater than this |
| `grid_export_kwh_gt` | float | Cumulative grid export must be greater than this |
| `load_kwh_gt` | float | Cumulative load consumption must be greater than this |

## Multi-day Scenarios
Set `days: N` (default 1). Events repeat daily with a date offset.
Day labels in assertion output: `"HH:MM (day N)"`.

## Example: Two-Day Scenario
```yaml
name: two day clear
days: 2
08:00:
  solar: 3500
12:00:
  expect:
    soc_gt: 70
22:00:
  expect:
    soc_lt: 50
    solar_kwh_gt: 20
```

## Complete Example Files
| File | Description |
|---|---|
| `examples/basic_day.yaml` | Clear summer day with family load |
| `examples/grid_outage.yaml` | Grid fault at 09:00, restore at 14:00 |
| `examples/force_charge.yaml` | Force charge from grid at midnight |
| `examples/weather_change.yaml` | Weather changes mid-day |
| `examples/two_day_clear.yaml` | Multi-day with energy assertions |
