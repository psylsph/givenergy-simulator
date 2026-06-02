# Backlog — Completed

All Epics from the original backlog are now complete.

## Epic 1 — Core Simulation ✅
- PlantState with all sub-state types
- Multi-battery support (1–3 modules)
- SimulationEngine with tick loop and command queue
- PlantConfig for static parameters
- Deterministic execution model

## Epic 2 — Modbus ✅
- TCP server with concurrent connections
- Function code 0x03 (Read Holding Registers)
- Function code 0x06 (Write Single Register)
- 45 registers across 7 categories
- Scaling-factor-based state projection
- Write-to-Command dispatch via mpsc channel
- 4 integration tests

## Epic 3 — UI ✅
- Tauri v2 desktop application
- 10 IPC commands (create_plant through export_recording)
- 4 events (state_changed, fault_triggered, scenario_completed, recording_saved)
- Real-time web dashboard with energy flow, SOC gauge, power timeline
- CSV/JSONL export

## Epic 4 — Scenarios ✅
- YAML DSL with time-stamped events
- 8 assertion types including energy totals
- Multi-day scenarios (days: N)
- Named scenarios with metadata
- 5 example scenarios
- E2E regression via CLI and GUI

## Epic 5 — Faults ✅
- Well-known fault catalogue (grid_loss, inverter_trip, battery_over_temp, comm_timeout, sensor_drift)
- FaultEngine device model
- Manual injection via Command
- Effects: grid disconnect, inverter zero, battery charge block
- Recovery on fault clear

## Epic 6 — Replay ✅
- Recording frames every tick
- JSON Lines, CSV, JUnit XML, JSON report output formats
- `giv-sim replay` CLI command
- Frame-by-frame diff for regression comparison
- Summary mode with energy totals

## Epic 7 — CI ✅
- `scripts/run-ci.sh` regression runner
- `.github/workflows/ci.yml` with build, test, lint, and scenario jobs
- JUnit XML output for CI integration
- 5 scenarios verified in CI pipeline

## Future Ideas (not in original scope)
- Thermal model for batteries (done)
- Aging model for batteries (done)
- Cell balancing through natural divergence (done)
- Inverter temperature model (done)
- Custom load profiles as time-series (done)
- Battery state-of-health tracking (done)
