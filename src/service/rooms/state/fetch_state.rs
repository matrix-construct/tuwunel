use std::collections::HashMap;

use ruma::{OwnedEventId, events::StateEventType};
use tuwunel_core::{
	Result, err,
	matrix::{PduEvent, ShortStateKey, StateKey},
};

use crate::{Services, rooms::state_res::FetchState};

#[cfg(test)]
mod tests;

/// Fetches state events named by a held map of short state keys.
///
/// An absent key is missing state. An event named by the map but absent from the
/// timeline is a storage failure.
#[derive(Clone, Copy)]
pub(crate) struct IdMapState<'a> {
	/// Services used to resolve short keys and read event storage.
	pub(crate) services: &'a Services,

	/// Held state event IDs indexed by their interned type and state key.
	pub(crate) ids: &'a HashMap<ShortStateKey, OwnedEventId>,
}

impl FetchState for IdMapState<'_> {
	type Pdu = PduEvent;

	async fn get(self, event_type: StateEventType, state_key: StateKey) -> Result<Self::Pdu> {
		let shortstatekey = self
			.services
			.short
			.get_shortstatekey(&event_type, state_key.as_str())
			.await?;

		let event_id = self
			.ids
			.get(&shortstatekey)
			.ok_or_else(|| err!(Request(NotFound("State key is absent from the held map."))))?;

		self.services
			.timeline
			.get_pdu(event_id)
			.await
			.map_err(|error| match error {
				| error if !error.is_not_found() => error,
				| _ => err!(Database("State map references missing event {event_id}.")),
			})
	}
}
