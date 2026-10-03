//! Matrix protocol limits used by core configuration.
//!
//! Defining them in core lets configuration read them without depending on
//! the Matrix crate.

/// The [maximum length allowed] for the `prev_events` array of a PDU.
/// [maximum length allowed]: <https://spec.matrix.org/latest/rooms/v1/#event-format>
pub const MAX_PREV_EVENTS: usize = 20;
