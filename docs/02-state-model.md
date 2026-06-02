# State Model

## PlantState

The authoritative simulation state. All register banks are projections of this.

```rust
pub struct PlantState {
    pub timestamp: NaiveDateTime,
    pub inverter: InverterState,
    pub battery: BatteryState,              // convenience, always == batteries[0]
    pub batteries: Vec<BatteryState>,        // 1–3 modules
    pub solar: SolarState,
    pub load: LoadState,
    pub grid: GridState,
    pub active_faults: Vec<String>,
    pub weather: String,                     // "Clear" | "PartlyCloudy" | "Overcast" | "Storm"
    pub energy_totals: EnergyTotals,
    pub config: PlantConfig,
}
```

## Sub-State Types

### InverterState
- `mode`: Normal | Eco | ForceCharge | ForceDischarge | ExportLimit
- `ac_power_w`: AC output in watts
- `export_limit_w`: grid export cap (ExportLimit mode)
- `temperature_celsius`: inverter temperature (default 35°C, thermal model)

### BatteryState
- `soc_percent`: 0.0–100.0
- `capacity_kwh`: current capacity (degrades with cycling)
- `nominal_capacity_kwh`: original capacity
- `max_charge_kw`, `max_discharge_kw`: rate limits
- `min_soc`, `max_soc`: SOC bounds
- `power_kw`: net power (positive=charge, negative=discharge)
- `charge_efficiency`, `discharge_efficiency`: 0.95 default
- `temperature_celsius`: battery temperature (default 25°C)
- `throughput_kwh`: cumulative energy throughput
- `soh`: State of Health (1.0 = new, degrades per cycle)
- `cycle_count`: equivalent full cycles

### SolarState
- `generation_w`: current PV output in watts

### LoadState
- `demand_w`: current household consumption in watts

### GridState
- `power_w`: positive=import, negative=export
- `connected`: whether grid connection is live

### EnergyTotals
- `grid_import_kwh`, `grid_export_kwh`
- `battery_charge_kwh`, `battery_discharge_kwh`
- `solar_generation_kwh`, `load_consumption_kwh`

### PlantConfig
- `solar_peak_watts`: installed panel capacity
- `latitude`: site latitude
- `tick_interval_secs`: simulation step size

## Batch Helpers

- `aggregate_soc()` — capacity-weighted average SOC across all modules
- `total_battery_capacity()` — sum of all module capacities
- `total_max_charge_kw()`, `total_max_discharge_kw()` — aggregate rate limits
- `total_battery_power_kw()` — net power of whole bank
- `battery_temperature_celsius()` — average temperature across modules
- `sync_battery_from_vec()` — sync `battery` from `batteries[0]`
- `sync_vec_from_battery()` — sync `batteries[0]` from `battery`
- `distribute_battery_power(total_kw)` — divide power evenly across modules

## State Transitions

State transitions occur only during simulation ticks.
All external writes become `Command` variants and are queued between ticks.

### Command Enum
```rust
pub enum Command {
    SetInverterMode(InverterMode),
    SetExportLimit(f64),
    SetMinSoc(f64),
    SetMaxSoc(f64),
    InjectFault(String),
    ClearFault(String),
    SetWeather(WeatherCondition),
}
```

Commands are drained from the queue at the start of each tick, before device models run.
