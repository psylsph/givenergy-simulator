# Battery Algorithms

SOC Formula

soc += ((charge_kw - discharge_kw) * tick_hours) / capacity_kwh

Constraints:
- Min SOC
- Max SOC
- Max Charge Rate
- Max Discharge Rate

Future:
Thermal model
Ageing model
Cell balancing
