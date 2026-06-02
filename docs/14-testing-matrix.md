# Testing Matrix

## Current Coverage: 82 tests

### Unit Tests (sim-core: 42)
- SolarEngine: night, midday, weather, winter
- LoadEngine: family evening peak, minimal low, custom interpolation (3), empty
- BatteryEngine: charge, discharge, SOC clamping (min/max), efficiency (charge/discharge)
- InverterEngine: solar→battery, deficit→grid, eco slow charge, eco night
- Integration: full day, tick advance
- Weather command
- EnergyTracker: accumulation, grid import
- Thermal model: charge heating, derating
- Aging model: throughput tracking, SOH degradation
- Cell balancing: multi-module SOC divergence
- Inverter temperature: load heating, night cooling

### Combinatorial Tests (sim-core: 6)
- Normal mode × 1 battery
- Normal mode × 2 batteries
- Eco mode × 3 batteries
- ForceCharge + grid_loss fault
- ExportLimit × 2 batteries
- ForceDischarge × 1/2/3 batteries (parametric)

### Register Tests (sim-registers: 6)
- State-to-register projection
- Access control (readwrite/readonly)
- Snapshot completeness
- Inverter mode encoding
- Negative grid power
- Multi-battery SOC registers

### Modbus Tests (sim-modbus: 4)
- Read Holding Registers
- Write Single Register (accepted)
- Write ReadOnly register (rejected)
- Unsupported function code

### Scenario Tests (sim-scenarios: 11)
- YAML parsing (basic, named, mode command)
- Assertion checking (SOC >, SOC <, battery charging, no faults, fault active, grid export, energy totals)
- Multi-day parsing

### Fault Engine Tests (sim-faults: 3)
- Grid loss disconnection
- Inverter trip (zeros output)
- Battery over-temp (blocks charging, allows discharging)
- Grid restore (clears on fault removal)

### Recording Tests (sim-recording: 1)
- JSONL round-trip

### Storage Tests (sim-storage: 1)
- File round-trip

### API Integration (sim-api: 9)
- Full-day cycle with register projection
- Battery capacity limits (single, 3-module)
- Max charge rate limits
- ForceCharge mode
- Over-discharge protection

## Regression Scenarios (5)
| Scenario | Tests |
|---|---|
| `basic_day.yaml` | SOC assertions at 10:00 and 22:00 |
| `grid_outage.yaml` | Grid disconnection and restoration |
| `force_charge.yaml` | Force charge from grid raises SOC |
| `weather_change.yaml` | Weather change affects solar output |
| `two_day_clear.yaml` | Multi-day with energy assertions |

## Matrix Coverage
| Dimension | Values Covered |
|---|---|
| Inverter mode | Normal (all combos), Eco (3 batt), ForceCharge (+fault), ForceDischarge (1/2/3), ExportLimit (2 batt) |
| Battery count | 1, 2, 3 |
| Fault condition | Grid loss, inverter trip, battery over-temp, fault clearance |
| Load profile | Family, Minimal, Custom (3 variants), EV, HeatPump (indirect via scenarios) |
| Weather | Clear, Overcast, Storm |
