# Rust Crate Design

crates/
sim-core
sim-models
sim-registers
sim-modbus
sim-scenarios
sim-faults
sim-recording
sim-api
sim-storage

## sim-core
Contains PlantState and tick scheduler.

## sim-models
Trait-based device implementation.

pub trait DeviceModel {
  fn update(&mut self, ctx: &TickContext);
}

## sim-registers
Register catalogue and mapping layer.
