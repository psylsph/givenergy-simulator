# Load Engine

## Built-in Profiles
| Profile | Description | Day Peak | Night Base |
|---|---|---|---|
| `Minimal` | Low baseline | ~200-400W | ~150W |
| `Family` | Typical home | ~3000W (evening) | ~250W |
| `EV` | +EV charging | ~3000W + 4.5kW EV | ~4.5kW overnight |
| `HeatPump` | +heat pump | ~4500W (evening) | ~400W |

## Custom Profiles (Time-Series)
Custom profiles loaded from YAML files. Format:
```yaml
- hour: 0.0    # decimal hour (0.0–23.9)
  watts: 2500
- hour: 6.0
  watts: 500
- hour: 12.0
  watts: 400
- hour: 22.0
  watts: 2000
```

Values are linearly interpolated between points. The profile wraps at midnight (last point → first point + 24h).

CLI usage: `--profile path/to/profile.yaml`

## LoadEngine
- One tick per update
- Reads `ctx.now` for time-of-day
- Interpolates between hourly values (built-in) or data points (custom)
- Writes `state.load.demand_w`

## Custom Interpolation Logic
- Points sorted by hour on load
- For each hour: find containing segment, linear interpolation
- Before first point: interpolate from (last_point_hour - 24) to first_point
- After last point: interpolate to (first_point_hour + 24)
