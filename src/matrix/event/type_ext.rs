use ruma::events::{StateEventType, TimelineEventType};

use super::StateKey;

/// Convenience trait for adding event type plus state key to state maps.
pub trait TypeExt {
	/// Pairs this event type with an owned state key.
	///
	/// Timeline event types are converted to their state-event type spelling.
	/// Borrowed event types are cloned as needed for the returned pair.
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey);
}

impl TypeExt for StateEventType {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		(self, state_key.into())
	}
}

impl TypeExt for &StateEventType {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		(self.clone(), state_key.into())
	}
}

impl TypeExt for TimelineEventType {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		(self.to_cow_str().into(), state_key.into())
	}
}

impl TypeExt for &TimelineEventType {
	fn with_state_key(self, state_key: impl Into<StateKey>) -> (StateEventType, StateKey) {
		(self.to_cow_str().into(), state_key.into())
	}
}
