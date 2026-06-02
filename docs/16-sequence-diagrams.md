# Sequence Diagrams

## Read Path (function code 0x03)
```
Client                     ModbusServer                RegisterStore              PlantState
  │                             │                            │                        │
  │── ReadHoldingRegisters ──▶  │                            │                        │
  │                             │── project_from_state() ──▶ │── reads state ──────▶  │
  │                             │                            │◀── returns values ────  │
  │                             │◀── returns projection ────  │                        │
  │◀── response with values ────│                            │                        │
```

## Write Path (function code 0x06)
```
Client           ModbusServer          RegisterStore    CommandSender    CLI/Tick Loop    SimulationCore
  │                    │                    │                │                │                │
  │── WriteSingle ──▶  │                    │                │                │                │
  │                    │── store.write() ──▶│                │                │                │
  │                    │◀── ok/err ────────  │                │                │                │
  │                    │── cmd_tx.send() ──────────────────▶ │                │                │
  │◀── response ───────│                    │                │                │                │
  │                                                          │                │                │
  │                                                          │── try_recv() ──▶                │
  │                                                          │                │── enqueue() ──▶  │
  │                                                          │                │                │── tick()
  │                                                          │                │                │── apply_commands()
```

## Simulation Tick
```
Tick Start            SolarEngine        LoadEngine      InverterEngine    FaultEngine      BatteryEngine    EnergyTracker
  │                        │                  │                │                │                 │                │
  │── apply_commands() ──▶ │                  │                │                │                 │                │
  │                        │                  │                │                │                 │                │
  │── update(solar) ─────▶ │                  │                │                │                 │                │
  │                        │── sets solar ────│                │                │                 │                │
  │                        │                  │                │                │                 │                │
  │── update(load) ────────│────────────────▶ │                │                │                 │                │
  │                        │                  │── sets load ──│                │                 │                │
  │                        │                  │                │                │                 │                │
  │── update(inverter) ────│─────────────────────────────────▶ │                │                 │                │
  │                        │                  │                │── sets power ──│── reads power ──│                │
  │                        │                  │                │── sets grid ───│                 │                │
  │                        │                  │                │                │                 │                │
  │── update(faults) ──────│──────────────────────────────────────────────────▶ │                 │                │
  │                        │                  │                │                │── modifies state│                │
  │                        │                  │                │                │                 │                │
  │── update(battery) ─────│───────────────────────────────────────────────────────────────────▶ │                │
  │                        │                  │                │                │                 │── soc update ──│
  │                        │                  │                │                │                 │── temp update  │
  │                        │                  │                │                │                 │── aging update │
  │                        │                  │                │                │                 │                │
  │── update(energy) ──────│───────────────────────────────────────────────────────────────────────────────────▶ │
  │                        │                  │                │                │                 │                │
  │── advance timestamp    │                  │                │                │                 │                │
```

## Multi-Day Scenario
```
Day 1                                    Day 2
  │ 06:00  08:00  10:00  12:00  ...  22:00  │ 06:00  08:00  10:00  ...  22:00
  │  │      │      │      │           │      │  │      │      │           │
  │──●──────●──────●──────●───────────●──────●──●──────●──────●───────────●──▶
     events repeat daily, dates offset by days
```

## Schedule Engine Interaction
```
Tick Start     ScheduleEngine (if registered)      InverterEngine
  │                    │                                  │
  │── update() ──────▶ │                                  │
  │                    │── checks charge/discharge window  │
  │                    │── may set mode to ForceCharge     │
  │                    │── or ForceDischarge               │
  │                    │                                  │
  │── update() ────────────────────────────────────────▶  │
  │                    │              reads inverter.mode  │
```
