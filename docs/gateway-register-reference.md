# GivEnergy Gateway — Modbus Register Reference & Simulation Spec

> **Purpose.** A standalone, client-implementation-grade reference for the GivEnergy
> Gateway device family. This is the authoritative source for simulating a Gateway on
> port 8899 and for adding Gateway support to any local Modbus client (the
> `givenergy-local` / `givenergy-modbus` Python library, GivTCP, Home Assistant
> integrations, etc.). Every register address, scaling, sign convention, and enum below
> is reverse-engineered from the reference implementations and confirmed against their
> test suites — sources cited inline.
>
> **Status:** Phase 1 research complete. The register map is definitive; the simulation
> build (Phase 2) is pending.

## 1. What the Gateway is

The GivEnergy **Gateway** (SKU `GIV-AIO-GW1`; newer **Gateway 2** `GIV-AIO-GW1-2`) is
**not an inverter**. It is an AC distribution + backup-transfer hub that sits in front of
one or more **All-in-One (AIO)** inverter/battery units. It is the system's measurement
and control point.

**Physical role (from the datasheets):**

| Function | Gateway 1 | Gateway 2 |
|---|---|---|
| PV input AC | 18.4 kW / 100 A | 18.4 kW / 100 A |
| Grid → General Load pass-through | — | 18.4 kW / 80–100 A |
| Critical Load output (EPS) | — | 9.2 kW / 40 A |
| Smart Load output | — | 9.2 kW / 40 A |
| EV Charger output | — | 7.2 kW / 32 A |
| AIO input | — | 52–63 A |
| Pre-installed energy meter | ✔ | ✔ |
| Grid switchover | 20 ms | instant |
| Comms | RS485 / CAN / LoRa / WiFi / LAN / 4G | + BLE |

**Logical role on the wire (the important part for simulation):**

- It speaks the **same GivEnergy proprietary Modbus TCP envelope on port 8899** as every
  other device (see `AGENTS.md` → "GivEnergy Modbus protocol is NOT standard Modbus TCP").
- It exposes a **gateway-specific Input Register bank at IR 1600–1859** that **aggregates**
  data from the AIO(s) behind it: grid (from its built-in meter), PV, load, per-AIO
  power/SOC, energy totals, and relay/voltage state.
- It is the **control endpoint**. GivTCP's own guidance for parallel-AIO installs:
  *"The Gateway inverter should be the only inverter that is controlled via GivTCP, the
  individual AIO inverter data treated for information only."* Writes to the Gateway's
  schedule/SOC registers are authoritative; per-AIO writes are informational.
- A real Gateway has **zero directly-attached batteries** — the batteries live in the AIOs.
  GivTCP logs this explicitly: `"GW2423G192 which is a Gen1 - Gateway with 0 batteries"`.
- Its **load figure correctly excludes the EV charger**; the AIO's does not. This is why
  real users treat the Gateway as the authoritative "house load" source.

## 2. Device detection & addressing

A client concludes "this is a Gateway" from a combination of three signals. **All three
should agree** on a real device; a simulator must emit all three consistently.

| Signal | Gateway value | Where | Notes |
|---|---|---|---|
| **Serial-number prefix** | `GW` (e.g. `GW2423G192`) | HR(13–17), 5 regs, Latin-1 | The **primary** classifier. GivTCP's detection string is built from this prefix. Standard GE serial shape: `AAYYWWANNN` → here `AA`=`GW`. |
| **Device Type Code (DTC)** | `0x7001` | HR(0) | Family prefix `0x7xxx`. Our catalogue already maps `Gateway12kW → 0x7001`. |
| **Firmware version string** | `GA000009` / `GA0000010` / … | IR(1600–1603) | Decoded by `gateway_version`. **Also the variant selector** (see §3). |

**Wire addressing.** The Gateway answers transparent Modbus reads addressed to **slave
`0x32`** for its inverter-style block (HR/IR 0–119, the standard identity + config block
every GE device exposes) **and** serves the gateway aggregation bank over **Input
Register reads (function `0x04`)** at the same slave. There is no separate slave address
for the gateway bank — it lives in the Input Register space of the gateway's own dongle
address. (Mirrors how a hybrid exposes battery BMS data at slaves `0x32–0x37`; here the
gateway aggregation data is on the gateway's primary address, just at high IR offsets.)

**Detection sequence a client runs** (matches `dewet22/givenergy-modbus` `Client.detect()` +
GivTCP `startup.py`):

1. Connect TCP `host:8899`.
2. Read HR(0–59) at slave `0x11`/`0x32` → identity block.
3. Decode serial from HR(13–17); **prefix `GW` → device class Gateway**.
4. For a Gateway, additionally read **IR(1600–1859)** (the aggregation bank) and decode
   via the Gateway model. Firmware variant is chosen from IR(1603) at this point.
5. Child AIOs (if modelling a parallel install) are discovered as *separate* TCP
   connections at their own IPs, each a normal AIO device.

> A client that only reads HR/IR 0–119 on a Gateway will get a valid but **partial**
> picture (identity + the standard config block, none of the aggregation). The
> GivTCP `power_flow_output` crash (issues #364/#367) was precisely a client reading the
> gateway bank but failing to decode `power_flow_output` from it — i.e. the bank *was*
> being polled, the decoder was the bug.

## 3. Firmware variants — V1 vs V2 (critical)

There are **two Gateway firmware variants** that differ in wire encoding. A client/simulator
**must** pick the right one or every `uint32` energy total decodes wrong by 4+ orders of
magnitude. Source: `dewet22/givenergy-modbus` `model/gateway.py`.

| Variant | Firmware | Selector | uint32 energy-total byte order | AIO serial addresses |
|---|---|---|---|---|
| **GatewayV1** | `GA000009` and earlier | `IR(1603) < 10` | **high register first**, then low | aio1 @ IR(1831–1835) |
| **GatewayV2** | `GA000010` and later | `IR(1603) >= 10` | **low register first**, then high (swapped) | aio1 @ IR(1841–1845) |

**Selection rule** (verbatim from `select_gateway()`):
```python
fw_raw = register_cache.get(IR(1603))
if fw_raw is not None and fw_raw >= 10:
    return GatewayV2   # GA000010+
return GatewayV1       # GA000009 and earlier (also the empty-cache default)
```

**Implication for simulation:** pick one variant and emit it consistently. V1 (`GA000009`)
is the safe default and matches the empty-cache fallback every client uses. If simulating
V2, set IR(1603) ≥ 10 **and** use the swapped byte order **and** the shifted AIO-serial
addresses — all three change together.

**Validity check** (what a client uses to decide "a gateway is actually present"):
`gateway.is_valid()` returns true iff `software_version is not None`, i.e. iff the
IR(1600–1603) version registers decode to a non-empty string. An all-zero IR(1600–1603)
bank means "no gateway" → a simulator must populate at least the version registers or
clients will silently ignore the gateway block.

## 4. Complete register map — Input Registers IR 1600–1859

All gateway aggregation data is in **Input Registers** (function `0x04`). Source:
`dewet22/givenergy-modbus` `model/gateway.py` (`_GATEWAY_COMMON_LUT` + variant LUTs),
cross-checked against `tests/model/test_gateway.py`. "V1 order" / "V2 order" columns apply
only to the `uint32` totals (§3).

### 4.1 System state & version — IR 1600–1631

| Reg(s) | Field | Type | Scale / decode | Notes |
|---|---|---|---|---|
| 1600–1603 | `software_version` | str | `gateway_version` | e.g. `GA000009`. **IR(1603) is also the V1/V2 selector.** |
| 1604 | `work_mode` | enum | `WorkMode` (uint16) | See §6. |
| 1608 | `v_grid` | float | `int16` ÷ 10 | Grid voltage, V. Range 0–500. |
| 1609 | `i_grid` | float | `int16` ÷ 10 | Grid current, A. **Signed.** Range −500–+500. |
| 1610 | `v_load` | float | ÷ 10 | Load voltage, V. Range 0–500. |
| 1611 | `i_load` | float | ÷ 10 | Load current, A. Range 0–500 (unsigned). |
| 1612 | `i_pv` | float | `int16` ÷ 10 | PV current, A. Range 0–500. |
| 1616 | `p_ac1` | int | `int16` | AC port 1 power, W (signed). |
| 1617 | `p_pv` | uint | uint16 | PV total power, W. Max 50000. |
| 1618 | `p_load` | uint | uint16 | Load power, W. Max 50000. **Excludes EV charger.** |
| 1619 | `p_liberty` | int | `int16` | Liberty/smart-load power, W (signed). |
| 1620–1621 | `fault_protection` | uint32 | — | Fault protection bitmask. |
| 1622–1623 | `gateway_fault_codes` | uint32 → `[str]` | `_gateway_fault_code` | Decoded fault name list. See §7. |
| 1624 | `v_grid_relay` | float | ÷ 10 | Grid-relay side voltage, V. |
| 1625 | `v_inverter_relay` | float | ÷ 10 | Inverter-relay side voltage, V. |
| 1627–1631 | `first_inverter_serial_number` | str | `serial` | 5 regs = 10 Latin-1 chars. The primary AIO serial. (1626 unused/padding.) |

### 4.2 Daily ("today") energy — IR 1640–1657

All `÷ 10` → kWh. Identical in V1 and V2 (only the *totals* swap).

| Reg | Field | Meaning |
|---|---|---|
| 1640 | `e_grid_import_today` | Grid import today |
| 1643 | `e_pv_today` | PV generation today |
| 1646 | `e_grid_export_today` | Grid export today |
| 1649 | `e_aio_charge_today` | AIO battery charge today (aggregate) |
| 1652 | `e_aio_discharge_today` | AIO battery discharge today (aggregate) |
| 1655 | `e_load_today` | House load today (excludes EV) |

### 4.3 Lifetime energy totals — IR 1641–1657 (V1/V2 byte-order swap!)

`uint32`, `÷ 10` → kWh. **Register pair order differs by variant (§3).**

| Field | V1 regs (high, low) | V2 regs (high, low) |
|---|---|---|
| `e_grid_import_total` | 1641, 1642 | 1642, 1641 |
| `e_pv_total` | 1644, 1645 | 1645, 1644 |
| `e_grid_export_total` | 1647, 1648 | 1648, 1647 |
| `e_aio_charge_total` | 1650, 1651 | 1651, 1650 |
| `e_aio_discharge_total` | 1653, 1654 | 1654, 1653 |
| `e_load_total` | 1656, 1657 | 1657, 1656 |

> Worked example (from `test_gateway_energy_totals_v1`): V1 with IR(1641)=1, IR(1642)=0
> → `(1<<16)+0 = 65536` raw → `65536 / 10 = 6553.6 kWh`. V2 swaps the pair so the same
> physical quantity is encoded with IR(1642) as the high word.

### 4.4 AIO summary — IR 1700–1704

| Reg | Field | Type | Meaning |
|---|---|---|---|
| 1700 | `parallel_aio_num` | uint16 | Number of AIOs configured in the stack (1–3 observed). |
| 1701 | `parallel_aio_online_num` | uint16 | Number of AIOs currently online. |
| 1702 | `p_aio_total` | int16 | Aggregate AIO inverter power, W (signed). |
| 1703 | `aio_state` | enum | Battery `State` enum (charge/discharge/idle…). |
| 1704 | `battery_firmware_version` | uint16 | Aggregate battery firmware rev. |

### 4.5 Per-AIO daily charge — IR 1705–1713

`÷ 10` → kWh. (Discharge per-AIO is at 1750+, see 4.7.)

| Reg | Field |
|---|---|
| 1705 | `e_aio1_charge_today` |
| 1708 | `e_aio2_charge_today` |
| 1711 | `e_aio3_charge_today` |

### 4.6 Per-AIO charge totals — IR 1706–1713 (V1/V2 swap)

`uint32`, `÷ 10` → kWh.

| Field | V1 (high, low) | V2 (high, low) |
|---|---|---|
| `e_aio1_charge_total` | 1706, 1707 | 1707, 1706 |
| `e_aio2_charge_total` | 1709, 1710 | 1710, 1709 |
| `e_aio3_charge_total` | 1712, 1713 | 1713, 1712 |

### 4.7 Per-AIO daily discharge — IR 1750–1758

`÷ 10` → kWh.

| Reg | Field |
|---|---|
| 1750 | `e_aio1_discharge_today` |
| 1753 | `e_aio2_discharge_today` |
| 1756 | `e_aio3_discharge_today` |

### 4.8 Per-AIO discharge totals — IR 1751–1758 (V1/V2 swap)

`uint32`, `÷ 10` → kWh.

| Field | V1 (high, low) | V2 (high, low) |
|---|---|---|
| `e_aio1_discharge_total` | 1751, 1752 | 1752, 1751 |
| `e_aio2_discharge_total` | 1754, 1755 | 1755, 1754 |
| `e_aio3_discharge_total` | 1757, 1758 | 1758, 1757 |

### 4.9 Battery (aggregate) energy + per-AIO SOC — IR 1795–1803

| Reg(s) | Field | Type | Notes |
|---|---|---|---|
| 1795 | `e_battery_charge_today` | ÷ 10 kWh | Aggregate battery charge today. |
| 1796–1797 | `e_battery_charge_total` | uint32 ÷ 10 kWh | V1=(1796,1797); **V2 swapped=(1797,1796)**. |
| 1798 | `e_battery_discharge_today` | ÷ 10 kWh | Aggregate battery discharge today. |
| 1799–1800 | `e_battery_discharge_total` | uint32 ÷ 10 kWh | V1=(1799,1800); **V2 swapped=(1800,1799)**. |
| 1801 | `aio1_soc` | uint16 % | 0–100. |
| 1802 | `aio2_soc` | uint16 % | 0–100. |
| 1803 | `aio3_soc` | uint16 % | 0–100. |

### 4.10 Per-AIO inverter power — IR 1816–1818

| Reg | Field | Type |
|---|---|---|
| 1816 | `p_aio1_inverter` | int16, W (signed) |
| 1817 | `p_aio2_inverter` | int16, W (signed) |
| 1818 | `p_aio3_inverter` | int16, W (signed) |

### 4.11 AIO serial numbers — IR 1831–1849 (variant-dependent!)

5 registers each, `serial` (Latin-1). **Addresses shift between V1 and V2.**

| Field | V1 regs | V2 regs |
|---|---|---|
| `aio1_serial_number` | 1831–1835 | 1841–1845 |
| `aio2_serial_number` | 1838–1842 | 1848–1852 |
| `aio3_serial_number` | 1845–1849 | 1855–1859 |

> Note the 7-register stride per AIO (5 data + 2 gap) in both variants.

### 4.12 Address ranges not in the gateway model

The gaps (e.g. 1605–1607, 1613–1615, 1626, 1632–1639, 1658–1699, 1714–1749, 1759–1794,
1804–1815, 1819–1830, 1850–1854 in V1) are **not decoded** by the reference model. A
simulator should serve **error responses** (the `0x12`-padding shape) or zeros for reads
into unmapped banks — *not* synthetic garbage — so bounds-checking clients behave
correctly. (See `mock_plant.py` `_read`: present device + absent bank → error response.)

## 5. Data types & converters (scaling reference)

From `dewet22/givenergy-modbus` `model/register.py` (`Converter`). All multi-byte is
**big-endian**.

| Converter | Decode | Used for |
|---|---|---|
| `uint16` | raw value | counts, SOC %, power (unsigned) |
| `int16` | two's complement: `v if v<0x8000 else v-0x10000` | signed power/current (`p_ac1`, `i_grid`, `p_aio*_inverter`, `p_liberty`) |
| `uint32(hi, lo)` | `(hi<<16) + lo` | energy totals — **hi/lo order is variant-dependent (§3)** |
| `deci` | `v / 10` | V, A, kWh throughout the gateway bank |
| `serial(r0..r4)` | concat big-endian bytes → Latin-1 string, strip `\0`, **upper-case** | serial numbers |
| `gateway_version(r0,r1,r2,r3)` | prefix = latin1 decode of r0|r1 bytes (strip NUL); digits = decimal string of each byte of r2,r3 → e.g. `GA` + `000009` | firmware string |

**`gateway_version` decode detail** (confirmed by `test_software_version_decoding`):
```
r0=0x4741 ('G','A'), r1=0x3030 ('0','0'), r2=0x0000, r3=9
→ prefix = "GA", digits = "00"+"00"+"09" = "GA000009"
```
For V2: r3=10 → digits of byte `0x0A`=`10` → `"GA0000010"`.

**Bounds checking.** Fields declare `min`/`max` (e.g. `v_grid` 0–500, `p_pv` ≤ 50000,
SOC 0–100). A client **suppresses to `None`** any post-conversion value outside bounds —
unless the entire raw register bank is zero (the hardware "unpopulated" sentinel). A
simulator should keep values in bounds to avoid being treated as corrupt.

## 6. `WorkMode` enum (IR 1604)

The gateway reuses the standard inverter `WorkMode` enum. `ON_GRID = 2` is confirmed by
`test_gateway_power_readings`. The full GE WorkMode set (from the inverter model / GivTCP
`modes`):

| Value | Mode | Typical meaning |
|---|---|---|
| 0 | Normal / Eco | Self-consumption, discharging to meet load |
| 1 | Eco (Paused) / Timed Demand | Battery paused or timed discharge to load |
| 2 | **On Grid** | On-grid normal operation (the gateway default) |
| 3 | Timed Export | Discharging to export |
| 4 | Unknown | — |
| 5 | Export (Paused) | Export paused |

> GivTCP's parallel list: `modes=["Eco","Eco (Paused)","Timed Demand","Timed Export","Unknown","Export (Paused)"]`. The exact int↔name mapping is firmware-fragile; **emit `2` (On Grid)** for a quiescent gateway simulator and treat any non-`None` decode as "valid gateway present."

## 7. Gateway fault bitmask (IR 1622–1623)

`gateway_fault_codes` is a 32-bit MSB-first bitmask decoded into a list of active fault
names. Source: `_gateway_fault_code` in `model/gateway.py`. **Bit 0 = MSB (bit 31 of the
u32), bit 31 = LSB.** `None` entries are reserved/unused.

| Bit | Fault name |
|---|---|
| 0 | Relay 1&2 bonding |
| 1 | Relay 3&4 bonding |
| 2 | Relay 1&2 disconnect |
| 3 | Relay 3&4 disconnect |
| 4 | AC over frequency 1 |
| 5 | AC under frequency 1 |
| 6 | AC over voltage 1 |
| 7 | AC under voltage 1 |
| 8 | AC over frequency 2 |
| 9 | AC under frequency 2 |
| 10 | AC over voltage 2 |
| 11 | AC under voltage 2 |
| 12 | *(reserved)* |
| 13 | No zero-point protection |
| 14 | Over quarter AC voltage |
| 15 | Under quarter AC voltage |
| 16 | Over AC voltage long-time |
| 17 | AC over frequency constant |
| 18 | AC under frequency constant |
| 19 | AC over voltage constant |
| 20–30 | *(reserved)* |
| 31 | Grid mode Off |

For a healthy simulator: IR(1622)=0, IR(1623)=0 → empty fault list.

## 8. Power & energy semantics

### 8.1 Sign conventions on the gateway bank

| Quantity | Reg | Sign |
|---|---|---|
| Grid current `i_grid` | 1609 | signed (int16) |
| AC1 / AIO inverter power `p_ac1`, `p_aio*_inverter` | 1616, 1816–1818 | signed (int16); **+ = discharging/out, − = charging/in** (GE inverter convention) |
| Liberty / smart load `p_liberty` | 1619 | signed (int16) |
| PV power `p_pv` | 1617 | unsigned |
| Load power `p_load` | 1618 | unsigned; **excludes EV charger** (key gateway property) |

### 8.2 Aggregation model

The gateway **sums** across its child AIO(s):

```
p_aio_total      (1702) = Σ p_aioN_inverter        (1816..1818)
e_aio_charge_*            = Σ e_aioN_charge_*       (per-AIO regs)
aio_state        (1703)   = aggregate battery State enum
parallel_aio_num (1700)   = configured AIO count (1–3)
parallel_aio_online_num (1701) = online AIO count
```

Grid, PV, and load are measured by the gateway's **own** pre-installed meter — they are
*not* sums of AIO readings. This is why the gateway's `p_load` is authoritative (it sees
the true house load post-EVC-split) while an AIO's `p_load` double-counts the charger.

### 8.3 Energy-total monotonicity

All `*_total` registers are **lifetime-increasing** kWh (÷10). Daily counters reset at
midnight (local time). Clients mark these `state_class=total_increasing` so reset
detection is automatic. A simulator should never decrement a total.

## 9. Power-flow classification (shared spec)

When a client derives a human "what is the system doing" headline from the gateway's
instantaneous readings, the canonical decomposition (from `dewet22/givenergy-modbus`
`docs/flow-state-spec.md`, signed off by both the cli and hass frontends) is:

**Inputs (gateway equivalents):**
- `pv` ← `p_pv` (IR 1617), ≥ 0
- `grid` ← grid power; **+ = export, − = import** (derived from `i_grid`/`p_ac1`)
- `battery` ← `p_aio_total` (IR 1702); **+ = discharge, − = charge**

**Stateless decomposition** (`idle` threshold default 0.05 kW), solar prioritised:
```
solar_to_batt = min(solar_gen, batt_charge)
grid_to_batt  = min(grid_import, batt_charge - solar_to_batt)
solar_to_grid = min(solar_gen - solar_to_batt, grid_export)
batt_to_grid  = min(batt_discharge, grid_export - solar_to_grid)
solar_to_home = solar_gen - solar_to_batt - solar_to_grid
batt_to_home  = batt_discharge - batt_to_grid
grid_to_home  = grid_import - grid_to_batt
home          = solar_to_home + batt_to_home + grid_to_home
```
Residuals (`residual_charge`, `residual_export`) are **surfaced, never folded into an
edge**; |residual| > 0.1 kW flags "sensors disagree". Hysteresis is a frontend concern
(Schmitt 200 W on / 80 W off in hass); the core is stateless.

The 8-state headline collapse (grid > battery > solar precedence): `EXPORTING_SOLAR`,
`EXPORTING_BATTERY`, `IMPORTING`, `DISCHARGING`, `CHARGING_FROM_SOLAR`, `CHARGING`,
`SOLAR_COVERING_HOUSE`, `IDLE`. A gateway simulator need only produce consistent
`pv`/`grid`/`battery` magnitudes; the classification follows for free.

## 10. Polling strategy for a client

To fully populate a Gateway model, a client issues **Input Register reads (fc 0x04)** at
the gateway's slave address covering:

| Read | Range | Why |
|---|---|---|
| 1 | IR(1600–1631) | version, work mode, V/I/P, faults, first AIO serial |
| 2 | IR(1640–1657) | daily + lifetime energy (grid/pv/aio/load) |
| 3 | IR(1700–1713) | AIO summary + per-AIO charge (daily + total) |
| 4 | IR(1750–1758) | per-AIO discharge (daily + total) |
| 5 | IR(1795–1803) | battery aggregate energy + per-AIO SOC |
| 6 | IR(1816–1818) | per-AIO inverter power |
| 7 | IR(1831–1849) (V1) *or* IR(1841–1859) (V2) | per-AIO serials |

…plus the standard identity/config block HR(0–119) read that every GE device gets on
detect. Clients typically batch these into the fewest fc-0x04 requests the dongle will
answer in one transaction (GivEnergy dongles prefer ≤60-register reads; large reads cause
the timeout cascades documented in giv_tcp issue #471).

A **simulator** should answer all of the above with in-bounds, variant-consistent values,
and answer any read into an unmapped sub-range with an error response (not zeros) — see
§4.12.

## 11. What a faithful simulation must do

Derived from the above. This is the Phase 2 build checklist.

1. **Emit a `GW`-prefixed serial** on HR(13–17) so every client's prefix-based detection
   classifies the device as a Gateway. (Our current `Gateway12kW` emits a generic serial —
   the detection gap.)
2. **Set HR(0) = 0x7001** (already done) **and** populate IR(1600–1603) with a real
   version string (default `GA000009` → V1). Without the version string, clients treat the
   gateway as absent (`is_valid() == false`).
3. **Serve the full IR 1600–1859 aggregation bank** projected from the underlying plant
   state: grid/PV/load from a meter model, AIO power/SOC/energy summed across child
   inverters, faults as a bitmask.
4. **Zero direct batteries** on the gateway device itself (batteries belong to child AIOs).
5. **Honour the V1/V2 variant contract** as a single switch: byte order of all `uint32`
   totals *and* the AIO-serial addresses change together, gated on IR(1603).
6. **Route control writes** (charge/discharge enable, SOC target, slots) to the gateway as
   authoritative; per-AIO writes accepted but informational.
7. **Exclude EV-charger power from `p_load` / `e_load_*`** — the defining gateway property.
8. **Error-respond to unmapped banks** rather than serving zeros, so client bounds-checks
   behave.

### Minimum-viable first cut (single-AIO Gateway)

The dominant real topology (1 Gateway + 1 AIO — the case behind giv_tcp issues #364/#367):
- 1 child AIO state, so `parallel_aio_num = parallel_aio_online_num = 1`, `aio2/aio3` regs
  read zero + error on totals (or simply zeroed with bounds-suppression tolerated).
- `p_aio_total = p_aio1_inverter`, `aio_state` from the single AIO's battery.
- `aio1_soc` populated; `aio2_soc`/`aio3_soc` = 0.
- This is enough to exercise every client detection + decode path without modelling
  parallel AIOs.

## 12. Sources

All register addresses, scalings, enums, and the V1/V2 split are taken from:

- **`dewet22/givenergy-modbus`** (Python, Apache-2.0) — the client library used by the
  `givenergy-hass` / `givenergy-local` Home Assistant integration:
  - `givenergy_modbus/model/gateway.py` — the gateway model, `_GATEWAY_COMMON_LUT`,
    `_GATEWAY_V{1,2}_ENERGY_TOTALS`, `_GATEWAY_V{1,2}_SERIALS`, `select_gateway()`,
    `_gateway_fault_code`.
  - `givenergy_modbus/model/register.py` — `Converter` (deci/int16/uint32/serial/
    gateway_version), `IR`/`HR`/`Def`, bounds-suppression rules.
  - `givenergy_modbus/model/devices.py` — `DeviceType.GATEWAY`, plant-device graph.
  - `givenergy_modbus/client/commands.py` — `RegisterMap`, write-safe register sets,
    capability-aware polling (`detect()` → `load_config()`/`refresh()`).
  - `givenergy_modbus/testing/mock_plant.py` — the in-memory plant server pattern
    (error-on-absent-bank, serial stamping) to mirror in Rust.
  - `tests/model/test_gateway.py` — golden values pinning every decode (version strings,
    byte-order swap, serial addresses, SOC, power readings).
  - `docs/flow-state-spec.md` — the shared power-flow classification spec.
  - `docs/hardware-firmware-quirks.md` — firmware-gating / unit-fragility caveats.
- **`britkat1980/giv_tcp`** (Python) — the definitive protocol reference:
  - `GivTCP/givenergy_modbus_async/model/gateway.py`, `read.py` (the `power_flow_output`
    gateway processing around the notorious line-1693 bug), `GivLUT.py` (device class
    strings: `"Gen1 - Gateway with 0 batteries"`).
  - Issues #364, #367 (the single-AIO Gateway `power_flow_output` crash), #471 (dongle
    read-load / timeout cascades).
- **GivEnergy datasheets** — `givenergy.com/resource-hub/datasheets/gateway-datasheet/`
  (GIV-AIO-GW1) and `gateway-2-datasheet/` (GIV-AIO-GW1-2) for the physical specs in §1.
- **`jak/givenergy-modbus`** (TypeScript) — `src/generation.ts`, `identify()` — confirms
  serial-prefix-based generation detection (the JS client's `GW`-aware equivalent).

---

*Phase 1 research deliverable for the GivEnergy Plant Simulator. Feeds directly into the
Phase 2 gateway simulation build and into client-side Gateway support.*
