# Modbus Emulation

## Goals
- Match GivEnergy register addresses and scaling
- Support both read and write operations
- Forward writes as simulation Commands
- Deterministic: same Modbus input → same state change

## Status
- GivEnergy proprietary MBAP variant: ✅ implemented
- Inner function 0x03 (Read Holding Registers): ✅ implemented
- Inner function 0x04 (Read Input Registers): ✅ implemented
- Inner function 0x06 (Write Single Register): ✅ implemented
- CRC-16/Modbus on inner PDU: ✅ implemented
- Writable register → Command dispatch: ✅ implemented
- Modbus integration tests (7 real TCP): ✅ implemented
- Proper TCP buffering for partial reads: ✅ implemented
- Function code 0x10 (Write Multiple Registers): future

## Components
```
TCP Listener        — tokio::net::TcpListener, accepts concurrent connections
Session Manager     — per-connection tokio::spawn task
Request Decoder     — parses MBAP header + PDU
Register Store      — Arc<Mutex<RegisterStore>> shared with CLI
Write Dispatcher    — mpsc channel forwarding ModbusCommand → simulation
Command Translator  — modbus_command_to_sim() maps address+value to Command
```

## Write Path
```
Client → ModbusServer
  → WriteSingleRegister (fn 0x06)
    → RegisterStore::write() validates access
    → CommandSender::send(ModbusCommand{address, value})
    → CLI drains channel each tick
      → modbus_command_to_sim() → Command
        → SimulationEngine::enqueue()
          → applied before next tick
```

## Writables (addr → Command)
| Address | Register | Command |
|---|---|---|
| 100 | inverter_mode | SetInverterMode |
| 102 | inverter_export_limit_w | SetExportLimit |
| 210 | battery_min_soc | SetMinSoc |
| 211 | battery_max_soc | SetMaxSoc |
| 602 | config_weather | SetWeather |

## Integration Tests
4 tests covering: read registers, write single register (readwrite), write rejected (readonly), unsupported function code.

## Future
- Packet capture comparison against real GivEnergy hardware
- Function code 0x10 (Write Multiple Registers)
- Function code 0x04 (Read Input Registers)
