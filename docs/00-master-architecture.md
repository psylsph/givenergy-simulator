# Master Architecture

## Vision
A hardware-faithful digital twin of a GivEnergy installation.

## Status
All 4 phases complete. 82 unit tests, 5 regression scenarios. 10 workspace crates.

## Architectural Principles
1. **Simulation state is authoritative** — PlantState is the single source of truth.
2. **Register banks are projections of state** — Modbus registers are read from PlantState each tick.
3. **Deterministic execution** — Same inputs always produce identical outputs.
4. **Headless and GUI modes share identical core logic** — CLI (`sim-api`) and Tauri GUI (`sim-tauri`) use the same `SimulationEngine`.
5. **Device models are pluggable** — Each device implements `DeviceModel: Send`, called in registration order.

## Runtime Layers
```
Tauri GUI (sim-tauri) / Headless CLI (sim-api)
    → Scenario Parser (sim-scenarios)
    → SimulationEngine (sim-core)
        → Device Models (SolarEngine, LoadEngine, InverterEngine, FaultEngine, BatteryEngine, EnergyTracker, ScheduleEngine)
    → RegisterStore (sim-registers)
    → Modbus TCP Server (sim-modbus)
    → Recording (sim-recording, sim-storage)
```

## Major Components
- **PlantState** — Simulation state with `PlantConfig`, `EnergyTotals`, multi-battery support
- **SolarEngine** — PV generation with latitude/day-of-year/weather model
- **LoadEngine** — Household load with 4 built-in profiles + custom time-series
- **InverterEngine** — Power-flow priority logic with 5 modes + island mode + thermal model
- **BatteryEngine** — SOC tracking, charge/discharge efficiency, thermal model, aging/degradation
- **ScheduleEngine** — Timed charge/discharge windows
- **FaultEngine** — Fault injection with grid loss, inverter trip, battery over-temp
- **EnergyTracker** — Cumulative kWh totals (grid import/export, battery charge/discharge, solar, load)
- **RegisterStore** — 45 registers across 7 categories, scaling-factor-based projection
- **ModbusServer** — TCP server with Read (0x03) and Write (0x06) support, command dispatch
- **ScenarioEngine** — YAML DSL parser with time-stamped events and assertion checking

## Device Update Order (critical)
Solar → Load → Inverter → Faults → Battery → EnergyTracker
(With ScheduleEngine inserted before Solar when schedules are active.)
