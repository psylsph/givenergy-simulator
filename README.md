# GivEnergy Simulator

A hardware-faithful digital twin of a GivEnergy solar + battery installation, written in Rust.

## Architecture

Simulation state is authoritative. Register banks are projections of state. Execution is deterministic. Headless and GUI modes share identical core logic. Device models are pluggable.

```
UI / CLI → Application Services → Simulation Core → Device Models → Register Mapper → Protocol Adapters
```

## Crates

| Crate | Description |
|---|---|
| `sim-models` | `DeviceModel` trait and `TickContext` — the plugin interface |
| `sim-core` | `PlantState`, `Command` enum, `SimulationEngine` tick scheduler |
| `sim-registers` | Register catalogue, `RegisterStore` with projection from state |
| `sim-faults` | Fault categories, triggers, and well-known fault definitions |
| `sim-scenarios` | YAML scenario DSL parser with assertion checking |
| `sim-recording` | JSON Lines recording format for replay and diffing |
| `sim-modbus` | Modbus TCP server (Read Holding Registers, fn 0x03) |
| `sim-storage` | File-based recording persistence |
| `sim-api` | Headless CLI binary |

## Quick Start

```bash
# Build everything
cargo build

# Run tests (23 tests)
cargo test

# Run a scenario
cargo run --bin sim-api -- run examples/basic_day.yaml

# Run with options
cargo run --bin sim-api -- run examples/basic_day.yaml \
  --peak-watts 5000 --latitude 51.5 --profile family --weather clear

# Run with CI outputs (JSONL, CSV, JUnit XML, JSON report)
cargo run --bin sim-api -- run examples/grid_outage.yaml --output /tmp/results

# Run with Modbus server
cargo run --bin sim-api -- run examples/basic_day.yaml --modbus 127.0.0.1:5020

# All example scenarios
cargo run --bin sim-api -- run examples/basic_day.yaml        # clear summer day
cargo run --bin sim-api -- run examples/grid_outage.yaml       # grid fault & restore
cargo run --bin sim-api -- run examples/force_charge.yaml      # force charge from grid
```

## Scenario DSL

Scenarios are YAML files that describe time-stamped events and assertions:

```yaml
08:00:
  solar: 3500
09:00:
  fault: grid_loss
10:00:
  expect:
    soc_gt: 50
18:00:
  load: 3500
  expect:
    soc_lt: 80
```

Supported assertions: `soc_gt`, `soc_lt`, `grid_connected`.

## Design Documents

All design documents live in [`docs/`](docs/):

- [Master Architecture](docs/00-master-architecture.md)
- [Rust Crate Design](docs/01-rust-crate-design.md)
- [State Model](docs/02-state-model.md)
- [Register Catalogue Strategy](docs/03-register-catalogue-strategy.md)
- [Modbus Emulation](docs/04-modbus-emulation.md)
- [Tauri IPC Contracts](docs/05-tauri-ipc-contracts.md)
- [Solar Engine](docs/06-solar-engine.md)
- [Load Engine](docs/07-load-engine.md)
- [Battery Algorithms](docs/08-battery-algorithms.md)
- [Inverter Behaviour](docs/09-inverter-behaviour.md)
- [Scenario DSL](docs/10-scenario-dsl.md)
- [Recording Format](docs/11-recording-format.md)
- [Fault Framework](docs/12-fault-framework.md)
- [Headless CI Design](docs/13-headless-ci-design.md)
- [Testing Matrix](docs/14-testing-matrix.md)
- [Backlog](docs/15-backlog.md)
- [Sequence Diagrams](docs/16-sequence-diagrams.md)
- [Engineering Roadmap](docs/17-roadmap.md)

## Roadmap

| Phase | Scope | Timeline |
|---|---|---|
| 1 | MVP scaffold, core types, tick loop, CLI | 4–6 weeks |
| 2 | Real device models (solar PV, battery SOC, inverter priority, load profiles) | 8–10 weeks |
| 3 | Hardware-accurate Modbus registers, timing, and scaling | 12–16 weeks |
| 4 | Full digital twin with Tauri UI | Ongoing |

## License

MIT
