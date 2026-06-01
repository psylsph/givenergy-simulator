# Sequence Diagrams

Client -> ModbusServer
ModbusServer -> RegisterStore
RegisterStore -> PlantState

Write Path:

Client
 -> ModbusServer
 -> CommandQueue
 -> SimulationCore
 -> RegisterMapper
