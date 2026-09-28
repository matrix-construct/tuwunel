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
	utils::{BoolExt, result::NotFound, time::now_secs},
};
use tuwunel_database::Interfix;
use tuwunel_service::{
	Services,
	rooms::{
		event_handler::{PrevWalkMetrics, StateLocalMetrics},
		short::ShortStateHash,
	},
};

pub(super) type SignedPdu = (PduEvent, CanonicalJsonObject);
pub(super) type HeldFork = (PduEvent, PduEvent, PduEvent, CanonicalJsonObject);

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

// Context and Disposition mirror the event handler's private on-disk backoff discriminants.
#[derive(Clone, Copy)]
pub(super) enum Context {
	Upgrade = 2,
	Incoming = 3,
}

#[derive(Clone, Copy)]
pub(super) enum Disposition {
	Pending = 0,
	Transient = 1,
	Permanent = 2,
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

	let (held, _) = sign_outlier_message(services, user_id, room_id, "held corruption").await?;

	set_forward_extremity(services, room_id, held.event_id.as_ref()).await;

	let (top, top_json) =
		sign_outlier_message(services, user_id, room_id, "corruption top").await?;

	Ok((held, top, top_json))
}

pub(super) async fn held_state_fork(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
) -> Result<HeldFork> {
	let (left, left_json) = sign_state(services, user_id, room_id, "fork left").await?;
	let (right, right_json) = sign_state(services, user_id, room_id, "fork right").await?;
	let (top, top_json) = held_fork(
		services,
		user_id,
		room_id,
		(&left, &left_json),
		(&right, &right_json),
		"fork top",
	)
	.await?;

	Ok((left, right, top, top_json))
}

pub(super) async fn held_fork(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	(left, left_json): (&PduEvent, &CanonicalJsonObject),
	(right, right_json): (&PduEvent, &CanonicalJsonObject),
	top_body: &str,
) -> Result<SignedPdu> {
	services
		.timeline
		.add_pdu_outlier(&left.event_id, left_json);

	services
		.timeline
		.add_pdu_outlier(&right.event_id, right_json);

	set_forward_extremities(services, room_id, [left.event_id.as_ref(), right.event_id.as_ref()])
		.await;

	sign_outlier_message(services, user_id, room_id, top_body).await
}

pub(super) async fn append_state(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	name: &str,
) -> Result<OwnedEventId> {
	let builder = room_name(name);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.build_and_append_pdu(builder, user_id, room_id, &state_lock)
		.await
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

fn room_name(name: &str) -> PduBuilder {
	PduBuilder::state("", &RoomNameEventContent::new(name.to_owned()))
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

	let absent = map.exists(&key).await.is_not_found();

	assert!(absent, "raw short-id mutation left {map_name}[{short}] readable");

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
			let pdu = serde_json::from_slice(stored.as_ref()).map(malform_membership)?;

			pdus.insert(&pdu_id, serde_json::to_vec(&pdu)?);
		},
	}

	match cache_handling {
		| CacheHandling::Clear => services.clear_cache().await,
		| CacheHandling::Preserve => {},
	}

	let result = services.timeline.get_pdu_from_id(&pdu_id).await;

	match (failure, result) {
		| (PduFailure::Missing, Err(error)) if error.is_not_found() => Ok(()),
		| (PduFailure::MalformedMembership, Ok(_)) => Ok(()),
		| (PduFailure::Missing, result) =>
			Err!("missing timeline PDU {event_id} returned {result:?}"),
		| (PduFailure::MalformedMembership, result) =>
			Err!("valid malformed-membership PDU {event_id} returned {result:?}"),
	}
}

fn malform_membership(mut pdu: Value) -> Value {
	pdu["content"]["membership"] = Value::Bool(true);
	pdu
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
	let absent = memo.exists(event_id).await.is_not_found();

	assert!(absent, "failed fork {event_id} wrote a resolved-state memo");

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
	let event_id: &EventId = incoming.event_id.as_ref();
	let room_version = services
		.state
		.get_room_version(room_id)
		.await
		.map_err(|error| err!("{context} failed to load the room version: {error}"))?;

	let incoming_json = into_outgoing_federation(incoming_json, &room_version);
	let before = services.event_handler.state_local_metrics();

	let result = services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			room_id,
			event_id,
			incoming_json,
			true,
		)
		.await;

	let after = services.event_handler.state_local_metrics();

	let Err(error) = result else {
		return Err!("{context} did not fall through to federation fetch");
	};

	if error
		.to_string()
		.contains("no candidate servers available")
		.is_false()
	{
		return Err!("{context} failed before federation fetch: {error}");
	}

	assert_one_settled_walk(before, after, expected, context);

	let absent = services
		.timeline
		.non_outlier_pdu_exists(event_id)
		.await
		.is_not_found();

	assert!(absent, "{context} unexpectedly reached the timeline");

	let retained = services.timeline.pdu_exists(event_id).await;

	assert!(retained, "{context} was not retained as an outlier");

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
	let delta = field_delta(before, after, context);

	StateLocalMetrics {
		walk_attempts: delta(|metrics| metrics.walk_attempts),
		walk_resolved: delta(|metrics| metrics.walk_resolved),
		fallback_absent: delta(|metrics| metrics.fallback_absent),
		fallback_ceiling: delta(|metrics| metrics.fallback_ceiling),
		fallback_auth_missing: delta(|metrics| metrics.fallback_auth_missing),
		fallback_all_committed: delta(|metrics| metrics.fallback_all_committed),
		fallback_entries: delta(|metrics| metrics.fallback_entries),
		fallback_canary: delta(|metrics| metrics.fallback_canary),
		fallback_create_mismatch: delta(|metrics| metrics.fallback_create_mismatch),
		fallback_unevaluable: delta(|metrics| metrics.fallback_unevaluable),
		fallback_error: delta(|metrics| metrics.fallback_error),
		walk_failures: delta(|metrics| metrics.walk_failures),
		..StateLocalMetrics::default()
	}
}

pub(super) fn counter_delta(after: u64, before: u64, context: &str) -> u64 {
	after
		.checked_sub(before)
		.unwrap_or_else(|| panic!("{context} walk counter decreased"))
}

fn field_delta<'a, Metrics>(
	before: &'a Metrics,
	after: &'a Metrics,
	context: &'a str,
) -> impl Fn(fn(&Metrics) -> u64) -> u64 + 'a {
	move |counter| counter_delta(counter(after), counter(before), context)
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

pub(super) fn assert_prev_walk(
	before: PrevWalkMetrics,
	after: PrevWalkMetrics,
	expected: PrevWalkMetrics,
	context: &str,
) {
	let actual = prev_walk_metrics_delta(&before, &after, context);
	let gapped_ends: u64 = [
		actual.held,
		actual.closed,
		actual.fetch_failed,
		actual.fetch_cancelled,
		actual.walked,
	]
	.into_iter()
	.sum();

	let walk_ends: u64 = [actual.appended, actual.not_appended, actual.failed, actual.cancelled]
		.into_iter()
		.sum();

	assert_eq!(actual.gapped, gapped_ends, "{context} left a gapped event without an end");
	assert_eq!(actual.walked, walk_ends, "{context} left a walk without an outcome");
	assert_eq!(actual, expected, "{context} miscounted its prev walk");
}

fn prev_walk_metrics_delta(
	before: &PrevWalkMetrics,
	after: &PrevWalkMetrics,
	context: &str,
) -> PrevWalkMetrics {
	let delta = field_delta(before, after, context);

	// Exhaustive on purpose: a new counter fails to compile until it is covered here.
	PrevWalkMetrics {
		entered: delta(|metrics| metrics.entered),
		gapped: delta(|metrics| metrics.gapped),
		held: delta(|metrics| metrics.held),
		closed: delta(|metrics| metrics.closed),
		fetch_failed: delta(|metrics| metrics.fetch_failed),
		fetch_cancelled: delta(|metrics| metrics.fetch_cancelled),
		walked: delta(|metrics| metrics.walked),
		walked_prevs: delta(|metrics| metrics.walked_prevs),
		capped: delta(|metrics| metrics.capped),
		appended: delta(|metrics| metrics.appended),
		not_appended: delta(|metrics| metrics.not_appended),
		failed: delta(|metrics| metrics.failed),
		cancelled: delta(|metrics| metrics.cancelled),
		unprocessed_prevs: delta(|metrics| metrics.unprocessed_prevs),
	}
}

pub(super) async fn assert_accepts(
	services: &Services,
	room_id: &RoomId,
	incoming: &PduEvent,
	incoming_json: CanonicalJsonObject,
	context: &str,
) -> Result {
	let absent = services
		.timeline
		.non_outlier_pdu_exists(incoming.event_id.as_ref())
		.await
		.is_not_found();

	assert!(absent, "{context} unexpectedly started in the timeline");

	let handled = redeliver(services, room_id, incoming, incoming_json, context).await?;

	assert!(handled, "{context} did not continue through local state");

	services
		.timeline
		.non_outlier_pdu_exists(incoming.event_id.as_ref())
		.await
		.map_err(|error| err!("{context} did not reach the timeline: {error}"))
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

pub(super) async fn sign_outlier_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	body: &str,
) -> Result<SignedPdu> {
	let (pdu, pdu_json) = sign_message(services, user_id, room_id, body).await?;

	services
		.timeline
		.add_pdu_outlier(&pdu.event_id, &pdu_json);

	Ok((pdu, pdu_json))
}

pub(super) async fn sign_message(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	body: &str,
) -> Result<SignedPdu> {
	let builder = PduBuilder::timeline(&RoomMessageEventContent::text_plain(body));
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

pub(super) async fn sign_state(
	services: &Services,
	user_id: &UserId,
	room_id: &RoomId,
	name: &str,
) -> Result<SignedPdu> {
	let builder = room_name(name);
	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.timeline
		.create_hash_and_sign_event(builder, user_id, room_id, &state_lock)
		.await
}

pub(super) fn suppress_upgrade(services: &Services, event_id: &EventId) -> Result {
	let forever = u64::MAX;

	plant_backoff_row(services, Context::Upgrade, event_id, 0, Disposition::Permanent, forever)
}

pub(super) fn plant_backoff_rows(
	services: &Services,
	context: Context,
	event_id: &EventId,
	disposition: Disposition,
	rows: u32,
) -> Result {
	let now = now_secs();
	let minute = u32::try_from(now / 60)?;
	let ages = 1..=rows;

	ages.map(|age| minute.saturating_sub(age))
		.try_for_each(|bucket| {
			plant_backoff_row(services, context, event_id, bucket, disposition, now)
		})
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

pub(super) async fn backoff_rows(
	services: &Services,
	context: Context,
	event_id: &EventId,
) -> Result<usize> {
	let rows = services
		.db
		.get("eventid_backoff")?
		.count_prefix(&(u8::from(context), event_id, Interfix))
		.await;

	Ok(rows)
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

	response
		.get("room_id")
		.and_then(Value::as_str)
		.ok_or_else(|| err!("createRoom response omitted room_id"))?
		.try_into()
		.map_err(Into::into)
}
