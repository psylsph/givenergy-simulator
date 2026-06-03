# GivEnergy Plant Simulator

![License](https://img.shields.io/badge/license-MIT-blue.svg)

> 🙏 **Huge thanks to the open-source reverse-engineering efforts that made this possible:**  
> [**GivTCP**](https://github.com/GivEnergy/giv_tcp) — the original GivEnergy Modbus integration for Home Assistant  
> [**givenergy-modbus**](https://github.com/dewet22/givenergy-modbus) — detailed register map, protocol reference, and Python library  

<div align="center">

<a href="https://www.buymeacoffee.com/psylsph" target="_blank"><img src="https://cdn.buymeacoffee.com/buttons/v2/default-blue.png" alt="Buy Me a Coffee" style="height: 60px !important;width: 217px !important;" ></a>

</div>

A digital twin of a GivEnergy solar PV + battery storage system. Model your Gen3 Hybrid or AC Coupled inverter with up to 3 battery modules, realistic solar generation, household load profiles, and Modbus integration — all running locally on your machine.

## Screenshot

![GivEnergy Plant Simulator Dashboard](docs/screenshot.png)

| # | Area |
|---|------|
| 1 | Inverter type selector |
| 2 | Battery module config (capacity + SOH slider) |
| 3 | Solar peak + optional PV2 peak |
| 4 | Load profile |
| 5 | Weather + **Create Plant** button |
| 6 | Simulation (Pause / Reset / Tick speed) |
| 7 | Inverter mode + Weather controls |
| 8 | Solar / Load overrides + export limit |
| 9 | **Fault Injection** buttons (Grid Loss, Inverter Trip, Battery Over-Temp) |
| 10 | Scenario loader + Save/Load persistence |
| A | Energy flow diagram — live power arrows between Solar, Battery, Grid, Load |
| B | Battery SOC gauges — per-module cards with voltage, current, temp, capacity, SoH |
| C | Active Faults display + Schedule card |
| D | Power timeline — scrolling 4-trace chart |
| E | Cumulative kWh totals — import, export, solar, consumption, charge, discharge |

---

## Quick Start

### Desktop GUI

```bash
# Install frontend dependencies (first time only)
cd ui && npm install && cd ..

# Launch the desktop app
cd crates/sim-tauri && cargo tauri dev
```

> **Prerequisites:** `cargo install tauri-cli` and on Linux: `sudo apt install libwebkit2gtk-4.1-dev`

### Headless CLI

```bash
# Run a built-in scenario
cargo run --bin sim-api -- run examples/basic_day.yaml

# With Modbus server (connect your GivEnergy client)
cargo run --bin sim-api -- run examples/basic_day.yaml --modbus 127.0.0.1:5020

# Multi-battery, export results
cargo run --bin sim-api -- run examples/basic_day.yaml --battery-count 3 --output /tmp/results
```

---

## GUI — Step by Step

### 1. Creating a Plant

Click **Create Plant** on the setup screen. The defaults (1 battery, 5 kW solar, family load, clear weather, Gen3 Hybrid) give you a working system out of the box.

If you want to customise before starting:

**Inverter Type**
Choose from 12 supported models — Gen 1 Hybrid, Gen3 Hybrid (5/8/10 kW), AC Coupled (Mk1/Mk2), Three Phase, All-in-One (5/6/8/10 kW). Each has correct AC output and battery charge/discharge limits from official GivEnergy datasheets. The dropdown is ordered by device type code (DTC) hex value.

**Battery Modules**
Pick 1 to 3 modules. For each module:
- **Capacity** — select from standard GivEnergy sizes: 2.6, 5.2, 7.0, 8.2, 9.5, 12.8, 16.0, 19.0 kWh.
- **SOH (State of Health)** — drag the slider from 50% to 100%. This reduces effective capacity: a 9.5 kWh battery at 80% SOH behaves as 7.6 kWh. The nominal (nameplate) capacity is still shown for reference.

**Solar Peak**
Total wattage of your PV array. If you set a PV2 peak wattage, the simulator splits generation across two arrays (45% PV1 / 55% PV2) and shows them independently.

**Load Profile**
Select a household consumption pattern:
- **Minimal** — low baseline around 300W
- **Family** — morning peak, afternoon dip, evening peak around 3 kW
- **EV** — Family profile plus overnight EV charging
- **HeatPump** — Family plus a steady heat pump load
- **Custom** — define your own (hour, watts) points

**Weather**
Choose Clear, Partly Cloudy, Overcast, Rain, or Storm. This directly controls solar irradiance — you can change it mid-simulation to see how your system responds to a cloudy afternoon.

**PV2 Peak**
Optional. Set to 0 (default) for a single PV array, or enter peak watts for a second array. Both arrays share the solar peak you set above using the 45/55 split.

Click **Create Plant** once all settings are configured. The simulation starts immediately.

### 2. Dashboard

The main panel has four areas:

**Energy Flow Diagram**
Shows live power flowing between Solar, Battery, Grid, and Load. Arrows move in the direction of power flow and display the current wattage. If solar exceeds load, the excess flows to the battery (or grid when the battery is full). If load exceeds solar, the deficit comes from the battery or grid.

**Battery Modules Panel**
One card per battery module, showing:
- **SOC %** — state of charge, colour-coded green (>50%), yellow (20–50%), red (<20%)
- **SOC Gauge** — horizontal bar, live updating
- **Set SOC** — drag the range slider to manually set the battery charge level. The label updates as you drag.
- **Power** — current charge/discharge rate in kW, shown as "Charging", "Discharging", or "Idle"
- **Voltage, Current, Temp** — live readings per module
- **Capacity** — effective / nominal kWh (e.g. "7.6 / 9.5 kWh"). Effective is reduced by SOH.
- **SoH** — current state of health percentage
- **Cycles** — accumulated charge/discharge cycle count

**Power Timeline**
A scrolling line chart with four traces: Solar (yellow), Load (orange), Battery (green when charging, red when discharging), Grid (blue when importing, red when exporting).

**Cumulative kWh Cards**
Five cards showing totals since the simulation started:
- Solar generated
- Load consumed
- Grid imported / exported
- Battery charged / discharged

### 3. Sidebar Controls

| Control | What it does |
|---------|-------------|
| **Pause / Resume** | Freeze or unfreeze the simulation clock. Battery temperature continues to drift while paused (passive cooling). |
| **Reset** | Stop the simulation and return to the setup screen. |
| **Tick speed** | How fast simulated time advances. E.g. set to 60 and 1 real second = 1 simulation minute. |
| **Inverter mode** | Switch operating mode mid-simulation. See "Inverter Modes" below for what each does. |
| **Weather** | Change conditions on the fly. Solar output adjusts immediately — try switching to Rain mid-day. |
| **Solar override** | Pin solar generation to a fixed wattage. Enter 0 to disable solar entirely. Works at night too — the override bypasses the normal solar calculation entirely. Clear the field to restore automatic generation. |
| **Load override** | Pin household demand to a fixed wattage. Enter 0 for no load. Clear to restore the load profile. |
| **Export limit** | Cap how much power you send to the grid. Used with Export Limit mode, but also caps export in Normal/Eco. |

### 4. Schedules

Set timed charge or discharge windows that automatically switch the inverter mode:

1. Toggle **Enable charge** or **Enable discharge**.
2. Set a **Start** and **End** time in HHMM format (e.g. 200 for 02:00, 530 for 05:30, 1430 for 14:30).
3. Set a **Target SOC** — for charge, the battery charges until it reaches this percentage. For discharge, it discharges down to this percentage.
4. When the clock enters the window, the inverter switches to Force Charge or Force Discharge automatically. When the window ends, it returns to the previously selected mode.

### 5. Fault Injection

Test how the system handles failures without damaging real hardware:

- **Inject Grid Loss** — the grid disconnects. No import or export possible. Solar powers load and battery only; excess is curtailed.
- **Inject Inverter Trip** — the inverter shuts down completely. All power flow stops.
- **Inject Battery Over-Temp** — forces battery temperature above the safe threshold. The battery derates (limited output) above 45°C and shuts down above 55°C.

Click **Clear** next to each fault button to resolve it. The system recovers automatically.

### 6. Save and Load

- **Save** — writes the full plant state (all settings, SOC, SOH, energy totals, schedule config) to `~/.local/share/com.givenergy.simulator/plant_state.json`.
- **Load** — restores a previously saved plant. All sidebar settings (inverter type, battery config, overrides, sliders) are restored to their saved values.

> Tip: after a save, you can close the app and reopen — your plant will be exactly where you left it.

### 7. Connecting a Modbus Client

The simulator speaks the real GivEnergy Modbus protocol — not standard Modbus TCP. Any app that connects to a GivEnergy Wi-Fi dongle can connect to the simulator instead.

1. Start the CLI with the Modbus server enabled:
   ```bash
   cargo run --bin sim-api -- run examples/basic_day.yaml --modbus 127.0.0.1:5020
   ```
2. Point your GivEnergy client app at `127.0.0.1:5020`.
3. The client reads live registers (SOC, power, voltage, energy totals) and writes configuration (mode, schedules, SOC limits) just like a real inverter.

Protocol details:
- **Read Input Registers** (fn 0x04, slave 0x32) — live readings
- **Read Holding Registers** (fn 0x03, slave 0x32) — configuration
- **Write Single Register** (fn 0x06, slave 0x11) — write commands that dispatch to the simulation engine
- **Battery BMS reads** (slave 0x33–0x37) — per-module battery details for multi-battery setups

---

## CLI — Scenarios and Testing

### Running Scenarios

Scenarios are YAML files that describe a day (or multiple days) of simulated time with timed events and assertions:

```yaml
name: basic day
date: 2025-06-21
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

Each time entry can set `load`, `solar`, `mode`, `weather`, `fault`, `clear_fault`, `export_limit`, and/or `expect` assertions. All times are HH:MM.

### Available Assertions

```
soc_gt / soc_lt          Battery SOC above/below a percentage
solar_gt / solar_lt      Solar generation above/below watts
grid_connected           Grid connected (1) or disconnected (0)
grid_import_gt           Grid import above watts
grid_export_gt           Grid export above watts
battery_charging         Battery is charging (true/false)
no_faults                No active faults
fault_active <name>      Named fault is active
solar_kwh_gt             Cumulative solar generation above kWh
grid_export_kwh_gt       Cumulative grid export above kWh
grid_import_kwh_gt       Cumulative grid import above kWh
load_kwh_gt              Cumulative load consumption above kWh
```

If any assertion fails, the CLI exits with code 1 (CI-friendly).

### Multi-Day Scenarios

Add `days: N` to repeat events daily. Events fire at the same time each day; the simulation clock advances through each day in sequence.

### Output Formats

With `--output <dir>`:

- **JSON Lines** — one frame per tick (`recording.jsonl`)
- **CSV** — energy trace (`trace.csv`)
- **JUnit XML** — assertion results for CI (`report.xml`)
- **JSON report** — summary with per-assertion pass/fail (`report.json`)

### CLI Reference

```
giv-sim run <scenario.yaml> [options]

  --date YYYY-MM-DD       Simulation start date (default: today)
  --battery-count N       Number of battery modules, 1-3 (default: 1)
  --modbus ADDR:PORT      Start Modbus TCP server for client connections
  --output DIR            Write output files to directory
  --tick-interval MS      Real-time tick interval in milliseconds (default: 100)
```

---

## Inverter Types

All 12 supported inverter types with correct power limits from official datasheets:

| Inverter | DTC | AC Max | Battery Limit |
|----------|-----|--------|---------------|
| Gen 1 Hybrid | 0x1001 | 5,000W | 2,500W |
| Gen3 Hybrid | 0x2001 | 5,000W | 3,600W |
| Gen3 Hybrid 8kW | 0x2101 | 8,000W | 8,000W |
| Gen3 Hybrid 10kW | 0x2102 | 10,000W | 10,000W |
| AC Coupled | 0x3001 | 3,000W | 3,000W |
| AC Coupled Mk2 | 0x3002 | 3,000W | 3,000W |
| Three Phase | 0x4001 | 6,000W | 6,000W |
| All-in-One 6kW | 0x8001 | 6,000W | 6,000W |
| All-in-One | 0x8002 | 6,000W | 6,000W |
| All-in-One 5kW | 0x8003 | 5,000W | 5,000W |
| AIO 8kW | 0x8102 | 8,000W | 8,000W |
| AIO 10kW | 0x8103 | 10,000W | 10,000W |

Battery charge and discharge is capped by both the battery C-rate and the inverter's battery limit — whichever is lower.

### Inverter Modes

| Mode | What it does |
|------|-------------|
| **Normal** | Solar powers the house first. Excess charges the battery. Leftover exports to grid. If solar < load, battery discharges, then grid imports. |
| **Eco** | Same as Normal — solar excess charges battery before export. |
| **Force Charge** | Forces the battery to charge at maximum rate (from solar plus grid) until the target SOC is reached. Use with a schedule to charge overnight at cheap rates. |
| **Force Discharge** | Forces the battery to discharge at maximum rate to supply load and export to grid until the reserve is reached. |
| **Export Limit** | Like Normal but caps grid export at the configured wattage (see Export Limit control). Excess solar that can't be exported is curtailed. |

---

## Load Profiles

| Profile | Pattern |
|---------|---------|
| **Minimal** | Low baseline ~300W, good for holidays or empty house |
| **Family** | Morning peak (breakfast), afternoon dip, evening peak (dinner/TV) ~3 kW |
| **EV** | Family plus overnight EV charging block (22:00–06:00) |
| **HeatPump** | Family plus steady heat pump load through winter months |
| **Custom** | Define your own hourly wattage points for testing specific scenarios |

---

## Battery Module Configuration

Each module tracks these internal properties:

- **SOC** (state of charge) — percentage, clamped to configurable min/max
- **SOH** (state of health) — degrades with charge cycles, reduces effective capacity. Set at creation via slider; further reduced by cycling.
- **Temperature** — rises during charge/discharge, passive cooling towards ambient. Derates output above 45°C, shuts down above 55°C.
- **C-rate** — charge/discharge rate relative to capacity. Default max is 0.3C (e.g. 30% of capacity per hour).

Power is distributed evenly across modules. The inverter's battery power limit caps the total regardless of how much battery headroom exists.

---

## Architecture

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

The engine runs a deterministic tick loop. Each tick advances the simulation clock and processes all device models in a fixed order:

```
Schedule → Solar → Load → Inverter → Faults → Battery → Energy Tracker
```

### Modbus Protocol

The simulator implements the GivEnergy proprietary Modbus framing — not standard Modbus TCP. The Wi-Fi dongle wraps all frames in an envelope with transaction ID `0x5959`, a 10-byte inverter serial, and inner CRC-16. This means real GivEnergy monitoring apps can connect directly.

Register map covers:

- **Input registers** (0–59) — live readings: PV voltage/current/power, grid power, battery SOC/voltage/current/temperature, energy totals
- **Holding registers** (0–320) — configuration: inverter mode, charge/discharge slots, SOC limits, battery pause mode
- **Internal registers** (100–705) — extended simulator state: per-module battery details, PV parameters, grid stats, energy totals, schedule config

---

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
# Full test suite (211 tests)
cargo test

# Build all crates (sim-tauri excluded — needs GTK deps)
cargo build --workspace --exclude sim-tauri

# Run a single test
cargo test -p sim-core -- battery_balancing

# Lint
cargo fmt --all -- --check
cargo clippy --all-targets --workspace --exclude sim-tauri
```

## Design Documents

Full design docs live in [`docs/`](docs/) — architecture, state model, register strategy, Modbus protocol, IPC contracts, engine designs, and roadmap.

## Credits

This project would not exist without the pioneering reverse-engineering work of the GivEnergy open-source community.

- **[GivTCP](https://github.com/GivEnergy/giv_tcp)** — The original GivEnergy Modbus integration for Home Assistant. This project established the core Modbus protocol mapping, register addresses, and write methodology that this app builds on.

- **[givenergy-modbus](https://github.com/dewet22/givenergy-modbus)** — The definitive Python reference library for the GivEnergy Modbus protocol. Its detailed register map, frame format documentation, and working reference implementation were invaluable.

Both projects are open-source and available on GitHub. If you find this app useful, consider giving them a star too ⭐

## License

MIT
