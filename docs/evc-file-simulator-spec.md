# Specification — File-Driven GivEVC Modbus Simulator

**Goal:** Build a standalone service that emulates a **GivEnergy Electric Vehicle
Charger (GivEVC)** on the Modbus wire, but instead of simulating charging
physics, it serves **live values read from a JSON file**. The file is re-read on
a fixed interval (default 30 s), so an external process feeding telemetry from a
*different* type of charger (e.g. an OCPP backend, a vendor API, MQTT, or a
manual edit) can drive what every connected GivEnergy/GivTCP client sees.

This spec is derived from the existing in-repo EVC implementation
(`crates/sim-modbus/src/lib.rs` → `run_evc_modbus_server`, and
`crates/sim-models/src/lib.rs` → `EvcState`), which is a known-good reference
that real GivEnergy clients already talk to. Build to match it exactly on the
wire.

---

## 1. TL;DR for the implementer

* Serve **standard Modbus TCP** (NOT the GivEnergy proprietary envelope used on
  the inverter port 8899). Protocol ID `0x0000`, big-endian, no framing tricks.
* Listen on a configurable TCP port — **default 5020** (unprivileged, matches
  this simulator). Real GivEVC hardware uses **502**; see §3.
* Expose **115 holding registers, HR 0–114**. Two reads cover the whole map:
  `read_holding_registers(0, 60)` and `read_holding_registers(60, 55)`.
* Function codes: **0x03** (read holding), **0x06** (write single), **0x10**
  (write multiple). Anything else → Modbus exception `0x01`.
* Source of truth = a JSON file. A background task reloads it every 30 s into an
  in-memory state; every client read returns the *currently loaded* snapshot.
* Writes are **acknowledged but ephemeral** by default (kept in memory until the
  next file reload overwrites them). Two alternative policies are defined in §7.
* You can reuse the repo's `sim_modbus::run_evc_modbus_server` + `EvcState`
  verbatim and just swap the physics engine for a file loader (§10).

---

## 2. System architecture

```
                ┌──────────────────────────┐
  JSON file ──▶ │  File loader task         │  every 30 s:
  (telemetry)   │  parse → validate → swap  │  read file, build EvcState,
                └─────────────┬────────────┘   atomically replace shared state
                              │
                     Arc<Mutex<EvcState>>   (or Arc<RwLock<EvcState>>)
                              ▲                ▲
                              │ read           │ read/write (policy)
                ┌─────────────┴────────────────┴─────────────┐
                │  Modbus TCP server (port 5020)             │
                │  std framing, fn 0x03 / 0x06 / 0x10        │
                └─────────────┬──────────────────────────────┘
                              │
            one TCP connection per client (GivTCP, HA, app, test tool)
```

### 2.1 Concurrency rules

* The **file loader** is the *only* writer to the canonical state between
  reloads. It performs an **atomic swap** (build the new `EvcState` fully, then
  replace the pointer in one lock acquisition). Readers are never blocked on file
  I/O.
* The **Modbus handler** always reads the *current* snapshot under the lock and
  releases it before doing any network I/O. Never hold the state lock across an
  `await`/`read`/`write`.
* Each accepted TCP connection is handled in its own task/thread. Multiple
  concurrent clients must all work.
* If the file is missing or fails to parse, **keep the previous snapshot** and
  log a warning — do **not** zero the state and do **not** crash.

### 2.2 Recommended thread/task layout (async, Rust/Tokio)

```text
main:
  - parse env / config
  - state = Arc::new(RwLock::new(load_or_default(path)?))
  - spawn file_loader(state.clone(), path, interval)        // periodic reload
  - run modbus_server(state.clone(), bind_addr, port).await // accept loop
      per connection: spawn handle_conn(stream, state.clone())
```

---

## 3. Network & port

| Item | Value |
|---|---|
| Transport | TCP, listen on `0.0.0.0` (configurable) |
| Default port | **5020** (unprivileged; matches this simulator) |
| Real hardware port | **502** (standard Modbus). GivTCP connects to the EVC on **502** by default. |
| Using 502 on Linux | Requires root **or** `sudo setcap 'cap_net_bind_service=+ep' <binary>` |
| Framing | Standard Modbus TCP (MBAP header + PDU) |
| Heartbeat | **None.** Standard Modbus TCP is request/response; the proprietary 3-min heartbeat only applies to the inverter port 8899. Keep connections open until the client closes. |

> **Note for GivTCP users:** GivTCP's EVC read loop opens a connection to
> `<evc_ip>:502`. To point GivTCP at this simulator on 5020, either run the
> simulator on 502 (with the capability above) or override the EVC port in
> GivTCP's config. For a raw/custom client, 5020 is fine.

---

## 4. Building Modbus TCP packets from scratch (byte by byte)

Modbus TCP is **just bytes on a normal TCP socket**. No special library or
framing engine is required — you assemble a byte array, `send()` it, then
`recv()` the reply. This section shows exactly how, one byte at a time, and how
the simulator takes a value **read from the JSON file** and packs it into the
response.

There are two roles:

* **Client** (GivTCP / Home Assistant / your test tool) — sends a *request*, then
  reads a *response*.
* **Server** (this simulator) — reads a *request*, looks up the answer in the
  file-loaded state, and sends back a *response*.

Every packet, in either direction, has the same shape:

```
[ 7-byte header ][ function code (1 byte) ][ function-specific body ]
```

### 4.1 Rule #1 — every number is big-endian

"Big-endian" means the **most significant byte comes first**. A 16-bit value
(`u16`) occupies 2 bytes:

```
hi = (value >> 8) & 0xFF    lo = value & 0xFF
```

Worked conversions used throughout this section:

| Decimal | Hex value | Bytes on the wire |
|--------:|-----------|-------------------|
| 0       | 0x0000    | `00 00` |
| 1       | 0x0001    | `00 01` |
| 2       | 0x0002    | `00 02` |
| 6       | 0x0006    | `00 06` |
| 55      | 0x0037    | `00 37` |
| 60      | 0x003C    | `00 3C` |
| 91      | 0x005B    | `00 5B` |
| 95      | 0x005F    | `00 5F` |
| 97      | 0x0061    | `00 61` |
| 120     | 0x0078    | `00 78` |
| 123     | 0x007B    | `00 7B` |
| 160     | 0x00A0    | `00 A0` |
| 320     | 0x0140    | `01 40` |
| 2024    | 0x07E8    | `07 E8` |
| 2410    | 0x096A    | `09 6A` |

### 4.2 The 7-byte header (every packet starts with these 7 bytes)

```
byte offset  field           size  meaning
0–1          Transaction ID   2     any value; the server copies it into the reply
2–3          Protocol ID      2     ALWAYS 00 00 for standard Modbus TCP
4–5          Length           2     byte count of everything AFTER this field
6            Unit ID          1     any value; the server echoes it back (usually 01)
```

So after the header, byte offset **7** is always the **function code**
(`03`, `06`, or `10`), and the body follows.

#### The Length field — the one thing everyone gets wrong

`Length` counts **every byte after the Length field itself**, i.e.
`Unit ID (1) + function code (1) + body`. The header is 7 bytes, so the
**total packet size is always `6 + Length`**.

```
Length  = 1 (unit) + 1 (function) + body_byte_count
Total   = 6 + Length
```

* If the Protocol ID is anything other than `00 00`, the frame is **not
  Modbus TCP** — silently ignore it (do not reply).
* The server does **not** check the Unit ID; it copies whatever value it received
  into the response. (Real GivEVC is unit `01`.)

### 4.3 Reading a packet off the wire (TCP is a byte stream)

TCP gives you a stream of bytes, not discrete messages. One `send()` may arrive
as several `recv()`s, and several sends may merge into one recv. Always frame
using the Length field:

```
1. Buffer incoming bytes.
2. Once you have >= 7 bytes: parse Transaction(2) Protocol(2) Length(2) Unit(1).
3. total = 6 + Length.
4. Keep reading until the buffer holds `total` bytes.
5. Process that frame; remove it from the buffer; loop for any pipelined frame.
```

Never assume one `recv()` equals one packet.

### 4.4 Packet type 1 — Read Holding Registers (function 0x03)

Fetch `quantity` consecutive registers starting at `start_addr`.

**Request body** = `function(1) + start_addr(2) + quantity(2)`
`Length = 1 + 1 + 4 = 6`, so the request is always **12 bytes**.

#### Worked example A — "read HR 0 for 60 registers" (1st standard read)

`start_addr = 0` → `00 00`, `quantity = 60` → `00 3C`.

```
00 01   Transaction ID  = 1
00 00   Protocol ID     = 0 (Modbus)
00 06   Length          = 6
01      Unit ID         = 1
03      Function        = Read Holding Registers
00 00   Start address   = 0
00 3C   Quantity        = 60
```
Wire bytes: `00 01 00 00 00 06 01 03 00 00 00 3C`

#### Worked example B — "read HR 60 for 55 registers" (2nd standard read)

`start_addr = 60` → `00 3C`, `quantity = 55` → `00 37`.

```
00 01 00 00 00 06 01 03 00 3C 00 37
```

Together, examples A and B read the whole 115-register map.

**Validation** (reply with exception 0x02 if any is true):

* `quantity == 0`
* `start_addr + quantity > 115`

**Response** = header + `function(03) + byte_count(1) + data(quantity*2)`
where `byte_count = quantity * 2` and `Length = 3 + quantity*2`.

Response for `quantity = 60`: `byte_count = 120 (0x78)`, `Length = 123 (0x007B)`.

```
00 01   Transaction ID
00 00   Protocol ID
00 7B   Length          = 123
01      Unit ID
03      Function        = Read Holding Registers
78      Byte count      = 120
<120 data bytes: HR[start], HR[start+1], ... each 2 bytes big-endian>
```
Total response = `6 + 123 = 129` bytes.

#### Decoding a register out of the response

The data region begins at **byte offset 9** (7-byte header + function +
byte_count). Register `start + i` lives at bytes `9 + 2*i` and `10 + 2*i`:

```
HR[start + i] = (buf[9 + 2*i] << 8) | buf[10 + 2*i]
```

Example: you read starting at 0 and want `current_l1` (HR 6). `i = 6`, so read
bytes `9 + 12 = 21` and `22`. If they are `01 40`, the value is `0x0140 = 320`,
and ÷10 = **32.0 A**.

### 4.5 The file → bytes pipeline (how the server fills the response)

This is the heart of "take data from a file and byte-pack it". For every read
the simulator does the same five steps:

1. **Load file** (every 30 s, see §7): JSON `"current_a": { "l1": 32.0 }`.
2. **Project to raw u16**: HR 6 = `round(32.0 × 10) = 320` (the `÷10 Amps`
   scale from §5). Stored in the in-memory register array at index 6.
3. **Client requests** a read that covers HR 6.
4. **Encode big-endian**: `320 → [0x01, 0x40]`.
5. **Build the response frame** from §4.4 and send it.

So a one-register read of HR 6 (qty 1) produces:

```
00 01 00 00 00 05 01 03 02 01 40
            ^^^^ Length=5   ^^ byte_count=2   ^^^^^ HR6=320
```

The projection scales (step 2) are defined per-register in §5; the important
ones for packing:

| JSON field | Register | Scale to raw u16 |
|---|---|---|
| `current_a.l*` (A) | 6 / 8 / 10 | `round(A × 10)` |
| `voltage_v.l*` (V) | 109 / 111 / 113 | `round(V × 10)` |
| `meter_energy_kwh` (kWh) | 29 | `round(kWh × 10)` |
| `charge_limit_a` (A) | 36 | `round(A × 10)` |
| `active_power_w.*` (W) | 13 / 17 / 20 / 24 | `round(W)` (no scale) |
| `session_energy_kwh` (kWh) | 72 | `int(kWh)` (truncated) |
| `session_duration_s` (s) | 79 | `s & 0xFFFF` (low 16 bits) |
| `serial_number` (string) | 38–68 | one ASCII char per register, NUL-padded |
| enum/int fields | 0/2/4/32/34/91/93/94/95 | value as-is |

### 4.6 Packet type 2 — Write Single Register (function 0x06)

Write one register. **Request body** = `function(06) + addr(2) + value(2)`.
`Length = 6`, request is always **12 bytes**. The response is a **byte-for-byte
echo** of the request (same 12 bytes).

#### Worked example C — "start charging" → write value 1 to HR 95

`addr = 95` → `00 5F`, `value = 1` → `00 01`.

```
00 01 00 00 00 06 01 06 00 5F 00 01
```
Expected reply: **identical** `00 01 00 00 00 06 01 06 00 5F 00 01`.

#### Worked example D — "set charge current to 16 A"

The charge-current write register (HR 91) stores deci-Amps, so `16.0 A × 10 =
160` → `00 A0`, address `91` → `00 5B`.

```
00 01 00 00 00 06 01 06 00 5B 00 A0
```

**Validation:** `addr >= 115` → exception 0x02.

### 4.7 Packet type 3 — Write Multiple Registers (function 0x10)

Write `quantity` consecutive registers. **Request body** =
`function(10) + addr(2) + quantity(2) + byte_count(1) + values(2*quantity)`,
where `byte_count = quantity * 2`. `Length = 7 + 2*quantity`.

#### Worked example E — write HR 97,98 (system time, accepted & ignored)

Write year `2024` and month `6`: `addr = 97` → `00 61`, `quantity = 2` →
`00 02`, `byte_count = 4` → `04`, values `2024` → `07 E8`, `6` → `00 06`.
`Length = 7 + 4 = 11` → `00 0B`.

```
00 01 00 00 00 0B 01 10 00 61 00 02 04 07 E8 00 06
            ^^^^ Length=11                       ^^^^^^^^^^ 2 values
```
Total = `6 + 11 = 17` bytes.

**Response** = header + `function(10) + addr(2) + quantity(2)`. `Length = 6`.

```
00 01 00 00 00 06 01 10 00 61 00 02
```

**Validation:**

* `quantity == 0` or `addr + quantity > 115` → exception 0x02
* `byte_count != quantity * 2` → exception 0x03

### 4.8 Exception (error) packets

When the request is invalid or unsupported, reply with an exception frame:
**normal 7-byte header** (`Length = 3`) + `function | 0x80` + `exception code
(1)`. Total = **9 bytes**.

```
00 01 00 00 00 03 01 83 02
                       ^^    0x03 | 0x80 = 0x83 (read with error)
                          ^^ exception code 0x02
```

| Code | Name | Send when |
|------|------|-----------|
| `0x01` | Illegal Function | function code is not 0x03 / 0x06 / 0x10 |
| `0x02` | Illegal Data Address | address or quantity outside HR 0–114, or quantity 0 |
| `0x03` | Illegal Data Value | malformed body (e.g. 0x10 `byte_count` ≠ `quantity×2`) |

### 4.9 Compact packet-template reference

`N` = quantity. All values big-endian. `TID`/`UID` are copied to the reply.

| Function | Request bytes | Response bytes |
|---|---|---|
| Read (0x03) | `TID 00 00 00 06 UID 03 [start:2] [qty:2]` | `TID 00 00 [L:2] UID 03 [N*2:1] [data:N*2]` where L = 3+N*2 |
| Write one (0x06) | `TID 00 00 00 06 UID 06 [addr:2] [value:2]` | identical echo of the request |
| Write many (0x10) | `TID 00 00 [L:2] UID 10 [addr:2] [qty:2] [N*2:1] [data:N*2]` where L = 7+N*2 | `TID 00 00 00 06 UID 10 [addr:2] [qty:2]` |
| Exception | — | `TID 00 00 00 03 UID [func\|0x80] [code:1]` |

### 4.10 Minimal pseudocode (no Modbus library needed)

```
# ---- build a Read request ----
def read_request(tid, uid, start, qty):
    return bytes([
        tid >> 8, tid & 0xFF,     # transaction id
        0x00, 0x00,               # protocol id (Modbus)
        0x00, 0x06,               # length (always 6 for read)
        uid,                      # unit id
        0x03,                     # function: read holding registers
        start >> 8, start & 0xFF, # start address
        qty >> 8,   qty   & 0xFF, # quantity
    ])

# ---- build a Write-Single request ----
def write_single_request(tid, uid, addr, value):
    return bytes([
        tid >> 8, tid & 0xFF, 0x00, 0x00, 0x00, 0x06, uid, 0x06,
        addr  >> 8, addr  & 0xFF,
        value >> 8, value & 0xFF,
    ])

# ---- decode the Read response ----
def decode_read_response(buf, start):
    assert buf[7] == 0x03                 # function echoed
    byte_count = buf[8]
    qty = byte_count // 2
    regs = {}
    for i in range(qty):
        regs[start + i] = (buf[9 + 2*i] << 8) | buf[10 + 2*i]
    return regs

# ---- server side: pack a file value into the response data region ----
# current_a.l1 = 32.0 (Amps, from JSON) -> HR 6
raw = round(32.0 * 10)                    # 320 (deci-amps)
data_bytes = bytes([raw >> 8, raw & 0xFF]) # 01 40
```

> Reference: `evc_state_to_registers`, `registers_to_evc`, `build_evc_error`,
> and `run_evc_modbus_server` in `crates/sim-modbus/src/lib.rs` implement
> exactly the packet shapes above.

---

## 5. Register map (HR 0–114)

Every register is a `u16`. Addresses not listed below read as **0**. Scaling
columns show how a human JSON value becomes the raw `u16` ("÷10" = the JSON value
in real units is multiplied by 10 before storage; "÷10 Amps" means raw÷10 = Amps).

| HR | Name | R/W | Encoding / scale | JSON unit | Notes |
|----|------|-----|------------------|-----------|-------|
| 0 | `charging_state` | R | enum u16 | — | See §5.2 enum |
| 1 | _reserved_ | R | 0 | — | |
| 2 | `connection_status` | R | 0/1 | — | 0=Not Connected, 1=Connected |
| 3 | _reserved_ | R | 0 | — | |
| 4 | `error_code` | R | enum u16 | — | 0=Clear, 11=CP voltage abnormal, … |
| 5 | _reserved_ | R | 0 | — | |
| 6 | `current_l1` | R | ÷10 Amps (raw = A×10) | A | deci-Amps |
| 7 | _reserved_ | R | 0 | — | |
| 8 | `current_l2` | R | ÷10 Amps | A | |
| 9 | _reserved_ | R | 0 | — | |
| 10 | `current_l3` | R | ÷10 Amps | A | |
| 11–12 | _reserved_ | R | 0 | — | |
| 13 | `active_power` | R | Watts (raw = W) | W | total |
| 14–16 | _reserved_ | R | 0 | — | |
| 17 | `active_power_l1` | R | Watts | W | |
| 18–19 | _reserved_ | R | 0 | — | |
| 20 | `active_power_l2` | R | Watts | W | |
| 21–23 | _reserved_ | R | 0 | — | |
| 24 | `active_power_l3` | R | Watts | W | |
| 25–28 | _reserved_ | R | 0 | — | |
| 29 | `meter_energy` | R | ÷10 kWh (raw = kWh×10) | kWh | cumulative meter total |
| 30–31 | _reserved_ | R | 0 | — | |
| 32 | `evse_max_current` | R | Amps (raw = A) | A | hardware max, e.g. 32 |
| 33 | _reserved_ | R | 0 | — | |
| 34 | `evse_min_current` | R | Amps | A | hardware min, e.g. 6 |
| 35 | _reserved_ | R | 0 | — | |
| 36 | `charge_limit` | R | ÷10 Amps (raw = A×10) | A | configured charge current |
| 37 | _reserved_ | R | 0 | — | |
| 38–68 | `serial_number` | R | ASCII, **one char per register** (31 chars) | string | stop decoding at first 0x00 |
| 69–71 | _reserved_ | R | 0 | — | |
| 72 | `charge_session_energy` | R | integer kWh (`as u16`) | kWh | session total (see §9 limits) |
| 73–78 | _reserved_ | R | 0 | — | |
| 79 | `charge_session_duration` | R | seconds, **low 16 bits** (`& 0xFFFF`) | s | wraps at 65535 s (see §9) |
| 80–90 | _reserved_ | R | 0 | — | |
| 91 | `charge_current_limit` | **W** | deci-Amps (raw = A×10) | A | **write target**; clamp ≥60 (6.0 A) |
| 92 | _reserved_ | R/W | 0 | — | |
| 93 | `plug_and_go` | R/W | 0=enabled, 1=disabled | bool | see §5.3 |
| 94 | `charge_control` | R | enum u16 (0/1/2) | — | what the charger reports |
| 95 | `charge_control` (write) | **W** | enum u16 (0/1/2) | — | **write target** (see §5.4) |
| 96 | _reserved_ | R/W | 0 | — | |
| 97–102 | `system_time` | W | ignored | — | client clock writes; server discards |
| 103–108 | _reserved_ | R/W | 0 | — | |
| 109 | `voltage_l1` | R | ÷10 V (raw = V×10) | V | |
| 110 | _reserved_ | R | 0 | — | |
| 111 | `voltage_l2` | R | ÷10 V | V | |
| 112 | _reserved_ | R | 0 | — | |
| 113 | `voltage_l3` | R | ÷10 V | V | |
| 114 | _reserved_ | R | 0 | — | |

### 5.1 Critical read/write asymmetries (do not get these wrong)

These are **deliberate** because that is how GivTCP/the real charger behave:

1. **`charge_control` is read at HR 94 but written at HR 95.** A client writes
   Start/Stop to HR 95; everyone reads the reported state at HR 94.
2. **`charge_current_limit` is written at HR 91** (deci-Amps, raw = A×10),
   minimum 60 (= 6.0 A). It is *not* the same register as `charge_limit` (HR 36),
   which is the read-back of the currently active limit.
3. **`plug_and_go` is inverted**: register value **0 = enabled**, **1 = disabled**.
4. **`system_time` (HR 97–102) writes are accepted and discarded** — the
   simulator's clock is not driven by the client. Acknowledge the write normally.

### 5.2 `charging_state` enum (HR 0)

```
0  = Unknown
1  = Idle
2  = Connected
3  = Starting
4  = Charging
5  = Startup Failure
6  = End of Charging
7  = System Failure
8  = Scheduled
9  = Updating
10 = Unstable CP
```

When mapping telemetry from another charger, the most useful values are
**1 (Idle)**, **2 (Connected, not charging)**, **4 (Charging)**, and
**6 (End of Charging / finished)**. See §8 for a translation guide.

### 5.3 `plug_and_go` (HR 93)

JSON boolean `plug_and_go_enabled`:

* `true`  → register **0** (plug-and-go ON: vehicle starts charging on plug-in)
* `false` → register **1** (charging must be triggered by RFID / charge_control)

### 5.4 `charge_control` (HR 94 read / HR 95 write)

```
0 = Ready
1 = Start
2 = Stop
```

---

## 6. JSON state file format

The file is the **complete source of truth** for every served value. Use
**real-world units** in the JSON; the server applies the §5 scaling before
projecting to registers. Unknown/missing fields fall back to defaults (§6.3).

A top-level `registers` object (address → raw `u16`) is applied **last** and
**overrides** any computed value — use it for registers not covered by the
friendly fields, or for test fixtures.

### 6.1 Field reference

| JSON path | Type | Unit | Registers | Projection |
|---|---|---|---|---|
| `serial_number` | string (≤31 chars) | ASCII | 38–68 | one char per reg, pad 0 |
| `charging_state` | int 0–10 | enum | 0 | as-is |
| `connection_status` | int 0/1 | enum | 2 | as-is |
| `error_code` | int | enum | 4 | as-is |
| `current_a.l1` | number | A | 6 | ×10, clamp 0–65535 |
| `current_a.l2` | number | A | 8 | ×10 |
| `current_a.l3` | number | A | 10 | ×10 |
| `active_power_w.total` | number | W | 13 | as-is (u16) |
| `active_power_w.l1` | number | W | 17 | as-is |
| `active_power_w.l2` | number | W | 20 | as-is |
| `active_power_w.l3` | number | W | 24 | as-is |
| `meter_energy_kwh` | number | kWh | 29 | ×10 |
| `evse_max_current_a` | int | A | 32 | as-is |
| `evse_min_current_a` | int | A | 34 | as-is |
| `charge_limit_a` | number | A | 36 | ×10 |
| `session_energy_kwh` | number | kWh | 72 | truncate to int |
| `session_duration_s` | int | s | 79 | `& 0xFFFF` (low word) |
| `plug_and_go_enabled` | bool | — | 93 | true→0, false→1 |
| `charge_control` | int 0/1/2 | enum | 94 | as-is (reported state) |
| `voltage_v.l1` | number | V | 109 | ×10 |
| `voltage_v.l2` | number | V | 111 | ×10 |
| `voltage_v.l3` | number | V | 113 | ×10 |
| `charge_current_limit_a` | number | A | 91 | ×10 (write target echo; ≥6.0 A) |
| `registers` | object | raw u16 | any | override, applied last |

### 6.2 Example `evc_state.json` (a 32 A single-phase charge in progress)

```json
{
  "$schema": "giv-evc-state/v1",
  "serial_number": "11288853538258",
  "charging_state": 4,
  "connection_status": 1,
  "error_code": 0,
  "current_a": { "l1": 32.0, "l2": 0.0, "l3": 0.0 },
  "voltage_v": { "l1": 241.0, "l2": 0.0, "l3": 0.0 },
  "active_power_w": { "total": 7712, "l1": 7712, "l2": 0, "l3": 0 },
  "meter_energy_kwh": 1234.5,
  "session_energy_kwh": 12.7,
  "session_duration_s": 4530,
  "evse_max_current_a": 32,
  "evse_min_current_a": 6,
  "charge_limit_a": 32.0,
  "charge_current_limit_a": 32.0,
  "plug_and_go_enabled": true,
  "charge_control": 0,
  "registers": {}
}
```

What a client reading HR 0–114 would then observe (selected):

```
HR 0  = 4       (Charging)
HR 2  = 1       (Connected)
HR 6  = 320     (32.0 A)
HR 13 = 7712    (W)
HR 29 = 12345   (1234.5 kWh)
HR 36 = 320     (32.0 A)
HR 38..68 = "11288853538258" + zeros
HR 72 = 12      (12.7 → truncated integer kWh; see §9)
HR 79 = 4530
HR 91 = 320
HR 93 = 0       (plug-and-go enabled)
HR 94 = 0       (Ready)
HR 109 = 2410   (241.0 V)
```

### 6.3 Defaults (applied when a field is absent or the file is missing)

Match `EvcState::default()` so a brand-new install looks sane before any
telemetry arrives:

```json
{
  "serial_number": "11288853538258",
  "charging_state": 1,
  "connection_status": 0,
  "error_code": 0,
  "current_a": { "l1": 0, "l2": 0, "l3": 0 },
  "voltage_v": { "l1": 241.0, "l2": 241.0, "l3": 241.0 },
  "active_power_w": { "total": 0, "l1": 0, "l2": 0, "l3": 0 },
  "meter_energy_kwh": 1234.5,
  "session_energy_kwh": 0,
  "session_duration_s": 0,
  "evse_max_current_a": 32,
  "evse_min_current_a": 6,
  "charge_limit_a": 32.0,
  "charge_current_limit_a": 32.0,
  "plug_and_go_enabled": true,
  "charge_control": 0
}
```

---

## 7. Refresh behaviour & write policy

### 7.1 File refresh

* Re-read the file every `EVC_REFRESH_SECONDS` (default **30**). Re-reads are
  triggered by a timer, not by client activity.
* **Atomic swap:** build the new `EvcState` from the parsed JSON, then replace
  the shared snapshot under one short lock. Reads always return a complete,
  consistent snapshot — never a half-updated one.
* **Failure handling:** if the file is missing, unreadable, or fails JSON
  validation, **keep the previous snapshot**, log a warning with the error, and
  try again next interval. Do not zero state, do not exit.
* **Optional optimisation:** skip reparsing if the file mtime is unchanged since
  the last successful load.
* **Optional manual refresh:** reload immediately on `SIGHUP` (Unix) so an
  operator can force a refresh without waiting 30 s.

### 7.2 Write policy (how client writes are handled)

Because the file represents telemetry from a *different* charger, control writes
(start/stop, set current, plug-and-go) cannot reach that charger unless the
integrator wires up a translation. Pick one policy via `EVC_WRITE_POLICY`:

| Policy | Behaviour | Use when |
|---|---|---|
| `acknowledge` *(default)* | Accept the write into the in-memory snapshot and return the normal Modbus success response. The value is served to subsequent reads **until the next 30 s reload overwrites it** with the file's value. | You want the client's UI to "work" (commands acknowledged) but the file remains the source of truth. |
| `reject` | Return Modbus exception `0x02` for **every** write (fn 0x06 / 0x10), at any address. The server is effectively read-only. | Pure monitoring; you never want client commands to perturb the served state. |
| `passthrough` | Apply the write to memory **and** merge it back into the JSON file on disk (preserve all other fields), so the external feeder can read the commanded value and act on the real charger. | You have a feeder loop that can translate GivEVC commands into the other charger's API. |

For `acknowledge` and `passthrough`, honour the same write semantics as the
reference server (§5.1): writes to **HR 91** set `charge_current_limit` (clamp
≥ 60 / 6.0 A), **HR 93** toggles `plug_and_go`, **HR 95** sets `charge_control`,
and **HR 97–102** (time) are accepted and ignored. Writes to any other address
are accepted and stored as raw `u16` into the snapshot (they will appear on the
next read until overwritten).

---

## 8. Feeding it from a *different* type of charger

This is the whole point. The external feeder (your script/service that talks to
the real charger) writes `evc_state.json` whenever it has fresh data; the
simulator picks it up within 30 s. Map the other charger's state onto the JSON
fields like this:

| GivEVC concept | Typical source from another charger |
|---|---|
| `connection_status` (1) | "vehicle connected / cable locked" signal (OCPP `StatusNotification` connector state, or a pilot/CP state). |
| `charging_state` (4) | active charging session. Map: not plugged → **1**; plugged idle → **2**; charging → **4**; session finished → **6**; faulted → **7**. |
| `active_power_w.total` | live charging power (W). For 1-phase, put it all in `.l1` too. |
| `current_a.l1` | charging current (A). 3-phase: split across l1/l2/l3. |
| `voltage_v.l1` | supply voltage (V) (~230 single-phase, ~400 L-L three-phase → use phase-neutral ~230). |
| `session_energy_kwh` | energy delivered this session (kWh). |
| `session_duration_s` | seconds since session start. |
| `meter_energy_kwh` | the charger's lifetime meter (kWh); keep monotonically increasing. |
| `charge_limit_a` / `charge_current_limit_a` | the charger's configured max current (A), clamped 6–32. |
| `error_code` | map faults to the closest GivEVC code (0 = clear, 11 = CP abnormal, …). |
| `serial_number` | the real charger's serial (≤31 ASCII chars), or any stable ID. |

**Single-phase vs three-phase:** the GivEVC register map supports both. For a
1-phase charger, leave `l2`/`l3` currents and powers at 0 and carry everything on
L1. For 3-phase, distribute power/current across L1/L2/L3 and set `total` to the
sum.

**Example feeder loop (Python pseudo):**

```python
while True:
    real = poll_my_other_charger()          # OCPP / vendor API / MQTT
    snap = {
        "serial_number": real.serial,
        "connection_status": 1 if real.vehicle_connected else 0,
        "charging_state": 4 if real.charging else (2 if real.connected else 1),
        "active_power_w": {"total": real.power_w, "l1": real.power_w, "l2": 0, "l3": 0},
        "current_a": {"l1": real.current_a, "l2": 0.0, "l3": 0.0},
        "voltage_v": {"l1": real.voltage_v, "l2": 0.0, "l3": 0.0},
        "session_energy_kwh": real.session_kwh,
        "session_duration_s": int(real.session_seconds),
        "meter_energy_kwh": real.meter_total_kwh,
        "charge_limit_a": min(max(real.max_current_a, 6), 32),
        "charge_current_limit_a": min(max(real.max_current_a, 6), 32),
        "plug_and_go_enabled": True,
        "charge_control": 0,
        "error_code": 0,
    }
    atomic_write_json("evc_state.json", snap)  # write tmp + rename for atomicity
    sleep(15)                                   # faster than the 30 s server refresh
```

Write the file **atomically** (write to `evc_state.json.tmp` then `rename`) so the
server never reads a half-written file.

---

## 9. Known limitations / precision notes (match the reference exactly)

These are inherited from the proven in-repo implementation; replicate them rather
than "fixing" them silently, or a real client may decode differently:

* **`session_energy_kwh` (HR 72) is integer kWh** — the fractional part is
  truncated when projected. If you need 0.1 kWh resolution, encode it yourself
  via the `registers` override (e.g. put `kwh*10` at HR 72 and document it), but
  be aware the standard GivEVC decode treats HR 72 as integer kWh.
* **`session_duration_s` (HR 79) is a single 16-bit word** — it wraps at 65 535 s
  (~18.2 h). The reference only serves the low word.
* **All scalars are `u16`** — `active_power_w` caps at 65 535 W, currents at
  6553.5 A, etc. This is fine for a ≤22 kW charger but clamp on input.
* **Serial number** is truncated to 31 ASCII characters.
* **Registers HR 1,3,5,7,9,11,12,14–16,18,19,21–23,25–28,30,31,33,35,37,69–71,
  73–78,80–90,92,96,103–108,110,112,114** are reserved and always read 0.

---

## 10. Reusing this repository's implementation (fastest path)

You do not have to write the Modbus layer from scratch. The repo already
contains a known-good EVC Modbus server and a serialisable state struct:

* **`sim_modbus::run_evc_modbus_server(evc_state: Arc<Mutex<EvcState>>, port)`**
  — `crates/sim-modbus/src/lib.rs`. Handles 0x03/0x06/0x10, MBAP framing,
  exceptions, and the EACCES→5020 fallback. **Use this as-is.**
* **`sim_models::EvcState`** — `crates/sim-models/src/lib.rs`.
  `#[derive(Serialize, Deserialize)]`, so it round-trips JSON directly. Its field
  units are the *raw register units* (currents in deci-Amps, voltage in Volts,
  meter in kWh, etc.) — see the struct's doc comments.
* **`sim_modbus::evc_state_to_registers` / `registers_to_evc`** — the canonical
  projection functions. Mirror them if you implement your own.

### 10.1 Minimal change to make it file-driven

1. Stop running `EvcEngine` (the physics state machine) against `state.evc`.
2. Add a loader task that, every 30 s:
   ```rust
   let parsed: EvcState = serde_json::from_slice(&fs::read(&path)?)?;
   *state.write().await = parsed;   // atomic swap of the whole struct
   ```
   using a `watch`/`notify`-on-mtime optimisation if desired.
3. Keep `run_evc_modbus_server(evc_state, port)` exactly as is — it already reads
   from the shared `Arc<Mutex<EvcState>>` on every request, so it will
   transparently serve the file-loaded values.
4. For write policy, either leave the default (writes mutate the in-memory
   `EvcState` until the next reload) or wrap `registers_to_evc` to reject/persist
   per §7.2.

> If you prefer to keep using `EvcState`'s raw units in the JSON (no
> human-friendly scaling), you can serialise `EvcState` directly and skip the
> §6 projection table — just document that `current_*` are deci-Amps etc. The
> §6 schema above is the friendlier alternative for a human authoring the file.

---

## 11. Configuration (environment variables)

| Var | Default | Meaning |
|---|---|---|
| `EVC_STATE_FILE` | `./evc_state.json` | Path to the JSON state file |
| `EVC_PORT` | `5020` | TCP port to listen on (real hardware = 502) |
| `EVC_BIND_ADDR` | `0.0.0.0` | Bind address |
| `EVC_REFRESH_SECONDS` | `30` | File re-read interval |
| `EVC_UNIT_ID` | `0` (= accept any) | If non-zero, only respond when request Unit ID matches |
| `EVC_WRITE_POLICY` | `acknowledge` | `acknowledge` \| `reject` \| `passthrough` |
| `EVC_LOG_LEVEL` | `info` | `error`/`warn`/`info`/`debug` |

Startup sequence: load file (or defaults) → spawn refresh task → bind TCP →
serve. On bind failure with EACCES on a port < 1024 and no explicit override,
fall back to 5020 with a warning (matches the reference).

---

## 12. Acceptance / test plan

Implementer must verify all of the following before claiming done.

1. **Read the full map.** With any Modbus client (see snippet below), the two
   standard reads return 60 + 55 = 115 registers, values matching the JSON.
2. **Scaling.** JSON `current_a.l1 = 32.0` → HR 6 reads `320`. JSON
   `voltage_v.l1 = 241.0` → HR 109 reads `2410`. JSON `meter_energy_kwh = 1234.5`
   → HR 29 reads `12345`.
3. **Serial.** HR 38–68 decode to the JSON `serial_number` string, NUL-padded.
4. **Refresh.** Edit the JSON (e.g. change `charging_state` 1→4), wait ≤30 s, and
   confirm the next read reflects the new value with no restart.
5. **Atomicity.** During a reload, concurrent reads never return a mixed/half
   snapshot (hammer-read while reloading).
6. **Bad file.** Delete/corrupt the JSON; confirm the server keeps serving the
   last good snapshot and logs a warning (no crash, no zeroes).
7. **Writes (acknowledge).** Write HR 95 = 1 (Start); the 0x06 response echoes
   addr/value; a subsequent read of HR 95 returns 1 until the next reload.
8. **Writes (reject).** With `EVC_WRITE_POLICY=reject`, a write returns exception
   0x02.
9. **Out-of-range.** `read_holding_registers(110, 10)` → exception 0x02.
   `read_holding_registers(0, 0)` → exception 0x02.
10. **Bad function.** Send fn 0x01 → exception 0x01.
11. **Protocol ID.** Send a frame with Protocol ID 0x0001 → no response (ignored).
12. **Concurrent clients.** Two clients reading simultaneously both succeed.

### 12.1 Read test snippet (Python, pymodbus)

```python
from pymodbus.client import ModbusTcpClient

c = ModbusTcpClient("127.0.0.1", port=5020)
lo = c.read_holding_registers(address=0, count=60)   # HR 0-59
hi = c.read_holding_registers(address=60, count=55)  # HR 60-114
regs = lo.registers + hi.registers
assert len(regs) == 115
print("charging_state =", regs[0])
print("current_l1 (A)  =", regs[6] / 10)
print("voltage_l1 (V)  =", regs[109] / 10)
print("meter (kWh)     =", regs[29] / 10)
print("serial          =", "".join(chr(r) for r in regs[38:69] if r))
```

### 12.2 Raw TCP sanity check (no libraries)

```bash
# Read HR 0, count=60  (fn 0x03): TID=0001 PID=0000 LEN=0006 UID=01 FN=03 START=0000 QTY=003c
printf '\x00\x01\x00\x00\x00\x06\x01\x03\x00\x00\x00\x3c' | nc -q1 127.0.0.1 5020 | xxd | head
```

Expect a reply whose first bytes are `00 01 00 00 00 7b 01 03 78 …`
(`7b`=125 length, `78`=120 byte-count).

---

## 13. References

* In-repo reference implementation:
  `crates/sim-modbus/src/lib.rs` (`run_evc_modbus_server`, `evc_state_to_registers`,
  `registers_to_evc`, `build_evc_error`) and `crates/sim-models/src/lib.rs`
  (`EvcState`).
* Upstream wire-format source: `GivEnergy/giv_tcp`
  `givenergy_modbus/framer.py` (standard Modbus TCP MBAP header; Protocol ID
  `0x0000` distinguishes EVC from the proprietary inverter framing on 8899).
* Register semantics: GivTCP `evc.py` / `EVCLut.evc_lut` (the real charger's
  read-back values + HR 94 read / HR 95 write / HR 91 current-limit conventions).
* Real GivEVC: standard Modbus TCP on **port 502**; local control must be enabled
  in the GivEnergy portal ("My EV Charger → Settings → Other → Enable Local
  Control").
