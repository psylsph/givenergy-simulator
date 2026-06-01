//! Device model trait and shared types.
//!
//! All device models (solar, load, battery, inverter) implement
//! [`DeviceModel`], called once per simulation tick.

use chrono::NaiveDateTime;

/// Context provided to every device model on each tick.
pub struct TickContext {
    /// Current simulation timestamp.
    pub now: NaiveDateTime,
    /// Tick duration in fractional hours (e.g. 0.008_333 for 30 s).
    pub dt_hours: f64,
}

/// A pluggable device model.
///
/// Implementors update their internal state based on the tick context.
/// The simulation core calls `update` on every registered device each tick.
pub trait DeviceModel {
    /// Advance the model by one tick.
    fn update(&mut self, ctx: &TickContext);
}
