# Feature Ideas — GivEnergy Plant Simulator

## Tier 1: Quick Wins (🟢 Low effort, high value)

### 1. Battery chemistry selection
Add a `chemistry` field to `BatteryState` with configurable voltage curves, C-rates, and cycle life for Li-ion (NMC), LFP, and Lead-acid. The nominal voltage (44-52V for Li-ion vs flatter LFP curve) affects SOC-from-voltage estimation.

### 2. Battery calendar aging
Currently SOH degrades only with charge/discharge cycling. Add a time-based degradation component so a battery sitting at high SOC / high temperature loses capacity even without cycling. Well-understood Arrhenius model from battery literature.

### 3. Modbus FC 0x10 (Write Multiple Registers)
GivTCP uses FC 0x16 (Read Meter Product) but doesn't typically use FC 0x10. Adding it would be trivial (parallel to FC 0x06 Write Single) and improves protocol fidelity for any client that does use it.

### 4. PV soiling / degradation
Add a configurable derating factor to `SolarEngine` that reduces output over time (dust accumulation) or after a configurable number of days without rain.

### 5. CT clamp fault / meter misconfiguration
Extend the fault framework: a "ReversedCTClamp" or "MissingCTClamp" fault that inverts or zeroes the grid power reading — useful for testing how GivTCP/home assistant behaves with misconfigured meters.

### 6. Docker image
A single `Dockerfile` packaging `sim-api` so CI pipelines can spin up a simulator instance, run Modbus tests against it, and tear it down. Useful for givenergy-modbus/giv_tcp CI.

### 7. Battery cycle count / throughput reset
Add a "battery replacement" command that resets throughput, cycle count, and restores SOH to 1.0 — simulating what happens when you physically replace a battery module.

---

## Tier 2: Medium Impact (🟡 Medium effort)

### 8. Export limit scheduling
Time-windowed export power limits using the EMS export slot registers (HR 2062-2071). During peak grid hours (4-7pm), the simulator would curtail export to a configurable limit. Requires extending the `Schedule` model with 3 export slots and an `InverterEngine` path that caps `export_limit_w` during those windows.

### 9. AC-coupled battery logic
AC-coupled inverters (DTC 0x3001/0x3002) don't have a DC bus connection to the battery — power flows grid ↔ battery via the AC coupling. This changes the power flow calculation in `InverterEngine`: charging from grid is more natural, and the "solar charges battery" path is less direct. A separate `InverterEngineAC` implementation would make AC-coupled simulation more realistic.

### 10. Time-of-use tariff simulation
Add import/export price schedules (£/kWh) per time window. The simulator would compute daily cost/savings, making it possible to back-test whether a particular charge/discharge schedule saves money compared to another. A new `TariffEngine` could track financial totals alongside energy totals, and the UI could show cost/savings charts.

### 11. Weather API integration
Replace manual weather override with real forecast data. Pull cloud cover forecast from OpenWeatherMap or similar for the configured latitude/longitude, then use it to drive solar generation. Makes long-running simulations more realistic without manual intervention.

### 12. Scenario fuzzer
A `cargo test` that generates random scenario files (random weather, random faults, random schedules) and runs the simulation, asserting no panics, no NaN values, and sensible energy balance. Using the `proptest` crate for property-based testing would catch edge cases that hand-written tests miss.

### 13. WebSocket streaming
The Tauri frontend already receives push events via `app.emit("state_changed", dto)`. Adding a WebSocket endpoint to `sim-api` would let external tools (Grafana, custom dashboards, givenergy-modbus tests) subscribe to real-time state without polling.

### 14. EMS plant scheduling
Implement the 3-slot EMS charge/discharge/export schedule from HR 2044-2071. Currently the registers are catalogued but do nothing when written. This requires extending the `Schedule` model (or creating an `EmsSchedule`) and adding an `EmsEngine` device model.

---

## Tier 3: Major Features (🔴 High effort)

### 15. Multi-inverter parallel operation
Real installations can have 2-3 inverters sharing a battery bank and load. Modelling this requires either multiple `SimulationEngine` instances with a shared grid/battery bus, or a unified engine with per-inverter state. The register model would need per-inverter register banks (HR 2040+ for EMS aggregates). This is the single biggest architectural change possible.

### 16. Modbus TLS
GivEnergy's cloud-connected dongles use TLS-wrapped Modbus. Adding TLS support to the Modbus server would let the simulator test cloud connectivity paths, certificate validation, and encrypted protocol handling. Requires tokio-tls or rustls integration.

### 17. Full three-phase telemetry
Three-phase inverters expose 145+ input registers for per-phase V/I/P, energy totals, and fault codes (IR 1000-1413). Populating these from a three-phase engine model would make the simulator useful for testing three-phase monitoring setups. Currently the ThreePhase type returns zeros for all phase-specific registers.

### 18. Hot water diverter / solar diverter simulation
GivEnergy inverters can control a relay for diverting surplus solar to an immersion heater (HR 202-239, IR 23). Modelling this as a configurable load that activates when export exceeds a threshold would add realism for UK installations with diverter hardware.

### 19. Gateway-mode simulation ✅ (v0.16.0, Phase 1)
Gateway devices (DTC prefix `0x7xxx`) aggregate data from All-in-One inverters.
**Implemented as a single-AIO projection model** (v0.16.0): when `inverter_type`
starts with `Gateway`, the `PlantState` models the child AIO's physics and
`project_gateway_bank()` derives the IR 1600–1859 aggregation bank + `GW` serial
prefix from that state. Detection (`GW` serial), version/variant (V1
`GA000009`), work mode, AIO summary, per-AIO power/SOC/serials, energy totals,
and the EV-excluded `p_load` are all served. See
`docs/gateway-register-reference.md` for the authoritative register map.

**Remaining work (Phase 2+):**
- Multi-AIO topology (2–3 AIOs) — currently single-AIO only.
- Firmware variant **V2** (`GA000010+`): swapped `uint32` byte order + shifted
  AIO serial addresses, gated by a config knob. Currently V1-only.
- Control-write routing (charge/discharge enable, SOC target) currently behaves
  like a normal inverter; gateway-as-authoritative-control-endpoint semantics
  need explicit handling for parallel installs.
- Error-respond to unmapped gateway sub-ranges (currently returns zeros).

### 20. Frequency/Watt and Volt/VAR grid support
Modern inverters support grid support functions: frequency derating (curtail export when grid frequency > 50.2Hz) and Volt/VAR (absorb reactive power when voltage is high). Implementing these in the InverterEngine would make the simulator useful for grid-interconnection testing.
