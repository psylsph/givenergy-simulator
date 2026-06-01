# Master Architecture

## Vision
A hardware-faithful digital twin of a GivEnergy installation.

## Architectural Principles
1. Simulation state is authoritative.
2. Register banks are projections of state.
3. Deterministic execution.
4. Headless and GUI modes share identical core logic.
5. Device models are pluggable.

## Runtime Layers
UI -> Application Services -> Simulation Core -> Device Models -> Register Mapper -> Protocol Adapters

## Major Components
- PlantState
- TimeEngine
- SolarEngine
- LoadEngine
- InverterEngine
- BatteryEngine
- ScenarioEngine
- FaultEngine
- RecordingEngine
- RegisterMapper
- ModbusServer
