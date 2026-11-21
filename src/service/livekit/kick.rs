//! Removes departed users from LiveKit (MSC4195).
//!
//! When a user leaves a room or is banned from it, the membership change
//! queues a kick of the participants this server recorded issuing the user
//! tokens for in that room, since it must not wait on LiveKit. The worker then
//! asks LiveKit to remove each one.

use std::ops::Not;

use futures::StreamExt;
use ruma::{RoomId, UserId};
use tuwunel_core::{Result, error, implement, utils::stream::ReadyExt};
use tuwunel_database::{Ignore, Interfix};

use super::{
	Conf, Departure, Service,
	delegation::{Generation, Participant},
};

type Row = (Participant, Generation);

// Departures kicked at once, so one stalled RoomService call does not hold up the others.
const KICK_WIDTH: usize = 8;

/// Queues a kick of a departed user's LiveKit participants in a room, and
/// returns whether it did.
///
/// It queues nothing unless native LiveKit is on and the user was issued a
/// token in the room, which one seek answers, so a departure with nothing to
/// kick costs no more. An index it cannot read still queues the kick, so the
/// worker reads it again.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip(self))]
pub async fn on_departure(&self, room_id: &RoomId, user_id: &UserId) -> bool {
	let issued = self.is_enabled()
		&& self
			.db
			.roomuserid_lkparticipant
			.keys_prefix_raw(&(room_id, user_id, Interfix))
			.ready_any(|_| true)
			.await;

	if !issued {
		return false;
	}

	let (departures, _) = &self.departures;

	// Closed only at shutdown, which drops the kicks not yet made.
	departures
		.send((room_id.to_owned(), user_id.to_owned()))
		.is_ok()
}

/// Kicks departed users' participants as their departures arrive, until
/// shutdown.
///
/// A few departures are handled at once, so one stalled RoomService call does
/// not hold up the rest. Departures still queued at shutdown are dropped.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn kick_loop(&self) {
	let Ok(conf) = self.conf() else {
		return;
	};

	let (_, departures) = &self.departures;

	departures
		.stream()
		.take_until(self.services.server.until_shutdown())
		.for_each_concurrent(KICK_WIDTH, |departure| self.kick(conf, departure))
		.await;
}

/// Removes a departed user's participants in a room from LiveKit.
///
/// Each participant is removed once, and then every row read for it deleted,
/// whatever LiveKit answered, since a kick is never retried. A token issued
/// meanwhile has its own row under a fresh generation, which this kick never
/// deletes. The membership is read again before each row: while the user is
/// joined to the room again, and so may already hold new tokens for the same
/// participants, or while the membership cannot be read, the kick leaves both
/// the participant and its row alone. At shutdown it stops reading rows, and
/// only a removal already asked for finishes.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%room_id,
		%user_id,
	),
)]
async fn kick(&self, conf: &Conf, (room_id, user_id): Departure) {
	let issued = &self.db.roomuserid_lkparticipant;

	issued
		.keys_prefix(&(&room_id, &user_id, Interfix))
		.ready_filter_map(row)
		.take_until(self.services.server.until_shutdown())
		// Read for every row, since the user may join again while a removal is under way.
		.filter_map(async |row| {
			self.departed(&room_id, &user_id)
				.await
				.then_some(row)
		})
		// The rows of a participant issued several tokens come in one run.
		.ready_scan(None, |last: &mut Option<Participant>, (participant, generation): Row| {
			let repeat = last.replace(participant) == Some(participant);

			Some((participant, generation, repeat))
		})
		.for_each(async |(participant, generation, repeat)| {
			let (room, identity) = participant.key();

			// Not found means it already left; any other failure was logged by the call.
			if !repeat {
				self.remove_participant(conf, room, identity)
					.await
					.ok();
			}

			issued.del((&room_id, &user_id, room, identity, generation.as_str()));
		})
		.await;
}

/// Whether a user is not joined to a room.
///
/// A membership that cannot be read is logged and counts as joined, so no
/// participant is removed on it.
#[implement(Service)]
#[tracing::instrument(level = "trace", skip_all)]
async fn departed(&self, room_id: &RoomId, user_id: &UserId) -> bool {
	self.services
		.state_cache
		.try_is_joined(user_id, room_id)
		.await
		.inspect_err(|error| error!(%error, "Failed to read the membership of a user to kick"))
		.is_ok_and(Not::not)
}

fn row(key: Result<(Ignore, Ignore, &str, &str, &str)>) -> Option<Row> {
	let (_, _, room, identity, generation) = key
		.inspect_err(|error| error!(%error, "Unreadable LiveKit participant key"))
		.ok()?;

	let row = (Participant::from_names(room, identity)?, Generation::from(generation).ok()?);

	Some(row)
}
