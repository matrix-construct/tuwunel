//! Core Matrix event and room-version types.
//!
//! The module exposes the shared event model, PDU storage types, and
//! room-version rule lookup used throughout the server. Short identifiers are
//! compact database surrogates for Matrix identifiers.
#![deny(missing_docs)]

tuwunel_core::rustc_flags_capture! {}

pub mod event;
pub mod event_id;
pub mod pdu;
pub mod room_version;

pub use event::{Event, StateKey, TypeExt as EventTypeExt, TypeStateKey, state_key};
pub use pdu::{EventHash, Pdu, PduBuilder, PduCount, PduEvent, PduId, RawPduId};
pub use room_version::{RoomVersion, RoomVersionRules};

/// Compact database identifier for an event type and state-key pair.
///
/// Values are allocated by the short-state-key service and have meaning only
/// within the local database. They are not Matrix protocol identifiers.
pub type ShortStateKey = ShortId;

/// Compact database identifier for a Matrix event ID.
///
/// Values are allocated by the short-event-ID service and have meaning only
/// within the local database. They are not Matrix protocol identifiers.
pub type ShortEventId = ShortId;

/// Compact database identifier for a Matrix room ID.
///
/// Values are allocated by the short-room-ID service and have meaning only
/// within the local database. They are not Matrix protocol identifiers.
pub type ShortRoomId = ShortId;

/// Integer storage shared by the database's compact Matrix identifiers.
///
/// The allocation services draw from one global counter. Each alias selects the
/// database mapping in which a value is interpreted.
pub type ShortId = u64;
