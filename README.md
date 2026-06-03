# GivEnergy Plant Simulator

![License](https://img.shields.io/badge/license-MIT-blue.svg)

> 🙏 **Huge thanks to the open-source reverse-engineering efforts that made this possible:**  
> [**GivTCP**](https://github.com/GivEnergy/giv_tcp) — the original GivEnergy Modbus integration for Home Assistant  
> [**givenergy-modbus**](https://github.com/dewet22/givenergy-modbus) — detailed register map, protocol reference, and Python library

<div align="center">

<a href="https://www.buymeacoffee.com/psylsph" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-blue.png" alt="Buy Me a Coffee" style="height: 60px !important;width: 217px !important;" ></a>

</div>

<script type="text/javascript" src="https://cdnjs.buymeacoffee.com/1.0.0/button.prod.min.js" data-name="bmc-button" data-slug="psylsph" data-color="#5F7FFF" data-emoji=""  data-font="Cookie" data-text="Buy me a coffee" data-outline-color="#000000" data-font-color="#ffffff" data-coffee-color="#FFDD00" ></script>


A digital twin of a GivEnergy solar PV + battery storage system. Model your Gen3 Hybrid or AC Coupled inverter with up to 3 battery modules, realistic solar generation, household load profiles, and Modbus integration — all running locally on your machine.


## What It Does

- **Simulates a full GivEnergy plant** — inverter, batteries, solar PV, grid connection, and household load
- **Real-time dashboard** — energy flow diagram, battery SOC gauge, power timeline, cumulative kWh totals
- **Hardware-accurate Modbus protocol** — speaks the proprietary GivEnergy data-adapter framing, compatible with real client apps
- **Multiple inverter types** — Gen3 Hybrid (5 kW DC) and AC Coupled (3 kW DC) with correct power limits
- **Up to 3 battery modules** — each with independent SOC, SOH, capacity, and thermal behaviour
- **5 inverter modes** — Normal, Eco, Force Charge, Force Discharge, Export Limit
- **Timed schedules** — charge/discharge windows with SOC targets, midnight-wrapping support
- **Manual overrides** — pin solar generation or load demand to fixed wattages for testing
- **Fault injection** — simulate grid loss, inverter trips, battery over-temperature
- **Weather simulation** — Clear, Partly Cloudy, Overcast, Rain, Storm irradiance profiles
- **Save and restore** — persist plant state to disk, reload with all settings intact
- **Headless CLI** — run scripted scenarios with assertions for automated testing

## Quick Start — Desktop GUI

```bash
# Install frontend dependencies (first time only)
cd ui && npm install && cd ..

# Launch the desktop app
cd crates/sim-tauri && cargo tauri dev
```

Click **Create Plant** to start with defaults: 1 battery, 5 kW solar, family load profile, clear weather.

> **Prerequisites:** `cargo install tauri-cli` and on Linux: `sudo apt install libwebkit2gtk-4.1-dev`

## Quick Start — Headless CLI

```bash
# Run a built-in scenario
cargo run --bin sim-api -- run examples/basic_day.yaml

# With Modbus server (connect your GivEnergy client)
cargo run --bin sim-api -- run examples/basic_day.yaml --modbus 127.0.0.1:5020

# Multi-battery
cargo run --bin sim-api -- run examples/basic_day.yaml --battery-count 3

# Export results (JSON Lines, CSV, JUnit XML, JSON report)
cargo run --bin sim-api -- run examples/grid_outage.yaml --output /tmp/results
```

## How It Works

### Architecture Overview

```
┌─────────────────────────────────────────────────────────┐
│  Tauri GUI (sim-tauri) / Headless CLI (sim-api)        │
├─────────────────────────────────────────────────────────┤
│  → Scenario Parser (sim-scenarios)                      │
│  → SimulationEngine (sim-core)                          │
│      → Solar → Load → Inverter → Faults → Battery → ET  │
│  → RegisterStore (sim-registers)                        │
│  → Modbus TCP Server (sim-modbus)                       │
│  → Recording (sim-recording, sim-storage)               │
└─────────────────────────────────────────────────────────┘
```

Full architecture diagram: [`docs/architecture-diagram.md`](docs/architecture-diagram.md)

### Simulation Engine

The engine runs a deterministic tick loop. Each tick advances the simulation clock and processes all device models in a fixed order:

```
Schedule → Solar → Load → Inverter → Faults → Battery → Energy Tracker
```

Device models are pluggable — each implements the `DeviceModel` trait and receives mutable access to the shared `PlantState`.

### Battery Model

Each battery module tracks:

- **SOC** (state of charge) — percentage, clamped to configurable min/max
- **SOH** (state of health) — degrades with charge cycles, reduces effective capacity
- **Temperature** — rises during charge/discharge, passive cooling towards ambient, derates above 45°C, shuts down above 55°C
- **Efficiency** — configurable charge/discharge efficiency (default 95%)
- **Power limits** — C-rate based, capped by inverter DC power rating

Power is distributed evenly across modules. The inverter's `max_ac_watts` caps the total charge or discharge rate regardless of battery capability.

### Inverter Modes

| Mode | Behaviour |
|------|-----------|
| **Normal** | Solar powers load first, excess charges battery, remainder exports to grid. Deficit draws from battery, then grid. |
| **Eco** | Same as Normal — solar excess charges battery before export. |
| **Force Charge** | Grid charges battery at max rate until target SOC. Solar surplus assists. |
| **Force Discharge** | Battery discharges at max rate to grid/load. |
| **Export Limit** | Like Normal but caps grid export at a configurable wattage. |

### Modbus Protocol

The simulator implements the GivEnergy proprietary Modbus framing — not standard Modbus TCP. The Wi-Fi dongle wraps all frames in an envelope with transaction ID `0x5959`, a 10-byte inverter serial, and inner CRC-16. This means real GivEnergy monitoring apps can connect directly.

Register map covers:

- **Input registers** (0–59): live readings — PV voltage/current/power, grid power, battery SOC/voltage/current/temperature, energy totals
- **Holding registers** (0–320): configuration — inverter mode, charge/discharge slots, SOC limits, battery pause mode
- **Internal registers** (100–705): extended simulator state — per-module battery details, PV parameters, grid stats, energy totals, schedule config

### Scenario DSL

Write YAML test scenarios with timed events and assertions:

```yaml
name: basic day
06:00:
  load: 800
12:00:
  expect:
    soc_gt: 70
    solar_gt: 3000
18:00:
  load: 3500
  expect:
    soc_lt: 80
```

Supported assertions: `soc_gt`, `soc_lt`, `grid_connected`, `solar_gt`, `solar_lt`, `grid_import_gt`, `grid_export_gt`, `battery_charging`, `no_faults`, `fault_active`, `solar_kwh_gt`, `grid_export_kwh_gt`, `grid_import_kwh_gt`, `load_kwh_gt`.

Multi-day scenarios with `days: N` — events repeat daily with date offsets.

## Load Profiles

| Profile | Description |
|---------|-------------|
| **Minimal** | Low baseline ~300W |
| **Family** | Morning peak, afternoon dip, evening peak ~3 kW |
| **EV** | Family + overnight EV charging |
| **HeatPump** | Family + steady heat pump load |
| **Custom** | Define your own `(hour, watts)` points |

## Battery Module Configuration

Select from standard GivEnergy module sizes: 2.6, 5.2, 7.0, 8.2, 9.5, 12.8, 16.0, 19.0 kWh. Each module's SOH slider adjusts effective capacity — a 9.5 kWh module at 80% SOH behaves as 7.6 kWh.

## Project Structure

```
crates/
  sim-models/     — DeviceModel trait, PlantState, all sub-state types
  sim-core/       — SimulationEngine, Command enum, device model implementations
  sim-registers/  — RegisterDef catalogue, RegisterStore, state-to-register projection
  sim-modbus/     — GivEnergy proprietary Modbus TCP server
  sim-scenarios/  — YAML DSL parser with assertion checking
  sim-faults/     — Fault definitions and FaultEngine
  sim-recording/  — JSON Lines recording, CSV, JUnit XML, JSON report export
  sim-storage/    — File I/O for recordings
  sim-api/        — Headless CLI binary
  sim-tauri/      — Tauri v2 desktop GUI
ui/               — Web frontend (Vite + vanilla JS)
```

## Building and Testing

```bash
# Run the full test suite
cargo test

# Build everything
cargo build

# Run a single test
cargo test -p sim-core -- battery_balancing
```

## Design Documents

Full design docs live in [`docs/`](docs/) — architecture, state model, register strategy, Modbus protocol, IPC contracts, engine designs, and roadmap.

## License

MIT

<div align="center">

<a href="https://www.buymeacoffee.com/psylsph" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-blue.png" alt="Buy Me a Coffee" style="height: 60px !important;width: 217px !important;" ></a>

</div>
