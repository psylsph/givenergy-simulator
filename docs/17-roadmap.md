# Engineering Roadmap — Complete

All four phases have been implemented.

| Phase | Scope | Status | Key Deliverables |
|---|---|---|---|
| **1** | MVP scaffold, core types, tick loop, CLI | ✅ Complete | PlantState, DeviceModel trait, SimulationEngine, CLI binary, RegisterStore |
| **2** | Functional simulator with real device models | ✅ Complete | SolarEngine, LoadEngine (4 profiles + custom), InverterEngine (5 modes), BatteryEngine (SOC + thermal + aging), FaultEngine, ScheduleEngine |
| **3** | Hardware-accurate Modbus registers, energy totals | ✅ Complete | 45-register catalogue, Modbus write support (fn 0x06), scaling-factor-based projection, EnergyTracker, PlantConfig |
| **4** | Full digital twin with Tauri GUI | ✅ Complete | Tauri v2 desktop app, 10 IPC commands, 4 events, real-time dashboard, scenario playback, CSV/JSONL export |

## Metrics
| Metric | Value |
|---|---|
| Workspace crates | 10 |
| Unit tests | 82 |
| Regression scenarios | 5 |
| Registers | 45 across 7 categories |
| Registers (writable) | 5 (mode, export_limit, min/max SOC, weather) |
| Fault types | 5 well-known |
| Load profiles | 4 built-in + custom time-series |
| GUI commands | 10 |
| GUI events | 4 |
| Modbus function codes | 2 (0x03 read, 0x06 write) |

## Repository
```
crates/
  sim-models      — types, trait, state
  sim-core        — engine, device models
  sim-registers   — register catalogue & projection
  sim-modbus      — TCP server
  sim-scenarios   — DSL parser & assertions
  sim-faults      — fault definitions & engine
  sim-recording   — output formats
  sim-storage     — file I/O
  sim-api         — headless CLI
  sim-tauri       — desktop GUI
examples/         — 5 scenario YAMLs
examples/profiles/— custom load profile YAMLs
scripts/          — CI regression runner
.github/          — GitHub Actions CI
ui/               — web frontend (Tauri)
```
