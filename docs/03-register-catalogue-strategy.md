# Register Catalogue Strategy

## Categories (7)
| Category | Address Range | Example Registers |
|---|---|---|
| Inverter | 100–119 | mode, ac_power, export_limit, temperature, firmware_state |
| Battery | 200–239 | soc (×3 modules), power, voltage, current, temperature, capacity, rates, efficiency, module_count |
| PV | 300–319 | generation, voltage, current, energy_today, peak_capacity |
| Grid | 400–419 | power, voltage, frequency, connected, load_power |
| Energy Totals | 500–519 | grid_import/export, battery_charge/discharge, solar_gen, load_cons |
| Configuration | 600–619 | battery_count, tick_interval, weather |
| Schedules | 700–719 | charge/discharge start/end, target SOCs |

## Register Definition
```rust
pub struct RegisterDef {
    pub address: u16,
    pub name: String,
    pub category: RegisterCategory,
    pub typ: RegisterType,     // U16, S16, U32, S32, F32
    pub scaling_factor: f64,   // multiplier to convert raw → engineering units
    pub access: Access,        // ReadOnly | ReadWrite
}
```

## State-to-Register Projection
`RegisterStore::project_from_state()` maps `PlantState` fields → register values:

1. Each match arm returns an `f64` **engineering value**
2. The scaling factor is applied: `raw_u16 = engineering_value / scaling_factor`
3. Enum-like registers (mode, weather) and signed values (power, current) use direct insertion

Example: battery temperature (scaling_factor=0.1, engineering=37.5°C → raw=375)

## Writable Registers
Register writes (via Modbus fn 0x06) are validated against `Access::ReadWrite`:
- **100** (inverter_mode) → `Command::SetInverterMode`
- **102** (export_limit) → `Command::SetExportLimit`
- **210** (min_soc) → `Command::SetMinSoc`
- **211** (max_soc) → `Command::SetMaxSoc`
- **602** (weather) → `Command::SetWeather`

All other registers are ReadOnly. Write attempts return Modbus exception 0x02.

## Current Catalogue
45 registers total. Full list in `crates/sim-registers/src/lib.rs::default_register_catalogue()`.
