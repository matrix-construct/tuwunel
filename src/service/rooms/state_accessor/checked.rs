//! Check-only reads of room state.
//!
//! The live readers rebuild a room's state through typed decoders, an unchecked
//! slice and a cache of rebuilt chains, so a damaged row there panics, fails, or
//! hides behind a warm cache. Integrity checks resolve state through here
//! instead, from storage alone, where a damaged row is `Mapped::Damaged`.

use std::{collections::BTreeSet, str::from_utf8};

use futures::{TryFutureExt, TryStreamExt, stream::unfold};
use ruma::{EventId, OwnedEventId, RoomId, events::StateEventType};
use tuwunel_core::{Err, Result, implement, result::NotFound, utils::u64_from_bytes};

use super::Service;
use crate::{
	integrity::short_ids::state::{apply, layer, visit},
	rooms::{
		short::ShortStateHash,
		state_compressor::{CompressedState, compress_state_event, parse_compressed_state_event},
		timeline::Mapped,
	},
};

/// Finds the event one tuple holds in a room's current state, as
/// `room_state_get_id` does, from storage alone.
///
/// Every outcome is the one that function gives, except that damage it would
/// panic or fail on, or whose leading bytes a release build would accept, is
/// `Mapped::Damaged`, and a failed read keeps its own error where that function
/// wraps it. The state is rebuilt before the tuple's short id is read, so a
/// damaged state is reported even for a tuple without one.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub(crate) async fn room_state_get_id_checked(
	&self,
	room_id: &RoomId,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<Mapped<OwnedEventId>> {
	match self.room_state_checked(room_id).await? {
		| Mapped::Damaged => Ok(Mapped::Damaged),
		| Mapped::Event(state) =>
			self.state_get_id_checked(&state, event_type, state_key)
				.await,
	}
}

/// Rebuilds a room's current state as `room_state_get_id` does, from storage
/// alone.
///
/// A room with no current state fails as there. The state pointer and each diff
/// on its chain are decoded untrusted, so one that does not decode, a diff the
/// chain names but storage lacks, or a chain that loops is `Mapped::Damaged`,
/// where the live reader panics, fails, never returns, or in a release build
/// reads the leading bytes of a value longer than it expects. A failed read
/// keeps its own error, which the live reader wraps for a diff. The cache of
/// rebuilt chains is not read, so a warm one cannot hide damage.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub(crate) async fn room_state_checked(
	&self,
	room_id: &RoomId,
) -> Result<Mapped<CompressedState>> {
	let shortstatehash = self.services.db["roomid_shortstatehash"]
		.get(room_id)
		.map_ok(|value| u64_from_bytes(&value).ok())
		.await?;

	let Some(shortstatehash) = shortstatehash else {
		return Ok(Mapped::Damaged);
	};

	self.full_state_checked(shortstatehash).await
}

/// Rebuilds one state from its diff chain as `load_shortstatehash_info` does.
///
/// Each diff adds its entries to its parent's state and then takes its removed
/// ones away, while a root's added entries are the whole state.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
async fn full_state_checked(
	&self,
	shortstatehash: ShortStateHash,
) -> Result<Mapped<CompressedState>> {
	let db = &self.services.db;
	let chain = unfold((Some(shortstatehash), BTreeSet::new()), async |(next, visited)| {
		let next = next?;
		let diff = match visited.contains(&next) {
			| true => Ok(None),
			| false => layer(db, next).await,
		};

		let parent = diff
			.as_ref()
			.ok()
			.and_then(Option::as_ref)
			.and_then(|diff| diff.parent);

		Some((diff, (parent, visit(visited, next))))
	})
	.try_collect::<Vec<_>>()
	.await?;

	let state = chain
		.into_iter()
		.rev()
		.try_fold(CompressedState::new(), |state, diff| diff.map(|diff| apply(state, diff)));

	Ok(state.map_or(Mapped::Damaged, Mapped::Event))
}

/// Finds the event one tuple holds in a rebuilt state, as `state_get_id` does,
/// from storage alone.
///
/// A tuple without a short id, or one the state does not hold, fails as there. A
/// short id that does not decode, or a short event id with no event id behind it
/// or a damaged one, is `Mapped::Damaged`, where the live reader fails, panics or
/// decodes a prefix. A failed read keeps its own error, which the live reader
/// wraps for the event id.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self, state))]
pub(crate) async fn state_get_id_checked(
	&self,
	state: &CompressedState,
	event_type: &StateEventType,
	state_key: &str,
) -> Result<Mapped<OwnedEventId>> {
	let shortstatekey = self.services.db["statekey_shortstatekey"]
		.qry(&(event_type, state_key))
		.map_ok(|value| u64_from_bytes(&value).ok())
		.await?;

	let Some(shortstatekey) = shortstatekey else {
		return Ok(Mapped::Damaged);
	};

	let start = compress_state_event(shortstatekey, 0);
	let end = compress_state_event(shortstatekey, u64::MAX);
	let Some(&entry) = state.range(start..=end).next() else {
		return Err!(Request(NotFound("Not found in room state")));
	};

	let (_, shorteventid) = parse_compressed_state_event(entry);
	let event_id = self.services.db["shorteventid_eventid"]
		.get(&shorteventid.to_be_bytes())
		.map_ok(|value| event_id(&value))
		.await
		.present()?
		.flatten();

	Ok(event_id.map_or(Mapped::Damaged, Mapped::Event))
}

fn event_id(value: &[u8]) -> Option<OwnedEventId> {
	from_utf8(value)
		.ok()
		.and_then(|id| EventId::parse(id).ok())
}
