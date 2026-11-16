use futures::StreamExt;
use serde_json::{Value, value::to_raw_value};
use tuwunel_core::{
	Err, Result, async_noinline, at, err,
	itertools::Itertools,
	ruma::{
		CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId, UserId,
		api::federation::backfill::get_backfill::v1::Request as BackfillRequest,
		events::room::{
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			member::MembershipState,
		},
	},
	utils::result::NotFound,
};
use tuwunel_matrix::pdu::{PduBuilder, into_outgoing_federation};
use tuwunel_service::Services;

use super::{
	client::register,
	federation::send,
	helpers::{
		append_membership, append_message, create_room, redeliver, set_forward_extremities,
		sign_message, sign_outlier_message,
	},
};

type Pdus = Vec<Value>;

const BANNED_TOKEN: &str = "state-local-build-banned-access-token";
const WITNESS_TOKEN: &str = "state-local-build-witness-access-token";
const EARLIER: &str = "earlier soft-failed backfill message";
const SOFT_FAILED: &str = "soft-failed backfill message";
const FIRST: &str = "references the soft-failed message";
const SECOND: &str = "references it again";
const OUTLIER: &str = "plain outlier";
const THIRD: &str = "references the plain outlier";
const REFERRER: &str = "plain outlier referencing the soft-failed message";
const HIDDEN_SOFT_FAILED: &str = "soft-failed message behind a hidden event";
const HIDDEN: &str = "hidden event referencing the soft-failed message";
const WITHHELD_TOKEN: &str = "state-local-build-withheld-access-token";
const WITHHELD: &str = "soft-failed message offered by backfill";
const PROMOTED: &str = "plain outlier offered by backfill";
const FOREIGN_SERVER: &str = "policy.invalid";
const ACCEPTED: &str = "accepted message under the soft-failed events";
const SEED_OUTLIER: &str = "plain outlier under the oldest event";
const BRANCH_OUTLIER: &str = "plain outlier under a soft-failed branch";
const DEEPER: &str = "deeper soft-failed message under the oldest event";
const NEARER: &str = "nearer soft-failed message under the oldest event";
const LEFT: &str = "soft-failed branch over the accepted message";
const RIGHT: &str = "soft-failed branch over a plain outlier";
const SPLIT: &str = "soft-failed message naming both branches";
const REJOIN: &str = "soft-failed message naming the second branch again";
const ABOVE_CHAIN: &str = "references the soft-failed chain";
const ABOVE_FORK: &str = "references the chain and the accepted message";
const ABOVE_OUTLIER: &str = "references a plain outlier";
const ABOVE_SPLIT: &str = "references the soft-failed fork";
const ABOVE_BRANCHES: &str = "references every soft-failed branch";
const STORED: &str = "accepted message under the backfill scan";
const SCAN_OUTLIER: &str = "plain outlier under the backfill scan";
const DEEP: &str = "backfilled message over the plain outlier";
const MISSING_WITHHELD: &str = "soft-failed message over the plain outlier";
const BEYOND: &str = "backfilled message over the soft-failed message missing history";
const STORED_WITHHELD: &str = "soft-failed message over the accepted message";
const THROUGH: &str = "backfilled message over the soft-failed message with stored history";
const SHALLOW: &str = "backfilled message over the accepted message";
const ABOVE_GAP: &str = "references the plain outlier under the backfill scan";
const SEEDS_GAP: &str = "plain outlier behind the accepted backfilled message";
const SEEDS_LOST: &str = "plain outlier no candidate supplies";
const SEEDS_STORED: &str = "accepted backfilled message over a missing predecessor";
const SEEDS_SCANNED: &str = "backfilled message over the predecessor no candidate supplies";
const SEEDS_WITHHELD: &str = "soft-failed message over the accepted backfilled message";
const SEEDS_FIRST: &str = "backfilled message over the soft-failed message";

// Enough to reach the room's create event from any request below.
const WHOLE_ROOM: u32 = 50;

// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn soft_failed_served_when_referenced<'a>(
	services: &'a Services,
	base: &'a str,
	token: &'a str,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let banned = register(services, "backfillbanned", BANNED_TOKEN).await?;

	append_membership(services, user_id, room_id, &banned, MembershipState::Invite).await?;
	let join =
		append_membership(services, &banned, room_id, &banned, MembershipState::Join).await?;

	let (earlier, earlier_json) = sign_message(services, &banned, room_id, EARLIER).await?;

	services
		.timeline
		.add_pdu_outlier(&earlier.event_id, &earlier_json);

	set_forward_extremities(services, room_id, [earlier.event_id.as_ref()]).await;
	let (message, message_json) = sign_message(services, &banned, room_id, SOFT_FAILED).await?;

	set_forward_extremities(services, room_id, [&join]).await;
	let ban =
		append_membership(services, user_id, room_id, &banned, MembershipState::Ban).await?;

	let handled = redeliver(services, room_id, &earlier, earlier_json, "earlier message").await?;

	assert!(!handled, "the banned user's earlier message was not soft failed");

	let handled = redeliver(services, room_id, &message, message_json, "later message").await?;

	assert!(!handled, "the banned user's later message was not soft failed");

	let soft_failed: &EventId = message.event_id.as_ref();

	assert_soft_failed(services, &earlier.event_id).await;
	assert_soft_failed(services, soft_failed).await;

	set_forward_extremities(services, room_id, [soft_failed, &ban]).await;
	let first = append_message(services, user_id, room_id, FIRST).await?;

	set_forward_extremities(services, room_id, [soft_failed, &first]).await;
	let second = append_message(services, user_id, room_id, SECOND).await?;

	set_forward_extremities(services, room_id, [soft_failed]).await;
	let (referrer, _) = sign_outlier_message(services, user_id, room_id, REFERRER).await?;

	set_forward_extremities(services, room_id, [&second]).await;
	let (outlier, _) = sign_outlier_message(services, user_id, room_id, OUTLIER).await?;

	set_forward_extremities(services, room_id, [outlier.event_id.as_ref(), &second]).await;
	let third = append_message(services, user_id, room_id, THIRD).await?;

	let pdus = backfill(services, base, room_id, &second, WHOLE_ROOM).await?;

	assert_eq!(count(&pdus, SECOND), 1, "backfill omitted its own starting event");
	assert_eq!(count(&pdus, FIRST), 1, "backfill omitted an earlier timeline event");
	assert_eq!(count(&pdus, SOFT_FAILED), 1, "two references did not serve the event once");
	assert_eq!(count(&pdus, EARLIER), 1, "a served soft-failed event's predecessor was omitted");

	let pdus = backfill(services, base, room_id, &first, 2).await?;

	assert_eq!(pdus.len(), 2, "the soft-failed event did not take a slot within the limit");
	assert!(
		bodies(&pdus).eq([FIRST, SOFT_FAILED]),
		"the soft-failed event did not follow the event referencing it"
	);

	let pdus = backfill(services, base, room_id, soft_failed, WHOLE_ROOM).await?;

	assert_eq!(count(&pdus, SOFT_FAILED), 1, "a requested soft-failed event was not served");
	assert_eq!(
		count(&pdus, EARLIER),
		1,
		"the requested event's soft-failed predecessor was omitted"
	);
	assert!(
		has_membership(&pdus, &banned, "join"),
		"backfill did not continue from the soft-failed chain's predecessor"
	);
	assert!(
		!has_membership(&pdus, &banned, "ban"),
		"backfill from the soft-failed event served a later event"
	);

	let pdus = backfill(services, base, room_id, &ban, WHOLE_ROOM).await?;

	assert!(has_membership(&pdus, &banned, "ban"), "backfill omitted its own starting event");
	assert_eq!(count(&pdus, SOFT_FAILED), 0, "an unreferenced soft-failed event was served");
	assert_eq!(count(&pdus, EARLIER), 0, "an unreferenced earlier soft-failed event was served");

	let other_room = create_room(services, base, token).await?;
	let pdus = backfill(services, base, &other_room, soft_failed, WHOLE_ROOM).await?;

	assert_eq!(count(&pdus, SOFT_FAILED), 0, "another room's soft-failed event was served");

	let pdus = backfill(services, base, room_id, &third, WHOLE_ROOM).await?;

	assert_eq!(count(&pdus, THIRD), 1, "backfill omitted its own starting event");
	assert_eq!(count(&pdus, OUTLIER), 0, "an outlier that never soft failed was served");

	let pdus = backfill(services, base, room_id, &referrer.event_id, WHOLE_ROOM).await?;

	assert_eq!(
		count(&pdus, REFERRER),
		0,
		"a requested outlier that never soft failed was served"
	);
	assert_eq!(
		count(&pdus, SOFT_FAILED),
		1,
		"a requested outlier did not expose its soft-failed predecessor"
	);
	assert!(
		has_membership(&pdus, &banned, "join"),
		"backfill did not continue from the timeline before the requested outlier"
	);

	hidden_referencer_needs_the_request(services, base, token, user_id, &banned).await
}

/// A soft-failed event referenced only by an event the origin cannot see is
/// served when the request names that event, and otherwise withheld.
///
/// The witness is the server's only member at the end, and the hidden event
/// falls in the gap of its membership under `joined` history visibility.
// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
async fn hidden_referencer_needs_the_request<'a>(
	services: &'a Services,
	base: &'a str,
	token: &'a str,
	user_id: &'a UserId,
	banned: &'a UserId,
) -> Result {
	let room_id = &create_room(services, base, token).await?;
	let witness = register(services, "backfillwitness", WITNESS_TOKEN).await?;

	restrict_history(services, user_id, room_id).await?;
	append_membership(services, user_id, room_id, &witness, MembershipState::Invite).await?;
	append_membership(services, &witness, room_id, &witness, MembershipState::Join).await?;
	append_membership(services, user_id, room_id, banned, MembershipState::Invite).await?;
	append_membership(services, banned, room_id, banned, MembershipState::Join).await?;

	let (message, message_json) =
		sign_message(services, banned, room_id, HIDDEN_SOFT_FAILED).await?;

	append_membership(services, user_id, room_id, banned, MembershipState::Ban).await?;

	let handled = redeliver(services, room_id, &message, message_json, "hidden message").await?;

	assert!(!handled, "the message behind the hidden event was not soft failed");

	assert_soft_failed(services, &message.event_id).await;

	let leave =
		append_membership(services, &witness, room_id, &witness, MembershipState::Leave).await?;

	set_forward_extremities(services, room_id, [message.event_id.as_ref(), &leave]).await;
	let hidden = append_message(services, user_id, room_id, HIDDEN).await?;

	append_membership(services, user_id, room_id, &witness, MembershipState::Invite).await?;
	append_membership(services, &witness, room_id, &witness, MembershipState::Join).await?;

	let last =
		append_membership(services, user_id, room_id, user_id, MembershipState::Leave).await?;

	let pdus = backfill(services, base, room_id, &last, WHOLE_ROOM).await?;

	assert!(
		has_membership(&pdus, &witness, "leave"),
		"backfill did not walk past the hidden event"
	);
	assert_eq!(count(&pdus, HIDDEN), 0, "an event the origin cannot see was served");
	assert_eq!(count(&pdus, HIDDEN_SOFT_FAILED), 0, "a hidden event exposed a soft-failed one");

	let pdus = backfill(services, base, room_id, &hidden, WHOLE_ROOM).await?;

	assert_eq!(count(&pdus, HIDDEN), 0, "a requested event the origin cannot see was served");
	assert_eq!(
		count(&pdus, HIDDEN_SOFT_FAILED),
		1,
		"a soft-failed event the request references was not served"
	);

	Ok(())
}

/// Backfill leaves a soft-failed event the outlier it was stored as, and still
/// promotes a plain outlier.
///
/// The second offer carries a signature no verification asks for, so a copy
/// that replaced the stored outlier would show it. The last, once the stored
/// copy is unreadable, fails instead of promoting the event.
// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn soft_failed_stays_out_of_backfill<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let banned = register(services, "backfillwithheld", WITHHELD_TOKEN).await?;

	append_membership(services, user_id, room_id, &banned, MembershipState::Invite).await?;
	append_membership(services, &banned, room_id, &banned, MembershipState::Join).await?;

	let (message, message_json) = sign_message(services, &banned, room_id, WITHHELD).await?;

	append_membership(services, user_id, room_id, &banned, MembershipState::Ban).await?;

	let handled =
		redeliver(services, room_id, &message, message_json.clone(), "withheld message").await?;

	assert!(!handled, "the banned user's message was not soft failed");

	let withheld: &EventId = message.event_id.as_ref();

	assert_soft_failed(services, withheld).await;

	let prepended = ingest(services, room_id, message_json.clone()).await?;

	assert!(!prepended, "backfill prepended a soft-failed event");

	assert_soft_failed(services, withheld).await;

	let prepended =
		ingest(services, room_id, with_foreign_signature(message_json.clone())).await?;

	assert!(!prepended, "backfill prepended a copy of a soft-failed event");

	let replaced = services
		.timeline
		.get_outlier_pdu_json(withheld)
		.await?
		.get("signatures")
		.and_then(CanonicalJsonValue::as_object)
		.is_some_and(|signatures| signatures.contains_key(FOREIGN_SERVER));

	assert!(!replaced, "a backfilled copy replaced the stored soft-failed event");

	let (outlier, outlier_json) =
		sign_outlier_message(services, user_id, room_id, PROMOTED).await?;

	let prepended = ingest(services, room_id, outlier_json).await?;

	assert!(prepended, "backfill withheld a plain outlier");

	services
		.timeline
		.non_outlier_pdu_exists(outlier.event_id.as_ref())
		.await?;

	let outliers = services.db.get("eventid_outlierpdu")?;

	outliers.insert(withheld.as_bytes(), b"{");
	services.clear_cache().await;

	let ingested = ingest(services, room_id, message_json).await;

	assert!(ingested.is_err(), "backfill read an unreadable soft-failed event as unmarked");

	let promoted = services
		.timeline
		.non_outlier_pdu_exists(withheld)
		.await
		.is_ok();

	assert!(!promoted, "an unreadable soft-failed event reached the timeline");

	Ok(())
}

/// The backfill frontier holds every event below which history is missing.
///
/// The soft-failed outliers are stored as backfill leaves a refused event: an
/// outlier row and a marker, with no timeline row. The walk passes through them
/// to the end of a chain and reaches every branch of a withheld fork once. A
/// branch over an accepted predecessor stays, since that predecessor's own
/// history may still be missing.
// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn withheld_frontier_seeds_backfill<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let accepted = append_message(services, user_id, room_id, ACCEPTED).await?;
	let (gap, _) = sign_outlier_message(services, user_id, room_id, SEED_OUTLIER).await?;
	let (branch_gap, _) =
		sign_outlier_message(services, user_id, room_id, BRANCH_OUTLIER).await?;

	let gap: &EventId = gap.event_id.as_ref();

	let deeper = withheld_over(services, user_id, room_id, [gap], DEEPER).await?;
	let nearer = withheld_over(services, user_id, room_id, [&deeper], NEARER).await?;
	let left = withheld_over(services, user_id, room_id, [&accepted], LEFT).await?;
	let right = withheld_over(services, user_id, room_id, [&branch_gap.event_id], RIGHT).await?;
	let split = withheld_over(services, user_id, room_id, [&left, &right], SPLIT).await?;
	let rejoin = withheld_over(services, user_id, room_id, [&right], REJOIN).await?;

	let above_chain = message_over(services, user_id, room_id, [&nearer], ABOVE_CHAIN).await?;
	let above_fork =
		message_over(services, user_id, room_id, [&accepted, &nearer], ABOVE_FORK).await?;

	let above_outlier = message_over(services, user_id, room_id, [gap], ABOVE_OUTLIER).await?;
	let above_split = message_over(services, user_id, room_id, [&split], ABOVE_SPLIT).await?;
	let above_branches =
		message_over(services, user_id, room_id, [&split, &rejoin, &nearer], ABOVE_BRANCHES)
			.await?;

	let (deeper, left, right): (&EventId, &EventId, &EventId) =
		(deeper.as_ref(), left.as_ref(), right.as_ref());

	let frontier = frontier_below(services, room_id, &above_chain).await?;

	assert_eq!(frontier, [deeper], "the walk stopped short of the chain's end");

	let frontier = frontier_below(services, room_id, &above_fork).await?;

	assert_eq!(
		frontier,
		[above_fork.as_ref(), deeper],
		"the walk did not keep both the accepted and the withheld predecessor"
	);

	let frontier = frontier_below(services, room_id, &above_outlier).await?;

	assert_eq!(frontier, [above_outlier], "the walk passed over a plain outlier");

	let frontier = frontier_below(services, room_id, &above_split).await?;
	let frontier: Vec<_> = frontier.into_iter().sorted().collect();
	let split: Vec<_> = [left, right].into_iter().sorted().collect();

	assert_eq!(frontier, split, "the fork lost a branch over an accepted predecessor");

	let frontier = frontier_below(services, room_id, &above_branches).await?;
	let frontier: Vec<_> = frontier.into_iter().sorted().collect();
	let branches: Vec<_> = [left, right, deeper]
		.into_iter()
		.sorted()
		.collect();

	assert_eq!(
		frontier, branches,
		"the walk did not reach every branch of a withheld fork once"
	);

	Ok(())
}

/// The backfill scan rises past events whose history is stored to those still
/// missing some, and finds none once the room's history is whole.
///
/// Each event goes in through backfill, so it lies below the one before it in
/// stream order, as the last batches over a converging fork leave them. The
/// first event and the one over a soft-failed message name only stored
/// history; the next two miss it, the first behind a soft-failed message. The
/// create event ends the scan, so a later event over the same gap is not met.
// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn backfill_edges_rise_past_stored_history<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let accepted = append_message(services, user_id, room_id, STORED).await?;
	let (gap, gap_json) = sign_outlier_message(services, user_id, room_id, SCAN_OUTLIER).await?;
	let gap: &EventId = gap.event_id.as_ref();

	let deep = backfilled_over(services, user_id, room_id, [gap], DEEP).await?;
	let missing = withheld_over(services, user_id, room_id, [gap], MISSING_WITHHELD).await?;
	let beyond = backfilled_over(services, user_id, room_id, [&missing], BEYOND).await?;
	let stored = withheld_over(services, user_id, room_id, [&accepted], STORED_WITHHELD).await?;

	backfilled_over(services, user_id, room_id, [&stored], THROUGH).await?;
	let shallow = backfilled_over(services, user_id, room_id, [&accepted], SHALLOW).await?;

	message_over(services, user_id, room_id, [gap], ABOVE_GAP).await?;

	let (_, first) = services
		.timeline
		.first_item_in_room(room_id)
		.await?;

	assert_eq!(first.event_id, shallow, "the last backfilled event is not the room's first");

	let found = edges(services, room_id).await;

	assert_eq!(found, [beyond, deep], "the scan did not rise to the events missing history");

	let prepended = ingest(services, room_id, gap_json).await?;

	assert!(prepended, "backfill withheld the plain outlier under the scan");

	let found = edges(services, room_id).await;

	assert!(found.is_empty(), "the scan found missing history in a whole room");

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn edges(services: &Services, room_id: &RoomId) -> Vec<OwnedEventId> {
	services
		.timeline
		.backfill_edges(room_id)
		.map(|pdu| pdu.event_id)
		.collect()
		.await
}

/// The backfill seeds keep the first event's frontier ahead of the scanned
/// event's.
///
/// The first event names only a soft-failed message over an accepted one whose
/// own predecessor is missing, so the scan passes it for a later event over a
/// predecessor no candidate may supply. The soft-failed message, whose request
/// reaches the missing history, still leads the seeds.
// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
pub(super) async fn backfill_seeds_keep_the_first_frontier<'a>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
) -> Result {
	let (gap, _) = sign_outlier_message(services, user_id, room_id, SEEDS_GAP).await?;
	let (lost, _) = sign_outlier_message(services, user_id, room_id, SEEDS_LOST).await?;
	let (gap, lost): (&EventId, &EventId) = (gap.event_id.as_ref(), lost.event_id.as_ref());

	let stored = backfilled_over(services, user_id, room_id, [gap], SEEDS_STORED).await?;
	let scanned = backfilled_over(services, user_id, room_id, [lost], SEEDS_SCANNED).await?;
	let withheld = withheld_over(services, user_id, room_id, [&stored], SEEDS_WITHHELD).await?;
	let first = backfilled_over(services, user_id, room_id, [&withheld], SEEDS_FIRST).await?;

	let found = edges(services, room_id).await;

	assert_eq!(found, [scanned.clone(), stored], "the scan did not pass the first event");

	let found = seeds(services, room_id, &first, &scanned).await?;

	assert_eq!(found, [withheld, scanned], "the seeds lost the first event's frontier");

	Ok(())
}

#[tracing::instrument(level = "debug", skip_all)]
async fn seeds(
	services: &Services,
	room_id: &RoomId,
	first: &EventId,
	scanned: &EventId,
) -> Result<Vec<OwnedEventId>> {
	let (_, oldest) = services
		.timeline
		.first_item_in_room(room_id)
		.await?;

	assert_eq!(oldest.event_id, first, "the last backfilled event is not the room's first");

	let scanned = services.timeline.get_pdu(scanned).await?;
	let seeds = services
		.timeline
		.backfill_seeds(room_id, &oldest, Some(&scanned))
		.collect()
		.await;

	Ok(seeds)
}

// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
async fn withheld_over<'a, const N: usize>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	prev_events: [&'a EventId; N],
	body: &'a str,
) -> Result<OwnedEventId> {
	set_forward_extremities(services, room_id, prev_events).await;
	let (pdu, json) = sign_message(services, user_id, room_id, body).await?;

	services
		.timeline
		.add_pdu_outlier(&pdu.event_id, &json);

	services
		.pdu_metadata
		.mark_event_soft_failed(&pdu.event_id);

	Ok(pdu.event_id)
}

// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
async fn backfilled_over<'a, const N: usize>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	prev_events: [&'a EventId; N],
	body: &'a str,
) -> Result<OwnedEventId> {
	set_forward_extremities(services, room_id, prev_events).await;
	let (pdu, json) = sign_message(services, user_id, room_id, body).await?;

	let prepended = ingest(services, room_id, json).await?;

	assert!(prepended, "backfill did not prepend {body:?}");

	Ok(pdu.event_id)
}

// size firewall
#[async_noinline]
#[tracing::instrument(level = "debug", skip_all)]
async fn message_over<'a, const N: usize>(
	services: &'a Services,
	user_id: &'a UserId,
	room_id: &'a RoomId,
	prev_events: [&'a EventId; N],
	body: &'a str,
) -> Result<OwnedEventId> {
	set_forward_extremities(services, room_id, prev_events).await;
	append_message(services, user_id, room_id, body).await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn restrict_history(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<OwnedEventId> {
	let content = RoomHistoryVisibilityEventContent::new(HistoryVisibility::Joined);
	let builder = PduBuilder::state("", &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await
}

#[tracing::instrument(level = "debug", skip_all)]
async fn assert_soft_failed(services: &Services, event_id: &EventId) {
	let marked = services
		.pdu_metadata
		.is_event_soft_failed(event_id)
		.await;

	assert!(marked, "the banned user's message lacks its soft-fail marker");

	let absent = services
		.timeline
		.non_outlier_pdu_exists(event_id)
		.await
		.is_not_found();

	assert!(absent, "the soft-failed message reached the timeline");

	let has_state = services
		.state
		.pdu_shortstatehash(event_id)
		.await
		.is_ok();

	assert!(has_state, "the soft-failed message has no state row");
}

#[tracing::instrument(level = "debug", skip_all)]
async fn backfill(
	services: &Services,
	base: &str,
	room_id: &RoomId,
	from: &EventId,
	limit: u32,
) -> Result<Pdus> {
	let request = BackfillRequest::new(room_id.to_owned(), vec![from.to_owned()], limit.into());
	let (status, body) = send(services, base, request).await?;

	if status != 200 {
		return Err!("backfill from {from} answered {status}: {body}");
	}

	serde_json::from_str::<Value>(&body)?
		.get("pdus")
		.and_then(Value::as_array)
		.cloned()
		.ok_or_else(|| err!("backfill response omitted pdus"))
}

#[tracing::instrument(level = "debug", skip_all)]
async fn ingest(
	services: &Services,
	room_id: &RoomId,
	json: CanonicalJsonObject,
) -> Result<bool> {
	let room_version = services.state.get_room_version(room_id).await?;
	let json = into_outgoing_federation(json, &room_version);

	services
		.timeline
		.backfill_pdu(room_id, services.globals.server_name(), to_raw_value(&json)?)
		.await
}

fn with_foreign_signature(mut json: CanonicalJsonObject) -> CanonicalJsonObject {
	let signature = CanonicalJsonValue::String("AAAA".to_owned());
	let keys = CanonicalJsonValue::Object([("ed25519:policy_server".into(), signature)].into());

	if let Some(CanonicalJsonValue::Object(signatures)) = json.get_mut("signatures") {
		signatures.insert(FOREIGN_SERVER.into(), keys);
	}

	json
}

#[tracing::instrument(level = "debug", skip_all)]
async fn frontier_below(
	services: &Services,
	room_id: &RoomId,
	oldest: &EventId,
) -> Result<Vec<OwnedEventId>> {
	let oldest = services.timeline.get_pdu(oldest).await?;
	let frontier = services
		.timeline
		.backfill_frontier(room_id, &oldest)
		.map(at!(0))
		.collect()
		.await;

	Ok(frontier)
}

fn count(pdus: &[Value], body: &str) -> usize { bodies(pdus).filter(|&b| b == body).count() }

fn bodies(pdus: &[Value]) -> impl Iterator<Item = &str> {
	pdus.iter()
		.filter_map(|pdu| pdu["content"]["body"].as_str())
}

fn has_membership(pdus: &[Value], user_id: &UserId, membership: &str) -> bool {
	pdus.iter().any(|pdu| {
		pdu["state_key"] == user_id.as_str() && pdu["content"]["membership"] == membership
	})
}
