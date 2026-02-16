//! Core Matrix Library

pub mod event;
pub mod pdu;
pub mod room_version;

pub use event::{Event, StateKey, TypeExt as EventTypeExt, TypeStateKey, state_key};
pub use pdu::{EventHash, Pdu, PduBuilder, PduCount, PduEvent, PduId, RawPduId};
pub use room_version::{RoomVersion, RoomVersionRules};

pub type ShortStateKey = ShortId;
pub type ShortEventId = ShortId;
pub type ShortRoomId = ShortId;
pub type ShortId = u64;
