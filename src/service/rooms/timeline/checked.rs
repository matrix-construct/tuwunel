//! Check-only reads of timeline events.
//!
//! The live readers panic on an `eventid_pduid` mapping of the wrong length, so
//! integrity checks read through here, where that mapping is `Mapped::Damaged`.
//! `get_checked` reads as `get` does, racing the accepted branch (the mapping,
//! then the row) against the outlier copy, so a check reports a damaged mapping
//! only where `get` would panic and misses it when an outlier copy is found
//! first. `get_checked_keeping` reads the accepted branch before the outlier as
//! `get_room_version` does, so it always reports one. A copy is decoded as a
//! release build of the live readers decodes it, without a debug build's
//! assertion that no further record follows the event's.

use futures::{FutureExt, TryFutureExt, future::try_join};
use ruma::EventId;
use serde::Deserialize;
use tuwunel_core::{Error, Result, implement, result::NotFound};

use super::{RawPduId, Service};
use crate::integrity::leading_json;

/// Where the timeline places an event, its mapping decoded without trusting it.
///
/// A mapping that does not decode is told apart from an absent one, since
/// `get_pdu_id` panics on it.
pub(crate) enum Position {
	/// The event has no timeline position.
	Absent,

	/// The mapping does not decode as a PDU id.
	Damaged,

	/// The event's PDU id.
	At(RawPduId),
}

/// What a check-only read found, or damage on its way there.
///
/// An absent or undecodable event fails as the live reader fails, so a check
/// classifies those errors as it did before. Each route names the damage it
/// reports.
#[derive(Clone)]
pub(crate) enum Mapped<T> {
	/// The event, or what was read on the way to it.
	Event(T),

	/// A row on the way, such as the event's timeline mapping, is damaged.
	Damaged,
}

/// Deserializes an event from accepted or outlier storage as `get` does,
/// following its timeline mapping untrusted.
///
/// The accepted branch reads the mapping and then the row, while the outlier copy
/// is read beside it from the start, and the first found wins, as there. A
/// mapping that does not decode ends the race as `Mapped::Damaged` at the point
/// where `get` panics, so an outlier copy found before it is returned and the
/// damage goes unreported, as `get` returns that copy. When both fail, the
/// accepted error is returned unless its row is missing.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_checked<T>(&self, event_id: &EventId) -> Result<Mapped<T>>
where
	T: for<'de> Deserialize<'de>,
{
	let accepted = async {
		let pdu_id = match self.pdu_id_checked(event_id).await {
			| Mapped::Damaged => return Ok(Mapped::Damaged),
			| Mapped::Event(pdu_id) => pdu_id?,
		};

		self.get_from_id_checked(&pdu_id)
			.map_ok(Mapped::Event)
			.await
	};

	// Success takes the short-circuiting branch; only two failures are joined.
	let invert = |result: Result<Mapped<T>>| result.map_or_else(Ok, Err);
	let accepted = accepted.map(invert);
	let outlier = self
		.get_outlier_checked(event_id)
		.map_ok(Mapped::Event)
		.map(invert);

	try_join(accepted, outlier)
		.map(|result| match result {
			| Err(found) => Ok(found),
			| Ok((accepted, outlier)) if accepted.is_missing() => Err(outlier),
			| Ok((accepted, _)) => Err(accepted),
		})
		.await
}

/// Deserializes an event from accepted storage, or from outlier storage when
/// that fails, following its timeline mapping untrusted.
///
/// This is the read `get_room_version` makes through `get_non_outlier` and then
/// `get_outlier`: the accepted copy wins whenever it is read, and when both fail
/// `keep` says whether the accepted error is returned over the outlier's. A
/// mapping that does not decode is `Mapped::Damaged` before the outlier is read,
/// where `get_non_outlier` panics, and a failed or absent mapping fails the
/// accepted read, as there.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_checked_keeping<T, K>(
	&self,
	event_id: &EventId,
	keep: K,
) -> Result<Mapped<T>>
where
	T: for<'de> Deserialize<'de>,
	K: FnOnce(&Error) -> bool,
{
	let Mapped::Event(pdu_id) = self.pdu_id_checked(event_id).await else {
		return Ok(Mapped::Damaged);
	};

	let accepted = match pdu_id {
		| Ok(pdu_id) => self.get_from_id_checked(&pdu_id).await,
		| Err(error) => Err(error),
	};

	let accepted = match accepted {
		| Ok(event) => return Ok(Mapped::Event(event)),
		| Err(error) => error,
	};

	self.get_outlier_checked(event_id)
		.map_ok(Mapped::Event)
		.map_err(|outlier| match keep(&accepted) {
			| true => accepted,
			| false => outlier,
		})
		.await
}

/// Reads an event's timeline mapping as `get_pdu_id` does, decoding it untrusted.
///
/// A failed or absent read keeps its error, as there, and a mapping that does not
/// decode is `Mapped::Damaged` where that function panics.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn pdu_id_checked(&self, event_id: &EventId) -> Mapped<Result<RawPduId>> {
	match self.db.eventid_pduid.get(event_id).await {
		| Err(error) => Mapped::Event(Err(error)),
		| Ok(pdu_id) => RawPduId::try_from_bytes(&pdu_id)
			.map_or(Mapped::Damaged, |pdu_id| Mapped::Event(Ok(pdu_id))),
	}
}

/// Deserializes an accepted timeline row by PDU id as `get_from_id` does in a
/// release build.
///
/// A debug build of that function panics on a row holding a further record after
/// the event's, beyond one trailing separator, while this reads such a row as a
/// release build reads it.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn get_from_id_checked<T>(&self, pdu_id: &RawPduId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	let pdu = self.db.pduid_pdu.get(pdu_id).await?;

	leading_json(&pdu)
}

/// Deserializes an event from outlier storage as `get_outlier` does in a release
/// build.
///
/// Like `get_from_id_checked`, it reads a row holding a further record after the
/// event's as a release build reads it, where a debug build panics.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn get_outlier_checked<T>(&self, event_id: &EventId) -> Result<T>
where
	T: for<'de> Deserialize<'de>,
{
	let pdu = self.db.eventid_outlierpdu.get(event_id).await?;

	leading_json(&pdu)
}

/// Reads where the timeline places an event, decoding the mapping untrusted.
///
/// `get_pdu_id` panics on a mapping that is not a PDU id, which this reports.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub(crate) async fn pdu_position(&self, event_id: &EventId) -> Result<Position> {
	let position = self
		.db
		.eventid_pduid
		.get(event_id)
		.await
		.optional()?
		.map_or(Position::Absent, |pdu_id| {
			RawPduId::try_from_bytes(&pdu_id).map_or(Position::Damaged, Position::At)
		});

	Ok(position)
}

/// Transforms the event a read found, keeping a damaged mapping as it is.
///
/// A route reading part of an event, such as its content, maps the event it
/// found while the damage passes through untouched.
#[implement(Mapped, generics = "<T>", params = "<T>")]
pub(crate) fn map<U, F>(self, f: F) -> Mapped<U>
where
	F: FnOnce(T) -> U,
{
	match self {
		| Self::Event(event) => Mapped::Event(f(event)),
		| Self::Damaged => Mapped::Damaged,
	}
}
