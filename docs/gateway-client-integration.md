# GivEnergy Gateway — Client Display Integration Guide

> **Audience:** engineers building the *display/UI end* of a GivEnergy client
> (the `givenergy-local` / `givenergy-modbus` Python library, a Home Assistant
> integration, GivTCP, a custom dashboard). This is the high-level "how to think
> about this device and what to show" companion to the exhaustive register map
> in [`gateway-register-reference.md`](./gateway-register-reference.md).
>
> Read this first; drill into the register reference only when you need exact
> byte offsets.

## 1. Mental model — what you are displaying

The Gateway is **not an inverter**. Stop thinking "one device, one battery, one
PV string." Instead think: **a hub with a built-in energy meter that measures
the whole AC system and reports an aggregate view of the All-in-One (AIO)
unit(s) behind it.**

```
            ┌─────────────────────────────────────────┐
   GRID ───▶│              GATEWAY (0x7001)           │─── PV (metered)
            │  • built-in energy meter (grid/PV/load)  │
            │  • backup/transfer switch                │
            │  • aggregation logic                     │
            └──────────────┬──────────────────────────┘
                           │ AC (measured)
              ┌────────────▼────────────┐
              │   Child AIO inverter(s) │  ← battery lives here, not in gateway
              │   (DTC 0x8xxx, e.g.     │
              │    AllInOne 0x8002)     │
              └─────────────────────────┘
```

**Three things to internalise, because they shape the entire UI:**

1. **The Gateway's `load` figure is the authoritative house load** — it is
   measured by the gateway's own meter and **correctly excludes the EV charger**.
   An AIO's `load` figure does *not* exclude the charger. If you display both,
   prefer the gateway's.

2. **The Gateway has zero direct batteries.** The battery/SOC you display comes
   from the gateway's *aggregated* view of its child AIO(s) (IR 1700–1803), not
   from a battery BMS attached to the gateway itself. Do **not** probe battery
   slaves `0x32–0x37` or the HV cluster path (`0xA0`/`0x70`/`0x50`) against the
   gateway — those belong to hybrid inverters, and the gateway will serve nothing
   meaningful there.

3. **The Gateway is the control endpoint for parallel installs.** When a user
   sets a charge schedule or SOC target, the write goes to the **gateway**. Child
   AIOs are read-only telemetry satellites. If your UI offers controls, route
   them to the gateway.

## 2. Detection — "is this thing a Gateway?"

You detect a Gateway on every connect. Three signals, all of which should agree;
treat the **serial prefix as authoritative**:

| # | Signal | Where | Gateway value | Meaning |
|---|---|---|---|---|
| 1 | **Serial-number prefix** | HR(13–17), 5 regs, Latin-1 | `GW…` (e.g. `GW2423G192`) | **Primary classifier.** First two bytes = `'G'`,`'W'`. |
| 2 | Device Type Code | HR(0) | `0x7001` (family `0x7xxx`) | Confirms it. |
| 3 | Firmware version string | IR(1600–1603), `gateway_version` decode | `GA000009` / `GA000010` / … | Also the V1/V2 selector (§4). |

**Recommended detection flow:**

```
read HR(0–59)
decode serial from HR(13–17)
if serial.startswith("GW"):
    device_class = GATEWAY
    read IR(1600–1859)              # the aggregation bank
    pick variant from IR(1603)      # <10 → V1, >=10 → V2
    validity = (version_string is non-empty)   # i.e. IR(1600–1603) not all zero
else:
    device_class = <hybrid/AIO/etc.>
    # do NOT read IR(1600–1859) — they are zeros, will decode to None
```

> **Validity gotcha:** `gateway.is_valid()` is `software_version is not None`,
> i.e. IR(1600–1603) must decode to a non-blank string. If a simulator (or a
> non-gateway device) leaves that block zeroed, the client silently treats the
> gateway as absent. Always check validity before rendering gateway widgets.

## 3. The display data model — six groups

The IR 1600–1859 bank decomposes into six logical groups. Map each to a UI
region. Addresses below are V1; see §4 for V1/V2 differences.

### Group A — Identity & state (IR 1600–1631) → header / status badge
| Field | Regs | For display |
|---|---|---|
| `software_version` | 1600–1603 | Firmware string (`GA000009`), shown in device header |
| `work_mode` | 1604 | Enum badge. **2 = On Grid** is the normal state. (See reference §6 for the full enum.) |
| `first_inverter_serial_number` | 1627–1631 | "Primary AIO: SA24230001" subtitle |
| `gateway_fault_codes` | 1622–1623 | Fault badge / list (bitmask → names). Empty = healthy. |

### Group B — Instantaneous power (IR 1608–1619) → the live power-flow diagram
| Field | Reg | Units | Sign | Display meaning |
|---|---|---|---|---|
| `v_grid` | 1608 | V (÷10) | — | Grid voltage gauge |
| `i_grid` | 1609 | A (÷10) | **signed** | Grid current |
| `v_load` | 1610 | V (÷10) | — | Load-side voltage |
| `i_load` | 1611 | A (÷10) | unsigned | House current |
| `i_pv` | 1612 | A (÷10) | — | PV current |
| `p_ac1` | 1616 | W | **signed** | AIO inverter power (+ = out/discharge, − = in/charge) |
| `p_pv` | 1617 | W | unsigned | Total PV generation |
| **`p_load`** | **1618** | **W** | unsigned | **House load (excludes EV charger!)** |
| `p_liberty` | 1619 | W | signed | Smart/Liberty load (often 0 / unmodelled) |

### Group C — AIO stack summary (IR 1700–1704) → battery/inverter summary card
| Field | Reg | For display |
|---|---|---|
| `parallel_aio_num` | 1700 | "1 AIO" / "2 AIOs" |
| `parallel_aio_online_num` | 1701 | Online count (health indicator if < num) |
| `p_aio_total` | 1702 | Aggregate inverter power, W **signed** (same sign as `p_ac1`) |
| `aio_state` | 1703 | **1 = charging, 2 = discharging, 0 = idle** — drives the battery icon |
| `battery_firmware_version` | 1704 | Info-only |

### Group D — Per-AIO detail (IR 1705–1713, 1750–1758, 1816–1818, 1831–1849) → expandable per-inverter list
For a single-AIO install (the common case), only AIO1 is populated; AIO2/AIO3
read zero. For each AIO `n` (1–3) you have: daily charge/discharge, lifetime
charge/discharge, inverter power, and a serial number. Display this as an
expandable list — most installs show one row.

| Per AIO `n` | Charge today | Charge total | Disch today | Disch total | Inverter power | Serial |
|---|---|---|---|---|---|---|
| Address (V1) | 1705/1708/1711 | 1706–7/1709–10/1712–13 | 1750/1753/1756 | 1751–2/1754–5/1757–8 | 1816/1817/1818 | 1831–5/1838–42/1845–49 |

### Group E — Battery aggregate + per-AIO SOC (IR 1795–1803) → battery gauge
| Field | Regs | For display |
|---|---|---|
| `e_battery_charge_today` / `_total` | 1795 / 1796–1797 | Battery charge energy |
| `e_battery_discharge_today` / `_total` | 1798 / 1799–1800 | Battery discharge energy |
| `aio1_soc` / `aio2_soc` / `aio3_soc` | 1801 / 1802 / 1803 | **Per-AIO SOC %**. Average for a single battery widget, or show per-AIO. |

> **No per-cell data on the gateway.** Cell voltages/temps live on the AIO's own
> battery BMS (separate connection). The gateway only gives you SOC + aggregate
> energy. Don't expect cell-level telemetry here.

### Group F — Energy totals (IR 1640–1657) → the energy bar charts
Daily counters (÷10 kWh) and lifetime totals (`uint32`, ÷10 kWh). Six flows:

| Flow | Today | Lifetime (V1 regs) |
|---|---|---|
| Grid import | 1640 | 1641, 1642 |
| PV generation | 1643 | 1644, 1645 |
| Grid export | 1646 | 1647, 1648 |
| AIO battery charge | 1649 | 1650, 1651 |
| AIO battery discharge | 1652 | 1653, 1654 |
| House load | 1655 | 1656, 1657 |

> **Daily-vs-lifetime caveat:** the gateway exposes separate "today" and "total"
> registers, but many simulators (this one included) do not reset daily counters
> at midnight — so "today" and "total" may read identically in a fresh sim. Real
> hardware resets "today" at local midnight. Clients that graph "today" should
> handle the case where today == total gracefully.

## 4. The V1/V2 variant trap (read this before decoding energy totals)

There are **two firmware variants** that differ in wire encoding. Pick the wrong
one and every `uint32` energy total decodes wrong by orders of magnitude.

| Variant | Selector | uint32 byte order | AIO serial addresses |
|---|---|---|---|
| **V1** | `IR(1603) < 10` | **high register first**, then low | aio1 @ 1831–1835 |
| **V2** | `IR(1603) >= 10` | **low register first**, then high (swapped) | aio1 @ 1841–1845 |

```python
# One-time variant decision, per connect:
is_v2 = (ir1603 is not None) and (ir1603 >= 10)
```

Then for every `uint32` total:
```python
def u32(hi_reg, lo_reg, v2=False):
    return (lo_reg << 16 | hi_reg) if v2 else (hi_reg << 16 | lo_reg)
```

And for AIO serials, use the V1 addresses (1831+) or V2 addresses (1841+)
accordingly. **This simulator emits V1 only** (`IR(1603) = 9`), so against this
sim you will always take the V1 path — but ship the V2 path too or real-hardware
users on `GA000010+` will see garbage energy graphs.

## 5. Building the power-flow diagram

The headline "what is the system doing" visual. From the gateway's instantaneous
readings (all in kW; convert W→kW):

**Inputs:**
- `pv` ← `p_pv` (IR 1617), **always ≥ 0**
- `grid` ← derived: **+ = import, − = export** (from `i_grid` sign or net power)
- `battery` ← `p_aio_total` (IR 1702), **+ = discharging, − = charging**

**The 8-state headline** (grid > battery > solar precedence, idle threshold
~0.05 kW):

| State | Condition | Icon |
|---|---|---|
| `EXPORTING_SOLAR` | grid<0 (export) & pv>0 | ☀️→grid |
| `EXPORTING_BATTERY` | grid<0 & battery>0 & pv~0 | 🔋→grid |
| `IMPORTING` | grid>0 & battery~0 & pv<load | grid→🏠 |
| `DISCHARGING` | battery>0 & grid~0 | 🔋→🏠 |
| `CHARGING_FROM_SOLAR` | battery<0 & pv>0 & grid~0 | ☀️→🔋 |
| `CHARGING` | battery<0 & grid>0 | grid→🔋 |
| `SOLAR_COVERING_HOUSE` | pv>0 & pv≥load & battery~0 & grid~0 | ☀️→🏠 |
| `IDLE` | everything ~0 | 💤 |

The stateless edge decomposition (solar prioritised) for the Sankey-style flow:

```
solar_to_batt   = min(pv, -battery if charging else 0)
grid_to_batt    = min(grid_import, charge_remaining)
solar_to_grid   = min(pv - solar_to_batt, grid_export)
batt_to_grid    = min(battery_discharge, export_remaining)
solar_to_home   = pv - solar_to_batt - solar_to_grid
batt_to_home    = battery_discharge - batt_to_grid
grid_to_home    = grid_import - grid_to_batt
```

`aio_state` (IR 1703) is a quick independent signal: 1=charging, 2=discharging,
0=idle — useful to confirm the battery arrow direction without recomputing.

## 6. Sign conventions cheat sheet

The single biggest source of "my arrows point the wrong way" bugs:

| Quantity | Reg | Raw | Display |
|---|---|---|---|
| Grid power | `i_grid`/derived | signed | **+ import (from grid), − export (to grid)** |
| AIO/battery power | `p_ac1`, `p_aio_total`, `p_aio*_inverter` | signed | **+ discharging (out), − charging (in)** |
| PV power | `p_pv` | unsigned | always ≥ 0 (generation) |
| Load power | `p_load` | unsigned | always ≥ 0 (consumption, **excludes EV**) |
| Smart load | `p_liberty` | signed | + active |

> Note the battery sign here is the **GivEnergy wire convention** (+ = discharge),
> which is the *opposite* of the internal convention some libraries use
> (+ = charge). If your battery arrow is inverted, you forgot the negate.

## 7. Polling strategy (high level)

To fully populate the display, issue **Input Register reads (function `0x04`)** at
the gateway's slave. Batch into ~6 reads of ≤60 registers each (the dongle
dislikes huge reads):

1. `IR(1600–1631)` — identity, state, V/I/P, faults, first AIO serial
2. `IR(1640–1657)` — daily + lifetime energy
3. `IR(1700–1713)` — AIO summary + per-AIO charge
4. `IR(1750–1758)` — per-AIO discharge
5. `IR(1795–1803)` — battery aggregate energy + SOC
6. `IR(1816–1818)` + `IR(1831–1849)` — per-AIO power + serials

…plus the standard identity/config block `HR(0–119)` read that every GivEnergy
device gets on detect. Refresh at your normal cadence (typically 5–30 s).

**What not to poll:** battery BMS slaves (`0x32–0x37`, `0xA0`/`0x70`/`0x50`).
The gateway aggregates battery data for you — the per-cell telemetry, if you want
it, comes from a *separate* connection to each child AIO.

## 8. Minimum viable display (if you only implement one screen)

For a single-AIO Gateway, the fewest registers that produce a useful dashboard:

| Reg(s) | Field | Widget |
|---|---|---|
| 1600–1603 | version | "Gateway GA000009" header + validity check |
| 1604 | work_mode | "On Grid" status badge |
| 1617 | p_pv | PV generation gauge |
| 1618 | p_load | **House load gauge (excl. EV)** |
| 1702 | p_aio_total | Battery arrow (sign = direction) |
| 1703 | aio_state | Battery icon (charge/discharge/idle) |
| 1801 | aio1_soc | Battery SOC % gauge |
| 1640/1643/1646/1649/1652/1655 | today energy | 6-bar daily energy chart |

That's 4 reads, ~20 registers, and it covers everything a homeowner sees. Add
the totals and per-AIO detail for a power-user view.

## 9. Testing against this simulator

```bash
# Start a single-AIO Gateway on the wire:
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

**Known simulator limitations to code defensively against:**
- V2 firmware variant is **not** emitted (always V1). Don't assume you'll ever
  see `IR(1603) >= 10` here — but your client must still handle it.
- Daily and lifetime energy registers read identically (no midnight reset).
- Multi-AIO topology (2–3 AIOs) is not modelled — always single-AIO.
- Control writes behave like a normal inverter; gateway-as-authoritative-control
  routing for parallel installs is not yet differentiated.

## 10. Reference pointers

- **Exhaustive register map, converters, fault bitmask:** [`gateway-register-reference.md`](./gateway-register-reference.md)
- **Power-flow classification spec (the shared canonical decomposition):** §9 of the register reference
- **Simulator implementation:** `crates/sim-registers/src/lib.rs` → `project_gateway_bank()`
- **Detection + variant logic origin:** `dewet22/givenergy-modbus` `model/gateway.py` (`select_gateway`)

---

*Companion to `gateway-register-reference.md`. This guide answers "what do I show
and how"; the reference answers "what is the exact byte at address X."*
