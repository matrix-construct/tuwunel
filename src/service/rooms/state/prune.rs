use std::{cmp::Ordering, collections::BTreeMap};

use futures::StreamExt;
use ruma::{EventId, OwnedEventId, OwnedServerName, RoomId};
use tuwunel_core::{
	implement,
	matrix::{Event, PduCount},
	smallvec::SmallVec,
	utils::{IterStream, stream::BroadbandExt},
};

use crate::federation::ShouldAttempt;

type Servers = SmallVec<[OwnedServerName; 1]>;

/// Per-round summary of a scored prune, for the receive-path `debug!` and the
/// admin command's report. `after` counts engine survivors; the receive path
/// chains the incoming event afterwards, so the written band is `after + 1`
/// when the event is not soft-failed.
#[derive(Clone, Copy, Debug, Default)]
pub struct PruneSummary {
	pub before: usize,
	pub after: usize,
	pub dangling: usize,
	pub referenced: usize,
	pub message: usize,
	pub state: usize,
}

/// Selects whether the prune sweeps already-referenced leaves: the receive
/// path's retained set has excluded them upstream, an operator's raw band has
/// not.
#[derive(Clone, Copy, Debug)]
pub enum Trigger {
	Receive,
	Admin,
}

struct Candidate<Id> {
	id: Id,
	class: Class,
}

enum Class {
	Dangling,
	Referenced,
	Own,
	Live(Live),
}

struct Live {
	state: bool,
	redacted: bool,
	owner_online: bool,
	count: PduCount,
}

/// A leaf's classification before per-server reachability is resolved into
/// `Live::owner_online`, kept apart so the reachability query runs once per
/// unique server rather than once per leaf.
enum Partial {
	Dangling,
	Referenced,
	Own,
	Live {
		state: bool,
		redacted: bool,
		count: PduCount,
		server: OwnedServerName,
	},
}

/// Scores the retained forward-extremity set and drops the least useful leaves
/// in place until `goal` of them are gone, sweeping dangling leaves (and, under
/// `Trigger::Admin`, already-referenced ones) for free first. Never drops an
/// own-server leaf and always leaves at least one survivor.
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug"
	skip_all,
	fields(%room_id),
)]
pub async fn prune_forward_extremities(
	&self,
	room_id: &RoomId,
	extremities: &mut Vec<OwnedEventId>,
	goal: usize,
	trigger: Trigger,
) -> PruneSummary {
	let candidates = self
		.classify_extremities(room_id, extremities, trigger)
		.await;

	let (survivors, summary) = select(candidates, goal);
	*extremities = survivors;

	summary
}

#[implement(super::Service)]
async fn classify_extremities(
	&self,
	room_id: &RoomId,
	extremities: &[OwnedEventId],
	trigger: Trigger,
) -> Vec<Candidate<OwnedEventId>> {
	let partials: Vec<(OwnedEventId, Partial)> = extremities
		.iter()
		.stream()
		.broad_then(async |event_id| {
			let partial = self
				.classify_leaf(room_id, event_id, trigger)
				.await;

			(event_id.clone(), partial)
		})
		.collect()
		.await;

	let mut servers: Servers = partials
		.iter()
		.filter_map(|(_, partial)| match partial {
			| Partial::Live { server, .. } => Some(server.clone()),
			| _ => None,
		})
		.collect();

	servers.sort_unstable();
	servers.dedup();

	let online: BTreeMap<OwnedServerName, bool> = servers
		.into_iter()
		.stream()
		.broad_then(async |server| {
			let verdict = self
				.services
				.federation
				.should_attempt(&server)
				.await;

			(server, !matches!(verdict, ShouldAttempt::No { .. }))
		})
		.collect()
		.await;

	partials
		.into_iter()
		.map(|(id, partial)| {
			let class = match partial {
				| Partial::Dangling => Class::Dangling,
				| Partial::Referenced => Class::Referenced,
				| Partial::Own => Class::Own,
				| Partial::Live { state, redacted, count, server } => Class::Live(Live {
					state,
					redacted,
					owner_online: online.get(&server).copied().unwrap_or(false),
					count,
				}),
			};

			Candidate { id, class }
		})
		.collect()
}

#[implement(super::Service)]
async fn classify_leaf(&self, room_id: &RoomId, event_id: &EventId, trigger: Trigger) -> Partial {
	let Ok(count) = self
		.services
		.timeline
		.get_pdu_count(event_id)
		.await
	else {
		return Partial::Dangling;
	};

	let Ok(pdu) = self.services.timeline.get_pdu(event_id).await else {
		return Partial::Dangling;
	};

	if matches!(trigger, Trigger::Admin)
		&& self
			.services
			.pdu_metadata
			.is_event_referenced(room_id, event_id)
			.await
	{
		return Partial::Referenced;
	}

	let server = pdu.sender().server_name();
	if server == self.services.globals.server_name() {
		return Partial::Own;
	}

	Partial::Live {
		state: pdu.state_key().is_some(),
		redacted: pdu.is_redacted(),
		count,
		server: server.to_owned(),
	}
}

/// Chooses survivors from the classified leaves. Dangling and referenced leaves
/// are dropped for free (not counted toward `goal`), own leaves are always
/// kept, and the most-droppable live leaves are dropped up to `goal`, subject
/// to the one-survivor floor.
fn select<Id>(candidates: Vec<Candidate<Id>>, goal: usize) -> (Vec<Id>, PruneSummary) {
	let before = candidates.len();

	let mut own = Vec::new();
	let mut live = Vec::new();
	let mut dangling = Vec::new();
	let mut referenced = Vec::new();

	for Candidate { id, class } in candidates {
		match class {
			| Class::Own => own.push(id),
			| Class::Dangling => dangling.push(id),
			| Class::Referenced => referenced.push(id),
			| Class::Live(scored) => live.push((scored, id)),
		}
	}

	live.sort_unstable_by(|(a, _), (b, _)| drop_order(a, b));

	// Keep one live leaf when nothing else would survive (no own leaf to fall
	// back on): the floor caps the drop at one short of the whole live set.
	let floor = usize::from(own.is_empty());
	let drop_live = goal.min(live.len().saturating_sub(floor));

	let message = live[..drop_live]
		.iter()
		.filter(|(scored, _)| !scored.state)
		.count();

	let state = drop_live.saturating_sub(message);

	let mut survivors: Vec<Id> = own
		.into_iter()
		.chain(live.into_iter().skip(drop_live).map(|(_, id)| id))
		.collect();

	// Never write an empty band: when no own or live leaf survives, keep a single
	// swept leaf, preferring a referenced one since its event is still stored.
	if survivors.is_empty()
		&& let Some(id) = referenced.pop().or_else(|| dangling.pop())
	{
		survivors.push(id);
	}

	let summary = PruneSummary {
		before,
		after: survivors.len(),
		dangling: dangling.len(),
		referenced: referenced.len(),
		message,
		state,
	};

	(survivors, summary)
}

/// Orders live leaves most-droppable first: message before state, redacted
/// before intact, reachable-owner before unreachable, oldest before newest.
fn drop_order(a: &Live, b: &Live) -> Ordering {
	a.state
		.cmp(&b.state)
		.then(b.redacted.cmp(&a.redacted))
		.then(b.owner_online.cmp(&a.owner_online))
		.then(a.count.cmp(&b.count))
}

/// Per-round drop goal for the paced receive-path prune: an uncapped cut down
/// to the emergency bound, but never slower than the per-event batch down to
/// the cap. All saturating.
pub(crate) fn prune_goal(len: usize, max: usize, emergency: usize, batch: usize) -> usize {
	// Clamp emergency up to the cap so a value below it cannot invert the arms.
	let emergency = emergency.max(max);

	len.saturating_sub(emergency)
		.max(len.saturating_sub(max).min(batch))
}

#[cfg(test)]
mod tests {
	use super::*;

	fn live(
		id: u32,
		state: bool,
		redacted: bool,
		owner_online: bool,
		count: u64,
	) -> Candidate<u32> {
		Candidate {
			id,
			class: Class::Live(Live {
				state,
				redacted,
				owner_online,
				count: PduCount::Normal(count),
			}),
		}
	}

	fn own(id: u32) -> Candidate<u32> { Candidate { id, class: Class::Own } }

	fn dangling(id: u32) -> Candidate<u32> { Candidate { id, class: Class::Dangling } }

	fn referenced(id: u32) -> Candidate<u32> { Candidate { id, class: Class::Referenced } }

	#[test]
	fn message_leaves_drop_before_state() {
		let candidates = vec![
			live(1, true, false, false, 1),
			live(2, false, false, false, 5),
			live(3, false, false, false, 6),
		];

		let (survivors, summary) = select(candidates, 2);

		assert_eq!(survivors, vec![1]);
		assert_eq!(summary.message, 2);
		assert_eq!(summary.state, 0);
		assert_eq!(summary.after, 1);
		assert_eq!(summary.before, 3);
	}

	#[test]
	fn state_leaf_outlives_redacted_message() {
		// A redacted, online, old state leaf still survives over a pristine,
		// offline, newest message: the state axis is strictly major.
		let candidates = vec![live(1, true, true, true, 1), live(2, false, false, false, 9)];

		let (survivors, _) = select(candidates, 1);

		assert_eq!(survivors, vec![1]);
	}

	#[test]
	fn redacted_drops_before_intact() {
		let candidates = vec![live(1, false, false, false, 5), live(2, false, true, false, 5)];

		let (survivors, _) = select(candidates, 1);

		assert_eq!(survivors, vec![1]);
	}

	#[test]
	fn online_owner_drops_before_offline() {
		let candidates = vec![live(1, false, false, false, 5), live(2, false, false, true, 5)];

		let (survivors, _) = select(candidates, 1);

		assert_eq!(survivors, vec![1]);
	}

	#[test]
	fn oldest_drops_first() {
		let candidates = vec![live(1, false, false, false, 9), live(2, false, false, false, 3)];

		let (survivors, _) = select(candidates, 1);

		assert_eq!(survivors, vec![1]);
	}

	#[test]
	fn own_leaf_never_dropped() {
		let candidates =
			vec![own(1), live(2, false, false, false, 5), live(3, false, false, false, 6)];

		let (survivors, summary) = select(candidates, 10);

		assert_eq!(survivors, vec![1]);
		assert_eq!(summary.after, 1);
		assert_eq!(summary.message, 2);
	}

	#[test]
	fn dangling_swept_free_and_uncounted() {
		let candidates = vec![
			dangling(1),
			dangling(2),
			dangling(3),
			live(4, false, false, false, 5),
			live(5, false, false, false, 6),
		];

		let (survivors, summary) = select(candidates, 1);

		assert_eq!(survivors, vec![5]);
		assert_eq!(summary.dangling, 3);
		assert_eq!(summary.message, 1);
		assert_eq!(summary.after, 1);
	}

	#[test]
	fn referenced_swept_free_on_admin() {
		let candidates = vec![referenced(1), referenced(2), live(3, false, false, false, 5)];

		let (survivors, summary) = select(candidates, 0);

		assert_eq!(survivors, vec![3]);
		assert_eq!(summary.referenced, 2);
		assert_eq!(summary.after, 1);
	}

	#[test]
	fn floor_keeps_one_live_when_all_would_drop() {
		let candidates = vec![
			live(1, false, false, false, 5),
			live(2, false, false, false, 6),
			live(3, false, false, false, 7),
		];

		let (survivors, summary) = select(candidates, 10);

		assert_eq!(survivors, vec![3]);
		assert_eq!(summary.message, 2);
	}

	#[test]
	fn floor_keeps_one_dangling_when_nothing_else() {
		let candidates = vec![dangling(1), dangling(2), dangling(3)];

		let (survivors, summary) = select(candidates, 10);

		assert_eq!(survivors.len(), 1);
		assert_eq!(summary.dangling, 2);
	}

	#[test]
	fn floor_prefers_referenced_over_dangling() {
		let candidates = vec![dangling(1), referenced(2)];

		let (survivors, summary) = select(candidates, 10);

		assert_eq!(survivors, vec![2]);
		assert_eq!(summary.referenced, 0);
		assert_eq!(summary.dangling, 1);
	}

	#[test]
	fn pace_emergency_cut_above_bound() {
		assert_eq!(prune_goal(1060, 60, 256, 32), 804);
	}

	#[test]
	fn pace_batch_between_cap_and_bound() {
		assert_eq!(prune_goal(257, 60, 256, 32), 32);
	}

	#[test]
	fn pace_one_over_cap_drops_one() {
		assert_eq!(prune_goal(61, 60, 256, 32), 1);
	}

	#[test]
	fn pace_at_cap_drops_none() {
		assert_eq!(prune_goal(60, 60, 256, 32), 0);
	}

	#[test]
	fn pace_emergency_at_or_below_cap_is_unpaced() {
		// emergency <= max removes pacing: the goal is always len - max.
		assert_eq!(prune_goal(100, 60, 0, 32), 40);
		assert_eq!(prune_goal(100, 60, 60, 32), 40);
	}

	#[test]
	fn pace_zero_batch_parks_at_emergency() {
		assert_eq!(prune_goal(300, 60, 256, 0), 44);
		assert_eq!(prune_goal(256, 60, 256, 0), 0);
	}

	#[test]
	fn pace_monotone_across_emergency_bound() {
		// Just under the bound still takes the batch, so there is no discontinuous
		// jump to a one-leaf trickle at the boundary.
		assert_eq!(prune_goal(256, 60, 256, 32), 32);
	}
}
