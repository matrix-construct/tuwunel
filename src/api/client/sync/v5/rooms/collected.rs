//! What one delivery read of a room's required state.
//!
//! The result decides the snapshot the delivery records and the keys it leaves
//! for the next delivery to send.

use tuwunel_core::{implement, utils::BoolExt};
use tuwunel_service::{rooms::short::ShortStateHash, sync::UnreadState};

use super::{CollectedState, StateEvent};

/// One selected key's outcome: its event, if sent, or why it was left out.
///
/// A sent event carries its key's fingerprint when the last delivery had left
/// that key unread.
pub(super) type Outcome = Result<Option<(StateEvent, Option<u64>)>, Unread>;

/// Why a selected key's event was left out of a delivery, when not by choice.
///
/// A due event that could not be read leaves its key for the next delivery to
/// send; a key that could not be resolved makes the delivery record zero.
pub(super) enum Unread {
	/// The key could not be resolved, so whether its event was due is unknown.
	Unresolved,

	/// The event was due but could not be read; carries its key's fingerprint.
	Unreadable(u64),
}

/// The required state a delivery read, folded from its candidates.
///
/// A selected key adds its event when sent, its fingerprint when its due event
/// could not be read, or the mark of an unresolved key. A sent event whose key
/// the last delivery left unread also adds that key's fingerprint.
#[derive(Default)]
pub(super) struct Collected {
	/// The events read.
	pub(super) events: Vec<StateEvent>,

	/// Fingerprints of the keys whose due events could not be read.
	pub(super) unread: UnreadState,

	/// Fingerprints of keys the last delivery left unread whose events were sent.
	pub(super) settled: UnreadState,

	/// Whether a key could not be resolved.
	pub(super) unresolved: bool,
}

#[implement(Collected)]
pub(super) fn push(mut self, outcome: Outcome) -> Self {
	match outcome {
		| Ok(None) => {},
		| Err(Unread::Unreadable(fingerprint)) => self.unread.push(fingerprint),
		| Err(Unread::Unresolved) => self.unresolved = true,
		| Ok(Some((event, settled))) => {
			self.events.push(event);
			self.settled.extend(settled);
		},
	}

	self
}

/// The events read, the snapshot the delivery records, and the keys it leaves
/// unread.
///
/// `previous` holds the keys the last delivery left unread. A complete delivery,
/// one that compared every selected key against `snapshot` and resolved them
/// all, keeps none of them; any other keeps those whose events it did not send.
#[implement(Collected)]
pub(super) fn finish(self, snapshot: Option<ShortStateHash>, previous: &[u64]) -> CollectedState {
	let resolved = self.unresolved.is_false();
	let complete = snapshot.is_some() && resolved;
	let snapshot = recorded_snapshot(snapshot, resolved);
	let previous = complete
		.is_false()
		.then_some(previous)
		.unwrap_or_default();

	let kept = previous
		.iter()
		.copied()
		.filter(|fingerprint| !self.settled.contains(fingerprint));

	let unread = unread_state(kept, self.unread);

	(self.events, (snapshot, unread))
}

/// The snapshot a delivery records, given whether it resolved every selected key.
///
/// A key whose listing or current event could not be read may hide an event that
/// was due, so no snapshot vouches for what the client holds: the delivery
/// records zero, and the next delta compares against the cursor and resends
/// outliers.
pub(super) fn recorded_snapshot(
	snapshot: Option<ShortStateHash>,
	resolved: bool,
) -> Option<ShortStateHash> {
	resolved.copy_or(Some(0), snapshot)
}

/// The keys the next delivery sends whatever snapshot it compares against.
///
/// They are the keys whose due events this delivery could not read, and the
/// `kept` keys an earlier delivery left unread, each once and in order.
pub(super) fn unread_state(
	kept: impl Iterator<Item = u64>,
	mut unread: UnreadState,
) -> UnreadState {
	unread.extend(kept);
	unread.sort_unstable();
	unread.dedup();
	unread.shrink_to_fit();
	unread
}
