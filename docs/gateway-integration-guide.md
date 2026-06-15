# GivEnergy Gateway — Client Integration Guide (Topology, Connections & Register Map)

> **The one document an AI (or engineer) needs to implement Gateway display support.**
> Covers the connection model (which port, one connection vs. many, gateway-as-proxy vs.
> direct), how the Gateway links to its child AIO(s), and a register-name-by-register-name
> field table for every value you'll display.
>
> **Topology conclusions here are authoritative** — cross-checked against the GivEnergy
> AIO Parallel Connection Guide (installer doc), GivTCP v3 (the HA addon that runs against
> real parallel hardware), and the `dewet22/givenergy-modbus` client library (used by the
> givenergy-local HA integration). For pure byte-level reference see
> [`gateway-register-reference.md`](./gateway-register-reference.md).

---

## Part 1 — The connection model (read this first)

### 1.1 The single most important fact

**Each physical device has its OWN Wi-Fi/LAN dongle with its OWN IP address on TCP port 8899. You open a SEPARATE TCP connection to each device. The Gateway does NOT proxy reads to its child AIOs.**

This is the thing most people get wrong. The Gateway is an *aggregator* that reports a *summary* of the AIO(s) behind it — but it is not a Modbus gateway/router. To get per-AIO detail (cells, detailed status) you connect to each AIO at its own IP.

### 1.2 The port

| Purpose | Port | Protocol | Notes |
|---|---|---|---|
| **Every inverter/gateway/AIO dongle** | **8899** | GivEnergy proprietary Modbus TCP (envelope-wrapped) | Same for Gateway and AIO. Same envelope format. |
| GivEVC wallbox | 5020 (sim default) / 502 (real hw) | Standard Modbus TCP (no envelope) | Separate device, not relevant to gateway/AIO topology |
| Tauri dev UI (sim only) | 1420 | HTTP | Not on real hardware |

So: **`<gateway_ip>:8899`** is the Gateway, **`<aio1_ip>:8899`** is AIO #1, **`<aio2_ip>:8899`** is AIO #2. Three devices in a dual-AIO install = three TCP connections.

### 1.3 The three real topologies

```
TOPOLOGY A — Single AIO + Gateway (most common)           TOPOLOGY B — Parallel AIOs + Gateway
                                                            (2-3 AIOs, up to 18kW / 40.5kWh)

  ┌─────────────┐    AC + RJ45 comms     ┌──────────────┐     ┌─────────────┐  RJ45   ┌─────────────┐ RJ45  ┌─────────────┐
  │  Gateway    │◀──────────────────────▶│   AIO #1     │◀───▶│   AIO #1    │◀───────▶│   AIO #2    │◀─────▶│   AIO #3    │
  │  (0x7001)   │                        │  (0x8002)    │     │  (Master)   │  x2     │             │  x2   │  (optional) │
  │  GW serial  │                        │  SA serial   │     │  GW serial  │         │  SA serial  │       │  SA serial  │
  └──────┬──────┘                        └──────┬───────┘     └──────┬──────┘         └──────┬──────┘       └──────┬──────┘
         │ :8899                                 │ :8899              │ :8899                 │ :8899               │ :8899
         │                                       │                    │                       │                     │
         ▼ own dongle                            ▼ own dongle         ▼ own dongle            ▼ own dongle          ▼ own dongle
   ┌──────────┐                           ┌──────────┐         ┌──────────┐            ┌──────────┐          ┌──────────┐
   │ Client   │                           │ (only if you want   │ Client   │            │ Client   │          │ Client   │
   │ conn #1  │                           │  per-AIO detail)    │ conn #1  │            │ conn #2  │          │ conn #3  │
   └──────────┘                           └──────────┘         └──────────┘            └──────────┘          └──────────┘

TOPOLOGY C — Standalone AIO (no Gateway)
  AIO alone on :8899, no gateway bank, no GW serial. Backup/EPS not available.
```

**Key point about the parallel comms wiring (topology B):** the RJ45 daisy-chain
(Gateway→AIO1, AIO1→AIO2, AIO2→AIO3) is the *hardware control/sync bus* — it lets the
Gateway coordinate the AIOs and switch them into parallel mode. It is **not** a network
bus you read over. Each AIO still has its own independent Wi-Fi/LAN dongle on the network
with its own IP. The comms cable is installer-wiring, invisible to your Modbus client.

### 1.4 What connects to what — the connection decision tree

```
Do you only need the system-level view (grid, PV, load, aggregate battery,
energy totals, power flow)?
  └─ YES → connect to the GATEWAY only (<gw_ip>:8899).
            One Client. Read HR(0-119) + IR(1600-1859). Done.
            This covers the entire homeowner dashboard.

Do you also need per-AIO battery cell voltages / temperatures / per-module detail?
  └─ YES → connect to the GATEWAY for the aggregate view AND
           open a separate Client to each AIO (<aioN_ip>:8899).
           The AIO connection is read the same as any All-in-One: detect() + refresh().
           Each AIO appears as a normal AIO device (DTC 0x8xxx, SA serial).
```

**Practical guidance from GivTCP (which runs against real parallel hardware):**
> *"The Gateway inverter should be the only inverter that is controlled via GivTCP,
> the individual AIO inverter data treated for information only."*

Translation for your client:
- **Control writes (charge/discharge enable, SOC target, slots) → send to the GATEWAY only.** It is the authoritative control endpoint. Writes to child AIOs are at best ignored, at worst desync the parallel group.
- **Reads → Gateway always; AIOs only if you want their per-module detail.**

### 1.5 How the Gateway "links" to its child AIOs (on the wire)

The linkage is **data-level, not connection-level**. The Gateway's aggregation bank
(IR 1600–1859) contains fields that *reference* the child AIOs by serial number and
*summarise* their telemetry:

| Linkage field | Reg(s) | What it tells the client |
|---|---|---|
| `first_inverter_serial_number` | IR 1627–1631 | Serial of the primary (master) AIO |
| `aio1/2/3_serial_number` | IR 1831–1835 / 1838–1842 / 1845–1849 (V1) | Serial of each child AIO slot |
| `parallel_aio_num` | IR 1700 | How many AIOs are configured (1–3) |
| `parallel_aio_online_num` | IR 1701 | How many are currently online |
| `p_aio1/2/3_inverter` | IR 1816 / 1817 / 1818 | Per-AIO inverter power (W, signed) |
| `aio1/2/3_soc` | IR 1801 / 1802 / 1803 | Per-AIO state of charge (%) |
| `p_aio_total` | IR 1702 | Sum of all AIO inverter power |

So your client *discovers* the child AIO serials from the Gateway's bank, and if you
choose to open direct AIO connections you correlate them by serial number (the AIO's own
`HR(13–17)` serial matches the `aioN_serial_number` the Gateway reported).

> **You do not need the direct AIO connections to display the standard dashboard.** The
> Gateway already gives you per-AIO power, SOC, serial, and aggregated energy. Direct AIO
> connections are purely for cell-level battery diagnostics.

---

## Part 2 — Detecting a Gateway (vs. an AIO, vs. a hybrid)

Run this on every new connection. The **serial prefix is authoritative**.

```python
# Pseudocode — one Client, one connection, one device.
client.connect((host, 8899))
await client.detect()              # reads HR(0-59) at slave 0x11, resolves model

serial = decode_serial(client.plant, hr=13, count=5)   # 10 Latin-1 chars
dtc    = client.plant.register_caches[0x11].get(HR(0))

if serial.startswith("GW"):
    device_class = "GATEWAY"       # DTC family 0x7xxx (Gateway12kW = 0x7001)
    # NOW read the gateway aggregation bank:
    await client.refresh()         # the library's refresh() auto-reads IR(1600-1859)
                                   # when capabilities.is_gateway is True
elif serial.startswith("SA") or dtc in (0x8001,0x8002,0x8003,0x8102,0x8103,0x82xx):
    device_class = "AIO"           # All-in-One, DTC family 0x8xxx
elif serial.startswith(("CE","BE")):   # typical hybrid serials
    device_class = "HYBRID"
```

**Detection signals (all three agree on a real Gateway):**

| # | Signal | Read from | Gateway value |
|---|---|---|---|
| 1 | **Serial prefix** `GW` | HR(13–17) | e.g. `GW2423G192` — **primary classifier** |
| 2 | Device Type Code | HR(0) | `0x7001` (family `0x7xxx`) |
| 3 | Firmware version string | IR(1600–1603) | `GA000009` (V1) / `GA000010` (V2) |

**Validity:** the gateway model's `is_valid()` is `software_version is not None` — i.e.
IR(1600–1603) must decode to a non-empty string. If a non-gateway device is queried, that
block is all-zeros, the decode is empty, and the client correctly concludes "no gateway."
**Always guard display with the validity check** or you'll render phantom gateway widgets
against hybrids/AIOs.

---

## Part 3 — The full register map, by display widget

Every field below uses the exact register **name** from the reference model
(`dewet22/givenergy-modbus` `model/gateway.py`), so you can grep either codebase.
All gateway data is **Input Registers (function `0x04`)** at the gateway's slave (`0x11`
for the config/identity block, same slave for the IR 1600–1859 bank). All multi-byte is
**big-endian**.

### 3.1 Header / identity card

| Display element | Register name | Reg(s) | Type / scale | Example |
|---|---|---|---|---|
| Device name | — | HR(0) DTC | u16 | `0x7001` → "Gateway" |
| **Serial number** | `serial_number` (derived) | HR(13–17) | serial, 5 regs | `GW2423G192` |
| Firmware version | `software_version` | IR(1600–1603) | `gateway_version` | `GA000009` |
| Firmware variant | (from `software_version`) | IR(1603) | u16 | 9 → V1; ≥10 → V2 |
| Work mode | `work_mode` | IR(1604) | WorkMode enum | `2` = On Grid |
| Primary AIO serial | `first_inverter_serial_number` | IR(1627–1631) | serial, 5 regs | `SA24230001` |
| Faults | `gateway_fault_codes` | IR(1622–1623) | u32 bitmask → [names] | `[]` = healthy |

### 3.2 Live power-flow gauges (the main dashboard)

| Display element | Register name | Reg | Type / scale | Sign / range |
|---|---|---|---|---|
| Grid voltage | `v_grid` | IR 1608 | ÷10 V | 0–500 |
| Grid current | `i_grid` | IR 1609 | ÷10 A, **int16** | signed, ±500 |
| Load voltage | `v_load` | IR 1610 | ÷10 V | 0–500 |
| Load current | `i_load` | IR 1611 | ÷10 A | unsigned, 0–500 |
| PV current | `i_pv` | IR 1612 | ÷10 A, int16 | 0–500 |
| AIO inverter power (port 1) | `p_ac1` | IR 1616 | W, **int16** | **+ = discharge/out, − = charge/in** |
| **PV generation** | `p_pv` | IR 1617 | W, u16 | unsigned, ≤50000 |
| **House load (excl. EV!)** | `p_load` | IR 1618 | W, u16 | unsigned, ≤50000 |
| Smart/Liberty load | `p_liberty` | IR 1619 | W, int16 | signed |
| Grid relay voltage | `v_grid_relay` | IR 1624 | ÷10 V | |
| Inverter relay voltage | `v_inverter_relay` | IR 1625 | ÷10 V | |
| Fault protection bitmask | `fault_protection` | IR 1620–1621 | u32 | |

> **The `p_load` field is the gateway's defining property:** it is measured by the
> gateway's own pre-installed meter and **excludes the EV charger**. An AIO's own load
> figure does *not* exclude the charger. Always prefer the gateway's `p_load` for
> "house load."

### 3.3 AIO stack summary card (the battery/inverter overview)

| Display element | Register name | Reg | Type | Meaning |
|---|---|---|---|---|
| Number of AIOs configured | `parallel_aio_num` | IR 1700 | u16 | 1–3 |
| Number of AIOs online | `parallel_aio_online_num` | IR 1701 | u16 | health: should equal 1700 |
| Aggregate AIO power | `p_aio_total` | IR 1702 | W, **int16** | sum of all AIO inverter power, same sign as `p_ac1` |
| Aggregate battery state | `aio_state` | IR 1703 | enum | **1=charging, 2=discharging, 0=idle** |
| Battery firmware | `battery_firmware_version` | IR 1704 | u16 | info-only |

### 3.4 Per-AIO detail (expandable rows; single-AIO installs only populate AIO1)

For AIO index `n` ∈ {1,2,3}. Single-AIO topology: only the AIO1 columns are non-zero.

| Display element | Register name | AIO1 regs | AIO2 regs | AIO3 regs | Type |
|---|---|---|---|---|---|
| Charge today | `e_aio{N}_charge_today` | 1705 | 1708 | 1711 | ÷10 kWh |
| Charge total | `e_aio{N}_charge_total` | 1706–1707 | 1709–1710 | 1712–1713 | u32 ÷10 kWh |
| Discharge today | `e_aio{N}_discharge_today` | 1750 | 1753 | 1756 | ÷10 kWh |
| Discharge total | `e_aio{N}_discharge_total` | 1751–1752 | 1754–1755 | 1757–1758 | u32 ÷10 kWh |
| Inverter power | `p_aio{N}_inverter` | 1816 | 1817 | 1818 | W, **int16** |
| Serial number | `aio{N}_serial_number` | 1831–1835 | 1838–1842 | 1845–1849 | serial, 5 regs (V1) |

### 3.5 Battery gauge (aggregate + per-AIO SOC)

| Display element | Register name | Reg(s) | Type | Notes |
|---|---|---|---|---|
| Battery charge today | `e_battery_charge_today` | IR 1795 | ÷10 kWh | aggregate |
| Battery charge total | `e_battery_charge_total` | IR 1796–1797 | u32 ÷10 kWh | V1 byte order |
| Battery discharge today | `e_battery_discharge_today` | IR 1798 | ÷10 kWh | aggregate |
| Battery discharge total | `e_battery_discharge_total` | IR 1799–1800 | u32 ÷10 kWh | V1 byte order |
| **AIO1 SOC** | `aio1_soc` | IR 1801 | u16 % | 0–100 |
| AIO2 SOC | `aio2_soc` | IR 1802 | u16 % | 0 if absent |
| AIO3 SOC | `aio3_soc` | IR 1803 | u16 % | 0 if absent |

> **No per-cell data on the gateway.** Cell voltages/temperatures live on each AIO's own
> battery BMS — reachable only via the direct AIO connection (Part 1.4). The gateway gives
> you SOC + aggregate energy only.

### 3.6 Energy bar charts (daily + lifetime)

| Flow | Register name (today / total) | Today reg | Total regs (V1) | Scale |
|---|---|---|---|---|
| Grid import | `e_grid_import_today` / `_total` | 1640 | 1641, 1642 | ÷10 kWh |
| PV generation | `e_pv_today` / `_total` | 1643 | 1644, 1645 | ÷10 kWh |
| Grid export | `e_grid_export_today` / `_total` | 1646 | 1647, 1648 | ÷10 kWh |
| AIO battery charge | `e_aio_charge_today` / `_total` | 1649 | 1650, 1651 | ÷10 kWh |
| AIO battery discharge | `e_aio_discharge_today` / `_total` | 1652 | 1653, 1654 | ÷10 kWh |
| House load | `e_load_today` / `_total` | 1655 | 1656, 1657 | ÷10 kWh |

---

## Part 4 — The V1/V2 variant trap (decode energy totals correctly or get garbage)

Two firmware variants differ in wire encoding. Pick the wrong one and every `uint32`
energy total is off by orders of magnitude.

| Variant | Selector | uint32 byte order | AIO serial addresses |
|---|---|---|---|
| **V1** | `IR(1603) < 10` | **high register first**, then low | aio1 @ 1831–1835 |
| **V2** | `IR(1603) >= 10` | **low register first**, then high (swapped) | aio1 @ 1841–1845 |

```python
# Decide once per connect:
is_v2 = (ir(1603) is not None) and (ir(1603) >= 10)

# Decode every uint32 total accordingly:
def u32(hi_reg, lo_reg):
    return (lo_reg << 16 | hi_reg) if is_v2 else (hi_reg << 16 | lo_reg)

# AIO serial addresses also shift:
aio1_serial_base = 1841 if is_v2 else 1831   # then read 5 regs
```

**This simulator emits V1 only** (`IR(1603) = 9`), but ship the V2 path — real hardware
on `GA000010+` firmware needs it.

---

## Part 5 — Sign conventions (the #1 source of inverted-arrow bugs)

| Quantity | Register name | Raw type | Display meaning |
|---|---|---|---|
| Grid power | `i_grid` (and derived) | signed | **+ = import (from grid), − = export (to grid)** |
| AIO / battery power | `p_ac1`, `p_aio_total`, `p_aio{N}_inverter` | signed | **+ = discharging (out), − = charging (in)** |
| PV power | `p_pv` | unsigned | always ≥ 0 (generation) |
| Load power | `p_load` | unsigned | always ≥ 0 (**excludes EV**) |
| Smart load | `p_liberty` | signed | + = active |

The battery sign here is the **GivEnergy wire convention** (+ = discharge) — the
*opposite* of some libraries' internal convention (+ = charge). If your battery arrow
points the wrong way, you forgot to negate.

---

## Part 6 — Polling strategy

### 6.1 The Gateway connection

To fully populate the model, issue **Input Register reads (function `0x04`)** at the
gateway's slave, batched into ≤60-register reads (the dongle dislikes huge reads):

| Read # | Range | What it covers |
|---|---|---|
| 1 | HR(0–59) | identity, config, serial, firmware (the standard block every device gets) |
| 2 | IR(1600–1631) | version, work mode, V/I/P, faults, first AIO serial |
| 3 | IR(1640–1657) | daily + lifetime energy (grid/pv/aio/load) |
| 4 | IR(1700–1713) | AIO summary + per-AIO charge |
| 5 | IR(1750–1758) | per-AIO discharge |
| 6 | IR(1795–1803) | battery aggregate energy + per-AIO SOC |
| 7 | IR(1816–1818) + IR(1831–1849) | per-AIO power + serials |

Refresh at your normal cadence (5–30 s typical). The `dewet22` library's `refresh()`
does reads 2–7 automatically when `capabilities.is_gateway` is true.

### 6.2 The direct AIO connection(s) — only if you want per-module detail

Each AIO is polled as a normal All-in-One: `detect()` → `load_config()` → `refresh()`.
The AIO exposes its battery modules at slaves `0x50–0x53` (per-module cell/temp/serial),
discovered via the BCU at `0x70`. This is identical to polling a standalone AIO — the
Gateway plays no role in this connection.

### 6.3 What NOT to poll against a Gateway

- **Battery BMS slaves `0x32–0x37`** (LV pack protocol) — the gateway has no direct batteries; these belong to hybrids.
- **HV cluster path `0xA0` / `0x70` / `0x50`** — same; these belong to HV hybrids/AIOs, not the gateway. The gateway aggregates battery data *for* you in its own bank.
- **IR outside 1600–1859 on the gateway** — unmapped; expect zeros or error responses.

---

## Part 7 — Building the power-flow diagram

From the gateway's instantaneous readings (all converted to kW):

**Inputs:**
- `pv` ← `p_pv` (IR 1617), ≥ 0
- `grid` ← derived from `i_grid`/`p_ac1`: **+ = import, − = export**
- `battery` ← `p_aio_total` (IR 1702): **+ = discharging, − = charging**

**8-state headline** (grid > battery > solar precedence; idle threshold ~0.05 kW):

| State | Condition | Icon |
|---|---|---|
| `EXPORTING_SOLAR` | grid<0 & pv>0 | ☀️→grid |
| `EXPORTING_BATTERY` | grid<0 & battery>0 & pv≈0 | 🔋→grid |
| `IMPORTING` | grid>0 & battery≈0 & pv<load | grid→🏠 |
| `DISCHARGING` | battery>0 & grid≈0 | 🔋→🏠 |
| `CHARGING_FROM_SOLAR` | battery<0 & pv>0 & grid≈0 | ☀️→🔋 |
| `CHARGING` | battery<0 & grid>0 | grid→🔋 |
| `SOLAR_COVERING_HOUSE` | pv>0 & pv≥load & battery≈0 & grid≈0 | ☀️→🏠 |
| `IDLE` | everything ≈0 | 💤 |

`aio_state` (IR 1703: 1=charging, 2=discharging, 0=idle) is a quick independent
confirmation of the battery arrow direction.

---

## Part 8 — Minimum viable display (4 reads, ~20 registers)

For a single-AIO Gateway, the fewest registers that produce a useful homeowner dashboard:

| Reg(s) | Register name | Widget |
|---|---|---|
| 1600–1603 | `software_version` | "Gateway GA000009" header + validity check |
| 1604 | `work_mode` | "On Grid" status badge |
| 1617 | `p_pv` | PV generation gauge |
| 1618 | `p_load` | **House load gauge (excl. EV)** |
| 1702 | `p_aio_total` | Battery arrow (sign = direction) |
| 1703 | `aio_state` | Battery icon (charge/discharge/idle) |
| 1801 | `aio1_soc` | Battery SOC % gauge |
| 1640,1643,1646,1649,1652,1655 | today energy ×6 | Daily energy bar chart |

Add the lifetime totals (§3.6) and per-AIO rows (§3.4) for a power-user view.

---

## Part 9 — Control writes (parallel installs)

**Route ALL control writes to the GATEWAY, never to child AIOs.** The Gateway is the
authoritative control endpoint for a parallel group; writing to an individual AIO can
desync the group. Control targets the same standard holding registers as any GivEnergy
device (charge/discharge enable, SOC target, charge/discharge slots — see
`AGENTS.md` "Holding Registers" for the addresses). The Gateway forwards configuration
to its child AIOs over the RJ45 comms bus.

For a single-AIO install there's only one device, so this distinction is moot — but
structure your client to target the gateway connection for writes regardless, so it
scales to parallel installs without code changes.

---

## Part 10 — Testing against this simulator

```bash
# Single-AIO Gateway on the wire (Topology A):
cargo run -p sim-api -- simulate --inverter Gateway12kW --batteries 1 --soc 65 \
    --tick-interval 5 --modbus 0.0.0.0:8899
```

What you will observe (confirmed via live Modbus query):
- DTC `0x7001`, serial **`GW2423G192`** → detection classifies Gateway ✓
- Version `GA000009`, **IR(1603)=9 → V1** ✓
- `parallel_aio_num = 1`, `aio1_soc` tracks the battery, `aio2/aio3` = 0 ✓
- `aio1_serial = SA24230001` round-trips; AIO2/AIO3 serials empty ✓
- Energy totals use V1 (high-register-first) byte order ✓
- Battery power uses GE sign convention (+ discharge / − charge) ✓
- `p_load` excludes the (unmodelled) EV charger ✓

**Known simulator limitations (code defensively against these):**
- **Topology B (parallel AIOs) is NOT modelled** — always single-AIO. The simulator runs one plant = one gateway + one implicit AIO. To test a multi-AIO client you'd run multiple sim instances on different ports and treat them as separate IPs.
- **No direct AIO connection** in this simulator — the single-AIO gateway projection *is* the whole plant; there's no separate AIO device to connect to. (A direct-AIO-connection code path can only be tested against real hardware or a separate AIO sim instance.)
- **V2 firmware variant not emitted** (always V1).
- Daily and lifetime energy registers read identically (no midnight reset).
- Control writes behave like a normal inverter; gateway-as-authoritative-control routing is not differentiated.

---

## Part 11 — Reference pointers

- **Byte-level register map, converters, fault bitmask:** [`gateway-register-reference.md`](./gateway-register-reference.md)
- **Client library (detection, refresh, variant logic):** `dewet22/givenergy-modbus` `client/client.py` (`detect()`, `refresh()` → the `is_gateway` read loop), `model/gateway.py` (`select_gateway`, the LUTs)
- **Real-hardware parallel behaviour:** GivTCP v3 README ("Parallel AIO" section) + GivEnergy "AIO – Parallel Connection Guide" (installer doc, physical wiring)
- **Simulator implementation:** `crates/sim-registers/src/lib.rs` → `project_gateway_bank()`

---

## Appendix A — Quick decision table

| Question | Answer |
|---|---|
| Which port does the Gateway use? | **8899** (GivEnergy proprietary Modbus TCP) |
| Which port do the AIOs use? | **8899** (same protocol, each AIO's own dongle) |
| Do I talk to the AIO via the Gateway? | **No.** Separate TCP connection to each AIO's own IP. |
| How many connections for a dual-AIO install? | Up to 3 (1 Gateway + 2 AIOs), but **1 (Gateway only)** is enough for the standard dashboard. |
| How does the Gateway "know" about its AIOs? | Data-level: it reports their serials + summarised telemetry in IR 1600–1859. Hardware-level: RJ45 comms bus (invisible to Modbus). |
| Where do control writes go? | **The Gateway**, always. |
| Where does battery cell data come from? | The **AIO** (direct connection), not the Gateway. Gateway has SOC + aggregate energy only. |
| How do I detect a Gateway? | Serial prefix `GW` on HR(13–17) (authoritative); DTC `0x7001` confirms. |
| How do I pick V1 vs V2? | `IR(1603) >= 10` → V2 (swapped uint32 byte order + shifted serial addresses). |
| Why is my house load wrong? | You're reading the AIO's load, not the Gateway's `p_load` (IR 1618), which excludes the EV charger. |

---

*The authoritative integration guide. Companion to `gateway-register-reference.md`
(byte-level detail) and `gateway-client-integration.md` (shorter overview).*
