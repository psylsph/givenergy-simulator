# State Model

PlantState
- timestamp
- inverter
- batteries
- solar
- load
- grid
- faults

State transitions occur only during simulation ticks.

All external writes become Commands.
Commands are applied between ticks.
