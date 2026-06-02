# Battery Algorithms

## SOC Formula (per module)
```
delta_soc = (effective_power_kw * dt_hours) / capacity_kwh * 100
effective_power_kw = power_kw * charge_efficiency      (charging)
effective_power_kw = power_kw / discharge_efficiency    (discharging)

Clamped to [min_soc, max_soc] range.
```

## Efficiency
- `charge_efficiency`: fraction of input power stored (default 0.95)
- `discharge_efficiency`: fraction of stored energy reaching inverter (default 0.95)
- Losses manifest as heat (fed into thermal model)

## Constraints
- Min SOC / Max SOC bounds (clamped every tick)
- Max charge rate / Max discharge rate (enforced by InverterEngine)

## Thermal Model
Tracks battery temperature based on ambient, losses, and cooling.

### Temperature Update (per module, per tick)
```
ambient = 25 + 8 * sin(2π * (hour - 5) / 24)    // time-of-day sinusoidal
heat_rise = |power_kw - effective_power_kw| * thermal_resistance * dt_hours
cooling = cooling_rate * (temp - ambient) * dt_hours
temp += heat_rise - cooling
clamped to [-10, 70]°C
```

### Parameters
| Parameter | Default | Description |
|---|---|---|
| `ambient_temp_celsius` | 25.0 | Base ambient temperature |
| `thermal_resistance` | 5.0 | °C per kW of loss |
| `cooling_rate` | 2.0 | °C/hour per °C above ambient |

### Thermal Derating
| Temperature | Effect |
|---|---|
| < 45°C | Normal operation |
| 45–55°C | Linear power reduction (100% → 0%) |
| ≥ 55°C | Charging/discharging blocked |

## Aging Model
Tracks cumulative throughput and degrades capacity.

### Throughput & Cycle Counting (per module, per tick)
```
energy_this_tick = |power_kw| * dt_hours
throughput_kwh += energy_this_tick
cycle_count = throughput_kwh / nominal_capacity_kwh
```

### Capacity Degradation
```
soh_loss = degradation_per_cycle * (energy_this_tick / nominal_capacity_kwh)
soh = max(soh - soh_loss, min_soh)
capacity_kwh = nominal_capacity_kwh * soh
```

### Parameters
| Parameter | Default | Description |
|---|---|---|
| `degradation_per_cycle` | 0.0002 | ~0.02% per cycle → 80% SOH at ~1000 cycles |
| `min_soh` | 0.5 | Battery is "dead" below this threshold |

## Cell Balancing
Multiple battery modules with identical power but different capacities naturally diverge in SOC over time. This is inherent in the per-module SOC formula — no explicit balancing algorithm is needed for simulation fidelity.

### Multi-Battery Helpers
- `aggregate_soc()` — capacity-weighted average
- `total_battery_capacity()` — sum of all modules
- `distribute_battery_power(total_kw)` — equal split across modules
- `total_battery_power_kw()` — net bank power
