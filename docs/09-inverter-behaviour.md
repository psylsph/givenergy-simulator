# Inverter Behaviour

## Operating Modes
| Mode | Description |
|---|---|
| `Normal` | Solar → Load → Battery → Grid priority |
| `Eco` | Like Normal but caps daytime charging at 50% to preserve evening reserve |
| `ForceCharge` | Grid charges battery to target SOC at max rate |
| `ForceDischarge` | Battery discharges to grid at max rate |
| `ExportLimit` | Like Normal but caps grid export at `export_limit_w` |

## Priority Logic
```
Solar → Load → Battery → Grid
```

### Normal Mode
1. Solar supplies load first
2. Excess solar charges battery (up to max_charge_kw × module count)
3. Remaining surplus exported to grid
4. Deficit covered by battery discharge (up to max_discharge_kw)
5. Any remaining deficit imported from grid

### Eco Mode
Same as Normal but during 10:00–16:00 the charge rate is halved (50% of max), sending more excess to grid instead.

### ForceCharge Mode
Grid imports power to charge battery at max rate. Solar still covers load first.

### ForceDischarge Mode
Battery discharges to grid at max rate. Solar still covers load first.

### ExportLimit Mode
Normal priority, then export is capped. Surplus beyond the limit is curtailed (inverter output reduced).

### Island Mode (Grid Disconnected)
No grid interaction. Solar → Load → Battery only. Excess solar is curtailed.

## Inverter Temperature Model
```
heat = (ac_power_w / 1000) * 0.03 * thermal_resistance * dt_hours
cooling = cooling_coefficient * (temp - ambient) * dt_hours
temp += heat - cooling
clamped to [-10, 80]°C
```

Parameters: thermal_resistance=20°C/kW, cooling_coefficient=0.5, ambient=25°C

## Schedule Engine
Timed charge/discharge windows that override the inverter mode:

- **Charge window**: If `hour` is within `[charge_start, charge_end)` and `soc < charge_target_soc`, mode → ForceCharge
- **Discharge window**: If `hour` is within `[discharge_start, discharge_end)` and `soc > discharge_target_soc`, mode → ForceDischarge
- Windows wrap midnight (e.g. 22:00–06:00 works)
- Windows with start==end are disabled
- Must be registered BEFORE InverterEngine in device list
