use serde_json::{Value, value::to_raw_value};
use tuwunel_core::{
	Err, Result, async_noinline, err,
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
