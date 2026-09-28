use std::iter::once;

use serde_json::{Value, json};
use tuwunel_core::{
	Err, Result, err,
	matrix::{PduEvent, pdu::into_outgoing_federation},
	pdu::PduBuilder,
	ruma::{
		CanonicalJsonObject, EventId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId, UserId,
		events::room::{message::RoomMessageEventContent, name::RoomNameEventContent},
	},
};
use tuwunel_service::{
	Services,
	rooms::{event_handler::StateLocalMetrics, short::ShortStateHash},
};

#[derive(Clone, Copy)]
pub(super) enum PduFailure {
	Missing,
	MalformedMembership,
}

#[derive(Clone, Copy)]
pub(super) enum CacheHandling {
	Clear,
	Preserve,
}

// Mirrors the event handler's private on-disk backoff discriminants.
#[derive(Clone, Copy)]
pub(super) enum Context {
	Upgrade,
	Incoming,
}

#[derive(Clone, Copy)]
pub(super) enum Disposition {
	Pending,
	Transient,
	Permanent,
}

#[derive(Clone, Copy)]
pub(super) enum ExpectedWalkOutcome {
	Resolved,
	AllCommitted,
	Unevaluable,
}

impl From<Context> for u8 {
	fn from(context: Context) -> Self {
		match context {
			| Context::Upgrade => 2,
			| Context::Incoming => 3,
		}
	}
}

impl From<Disposition> for u64 {
	fn from(disposition: Disposition) -> Self {
		match disposition {
			| Disposition::Pending => 0,
			| Disposition::Transient => 1,
			| Disposition::Permanent => 2,
		}
	}
}

pub(super) async fn held_message_chain(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	boundary: &EventId,
) -> Result<(PduEvent, PduEvent, CanonicalJsonObject)> {
	set_forward_extremity(services, room_id, boundary).await;

	let (held, held_json) = sign_message(services, user_id, room_id, "held corruption").await?;

	services
		.timeline
		.add_pdu_outlier(&held.event_id, &held_json);

	set_forward_extremity(services, room_id, held.event_id.as_ref()).await;

	let (top, top_json) = sign_message(services, user_id, room_id, "corruption top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	Ok((held, top, top_json))
}

pub(super) async fn held_state_fork(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<(PduEvent, PduEvent, PduEvent, CanonicalJsonObject)> {
	let (left, left_json) = sign_state(services, user_id, room_id, "fork left").await?;
	let (right, right_json) = sign_state(services, user_id, room_id, "fork right").await?;

	services
		.timeline
		.add_pdu_outlier(&left.event_id, &left_json);

	services
		.timeline
		.add_pdu_outlier(&right.event_id, &right_json);

	set_forward_extremities(services, room_id, [left.event_id.as_ref(), right.event_id.as_ref()])
		.await;

	let (top, top_json) = sign_message(services, user_id, room_id, "fork top").await?;

	services
		.timeline
		.add_pdu_outlier(&top.event_id, &top_json);

	Ok((left, right, top, top_json))
}

pub(super) async fn append_state(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	name: &str,
) -> Result<OwnedEventId> {
	let content = RoomNameEventContent::new(name.to_owned());
	let builder = PduBuilder::state(String::new(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await
}

pub(super) async fn remove_short_row(services: &Services, map_name: &str, short: u64) -> Result {
	let map = services
		.db
		.get(map_name)
		.map_err(|error| err!("short-id map {map_name} unavailable for {short}: {error}"))?;

	let key = short.to_be_bytes();

	map.exists(&key)
		.await
		.map_err(|error| err!("short-id row {map_name}[{short}] unavailable: {error}"))?;

	map.remove(&key);
	services.clear_cache().await;

	assert!(
		map.exists(&key)
			.await
			.is_err_and(|error| error.is_not_found()),
		"raw short-id mutation left {map_name}[{short}] readable"
	);

	Ok(())
}

pub(super) async fn corrupt_timeline_pdu(
	services: &Services,
	event_id: &EventId,
	failure: PduFailure,
	cache_handling: CacheHandling,
) -> Result {
	let pdu_id = services
		.timeline
		.get_pdu_id(event_id)
		.await
		.map_err(|error| err!("timeline PDU {event_id} has no raw id: {error}"))?;

	let pdus = services
		.db
		.get("pduid_pdu")
		.map_err(|error| err!("timeline PDU map unavailable for {event_id}: {error}"))?;

	pdus.exists(&pdu_id)
		.await
		.map_err(|error| err!("timeline PDU row unavailable for {event_id}: {error}"))?;

	match failure {
		| PduFailure::Missing => {
			pdus.remove(&pdu_id);
		},
		| PduFailure::MalformedMembership => {
			let stored = pdus.get(&pdu_id).await?;
			let mut pdu: Value = serde_json::from_slice(stored.as_ref())?;

			pdu["content"]["membership"] = Value::Bool(true);

			pdus.insert(&pdu_id, serde_json::to_vec(&pdu)?);
		},
	}

	match cache_handling {
		| CacheHandling::Clear => services.clear_cache().await,
		| CacheHandling::Preserve => {},
	}

	let result = services.timeline.get_pdu_from_id(&pdu_id).await;

	match (failure, result) {
		| (PduFailure::Missing, Err(error)) if error.is_not_found() => {},
		| (PduFailure::MalformedMembership, Ok(_)) => {},
		| (PduFailure::Missing, result) =>
			return Err!("missing timeline PDU {event_id} returned {result:?}"),
		| (PduFailure::MalformedMembership, result) =>
			return Err!("valid malformed-membership PDU {event_id} returned {result:?}"),
	}

	Ok(())
}

pub(super) async fn assert_unevaluable(
	services: &Services,
	event_id: &EventId,
	context: &str,
) -> Result {
	let report = services
		.event_handler
		.local_state_report(event_id)
		.await?;

	assert!(report.visited > 0, "{context} did not exercise the local walk");
	assert_eq!(report.gate_drops, 0, "{context} became a denial");
	assert_eq!(
		report.fallback.as_deref(),
		Some("unevaluable"),
		"{context} used the wrong fallback",
	);

	assert_eq!(report.state_len, None, "{context} produced state");

	Ok(())
}

pub(super) async fn assert_no_memo(services: &Services, event_id: &EventId) -> Result {
	let memo = services.db.get("eventid_resolvedstate")?;

	assert!(
		memo.exists(event_id)
			.await
			.is_err_and(|error| error.is_not_found()),
		"failed fork {event_id} wrote a resolved-state memo"
	);

	Ok(())
}

pub(super) async fn assert_fetches(
	services: &Services,
	room_id: &RoomId,
	incoming: &PduEvent,
	incoming_json: CanonicalJsonObject,
	expected: ExpectedWalkOutcome,
	context: &str,
) -> Result {
	let room_version = match services.state.get_room_version(room_id).await {
		| Ok(room_version) => room_version,
		| Err(error) => return Err!("{context} failed to load the room version: {error}"),
	};

	let incoming_json = into_outgoing_federation(incoming_json, &room_version);
	let before = services.event_handler.state_local_metrics();

	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			incoming.event_id.as_ref(),
			incoming_json,
			true,
		)
		.await;

	let after = services.event_handler.state_local_metrics();

	let Err(error) = result else {
		return Err!("{context} did not fall through to federation fetch");
	};

	if !error
		.to_string()
		.contains("no candidate servers available")
	{
		return Err!("{context} failed before federation fetch: {error}");
	}

	assert_one_settled_walk(before, after, expected, context);

	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(incoming.event_id.as_ref())
			.await
			.is_err_and(|error| error.is_not_found()),
		"{context} unexpectedly reached the timeline"
	);

	assert!(
		services
			.timeline
			.pdu_exists(incoming.event_id.as_ref())
			.await,
		"{context} was not retained as an outlier"
	);

	Ok(())
}

pub(super) fn assert_one_settled_walk(
	before: StateLocalMetrics,
	after: StateLocalMetrics,
	expected: ExpectedWalkOutcome,
	context: &str,
) {
	let actual = walk_metrics_delta(&before, &after, context);
	let expected = expected_walk_metrics(expected);

	assert_eq!(actual, expected, "{context} used the wrong local walk outcome");
	assert_eq!(settled_walks(&actual), 1, "{context} did not settle exactly once");
}

fn walk_metrics_delta(
	before: &StateLocalMetrics,
	after: &StateLocalMetrics,
	context: &str,
) -> StateLocalMetrics {
	StateLocalMetrics {
		walk_attempts: counter_delta(after.walk_attempts, before.walk_attempts, context),
		walk_resolved: counter_delta(after.walk_resolved, before.walk_resolved, context),
		fallback_absent: counter_delta(after.fallback_absent, before.fallback_absent, context),
		fallback_ceiling: counter_delta(after.fallback_ceiling, before.fallback_ceiling, context),
		fallback_auth_missing: counter_delta(
			after.fallback_auth_missing,
			before.fallback_auth_missing,
			context,
		),
		fallback_all_committed: counter_delta(
			after.fallback_all_committed,
			before.fallback_all_committed,
			context,
		),
		fallback_entries: counter_delta(after.fallback_entries, before.fallback_entries, context),
		fallback_canary: counter_delta(after.fallback_canary, before.fallback_canary, context),
		fallback_create_mismatch: counter_delta(
			after.fallback_create_mismatch,
			before.fallback_create_mismatch,
			context,
		),
		fallback_unevaluable: counter_delta(
			after.fallback_unevaluable,
			before.fallback_unevaluable,
			context,
		),
		fallback_error: counter_delta(after.fallback_error, before.fallback_error, context),
		walk_failures: counter_delta(after.walk_failures, before.walk_failures, context),
		..StateLocalMetrics::default()
	}
}

fn counter_delta(after: u64, before: u64, context: &str) -> u64 {
	after
		.checked_sub(before)
		.unwrap_or_else(|| panic!("{context} local walk counter decreased"))
}

fn expected_walk_metrics(outcome: ExpectedWalkOutcome) -> StateLocalMetrics {
	match outcome {
		| ExpectedWalkOutcome::Resolved => StateLocalMetrics {
			walk_attempts: 1,
			walk_resolved: 1,
			..StateLocalMetrics::default()
		},
		| ExpectedWalkOutcome::AllCommitted => StateLocalMetrics {
			walk_attempts: 1,
			fallback_all_committed: 1,
			..StateLocalMetrics::default()
		},
		| ExpectedWalkOutcome::Unevaluable => StateLocalMetrics {
			walk_attempts: 1,
			fallback_unevaluable: 1,
			..StateLocalMetrics::default()
		},
	}
}

fn settled_walks(metrics: &StateLocalMetrics) -> u64 {
	[
		metrics.walk_resolved,
		metrics.fallback_absent,
		metrics.fallback_ceiling,
		metrics.fallback_auth_missing,
		metrics.fallback_all_committed,
		metrics.fallback_entries,
		metrics.fallback_canary,
		metrics.fallback_create_mismatch,
		metrics.fallback_unevaluable,
		metrics.fallback_error,
		metrics.walk_failures,
	]
	.into_iter()
	.sum()
}

pub(super) async fn assert_accepts(
	services: &Services,
	room_id: &RoomId,
	incoming: &PduEvent,
	incoming_json: CanonicalJsonObject,
	context: &str,
) -> Result {
	assert!(
		services
			.timeline
			.non_outlier_pdu_exists(incoming.event_id.as_ref())
			.await
			.is_err_and(|error| error.is_not_found()),
		"{context} unexpectedly started in the timeline"
	);

	let handled = redeliver(services, room_id, incoming, incoming_json, context).await?;

	assert!(handled, "{context} did not continue through local state");
	match services
		.timeline
		.non_outlier_pdu_exists(incoming.event_id.as_ref())
		.await
	{
		| Ok(()) => Ok(()),
		| Err(error) => Err!("{context} did not reach the timeline: {error}"),
	}
}

pub(super) async fn redeliver(
	services: &Services,
	room_id: &RoomId,
	incoming: &PduEvent,
	incoming_json: CanonicalJsonObject,
	context: &str,
) -> Result<bool> {
	let room_version = services
		.state
		.get_room_version(room_id)
		.await
		.map_err(|error| err!("{context} failed to load the room version: {error}"))?;

	let incoming_json = into_outgoing_federation(incoming_json, &room_version);

	services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			incoming.event_id.as_ref(),
			incoming_json,
			true,
		)
		.await
		.map(|handled| handled.is_some())
		.map_err(|error| err!("{context} failed to handle the incoming PDU: {error}"))
}

pub(super) async fn set_forward_extremities<const N: usize>(
	services: &Services,
	room_id: &RoomId,
	event_ids: [&EventId; N],
) {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, event_ids.into_iter(), &state_lock)
		.await;
}

pub(super) async fn append_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	body: &str,
) -> Result<OwnedEventId> {
	let builder = PduBuilder::timeline(&RoomMessageEventContent::text_plain(body));
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await
}

pub(super) async fn sign_state(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	name: &str,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let content = RoomNameEventContent::new(name.to_owned());
	let builder = PduBuilder::state(String::new(), &content);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

pub(super) async fn set_forward_extremity(
	services: &Services,
	room_id: &RoomId,
	event_id: &EventId,
) {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_forward_extremities(room_id, once(event_id), &state_lock)
		.await;
}

pub(super) async fn restore_room_state(
	services: &Services,
	room_id: &RoomId,
	shortstatehash: ShortStateHash,
	event_id: &EventId,
) {
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.set_room_state(room_id, shortstatehash, &state_lock);

	services
		.state
		.set_forward_extremities(room_id, once(event_id), &state_lock)
		.await;
}

pub(super) async fn sign_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	body: &str,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let builder = PduBuilder::timeline(&RoomMessageEventContent::text_plain(body));
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

pub(super) fn suppress_upgrade(services: &Services, event_id: &EventId) -> Result {
	plant_backoff_row(services, Context::Upgrade, event_id, 0, Disposition::Permanent, u64::MAX)
}

pub(super) fn plant_backoff_row(
	services: &Services,
	context: Context,
	event_id: &EventId,
	bucket: u32,
	disposition: Disposition,
	secs: u64,
) -> Result {
	services
		.db
		.get("eventid_backoff")?
		.put((u8::from(context), event_id, bucket), (u64::from(disposition), secs));

	Ok(())
}

pub(super) async fn create_room(
	services: &Services,
	base: &str,
	token: &str,
) -> Result<OwnedRoomId> {
	create_room_with_body(services, base, token, json!({})).await
}

pub(super) async fn create_room_version(
	services: &Services,
	base: &str,
	token: &str,
	room_version: &RoomVersionId,
) -> Result<OwnedRoomId> {
	create_room_with_body(services, base, token, json!({ "room_version": room_version.as_str() }))
		.await
}

async fn create_room_with_body(
	services: &Services,
	base: &str,
	token: &str,
	body: Value,
) -> Result<OwnedRoomId> {
	let response = services
		.client
		.clients
		.default
		.post(format!("{base}/_matrix/client/v3/createRoom"))
		.bearer_auth(token)
		.json(&body)
		.send()
		.await?
		.error_for_status()?
		.json::<Value>()
		.await?;

	let room_id = response
		.get("room_id")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("createRoom response omitted room_id"))?;

	Ok(room_id.try_into()?)
}
