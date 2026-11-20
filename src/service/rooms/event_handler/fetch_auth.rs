use std::{
	collections::{HashSet, VecDeque},
	sync::atomic::{AtomicUsize, Ordering},
	time::Duration,
};

use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId, RoomVersionId,
	ServerName,
};
use tuwunel_core::{
	Err, Result, debug, debug_error, debug_warn, err, expected, implement, trace,
	utils::stream::{BroadbandExt, IterStream},
	warn,
};
use tuwunel_matrix::{
	PduEvent,
	pdu::{MAX_AUTH_EVENTS, MAX_PDU_BYTES},
};

use super::backoff::{Context, Disposition};
use crate::fetcher::{Op, Opts};

/// Each root walk has a count limit; all roots in a lookup share a byte limit.
pub(super) struct AuthFetchBudget {
	pub(super) limit: usize,
	remaining: AtomicUsize,
}

impl AuthFetchBudget {
	fn new(limit: u16) -> Self {
		let limit = usize::from(limit);
		Self {
			limit,
			remaining: AtomicUsize::new(limit.saturating_mul(MAX_PDU_BYTES)),
		}
	}

	fn check_count(&self, fetched: usize) -> Result {
		if fetched >= self.limit {
			return Err!(Request(TooLarge("Auth chain fetch count limit reached")));
		}

		Ok(())
	}

	fn charge(&self, bytes: usize) -> Result {
		self.remaining
			.try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_sub(bytes))
			.map(|_| ())
			.map_err(|_| err!(Request(TooLarge("Auth chain fetch byte limit reached"))))
	}
}

#[implement(super::Service)]
pub(super) fn auth_fetch_budget(&self) -> AuthFetchBudget {
	AuthFetchBudget::new(
		self.services
			.server
			.config
			.federation
			.history
			.max_fetch_prev_events,
	)
}

/// Find the event and auth it. Once the event is validated (steps 1 - 8)
/// it is appended to the outliers Tree.
///
/// Returns pdu and if we fetched it over federation the raw json.
///
/// a. Look in the main timeline (pduid_pdu tree)
/// b. Look at outlier pdu tree
/// c. Ask origin server over federation
/// d. TODO: Ask other servers over federation?
#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(
		%origin,
		events = %events.clone().count(),
		lev = %recursion_level,
	),
)]
pub(super) async fn fetch_auth<'a, Events>(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	events: Events,
	room_version: &RoomVersionId,
	recursion_level: usize,
	budget: &AuthFetchBudget,
) -> Result<Vec<(PduEvent, Option<CanonicalJsonObject>)>>
where
	Events: Iterator<Item = &'a EventId> + Clone + Send,
{
	let events_with_auth_events: Vec<_> = events
		.stream()
		.broad_then(|event_id| {
			self.fetch_auth_chain(origin, room_id, event_id, room_version, budget)
		})
		.try_collect()
		.boxed() // size firewall
		.await?;

	let pdus = events_with_auth_events
		.into_iter()
		.stream()
		.fold(Vec::new(), async |mut pdus, (id, local_pdu, events_in_reverse_order)| {
			if self.services.server.check_running().is_err() {
				return pdus;
			}

			// a. Look in the main timeline (pduid_pdu tree)
			// b. Look at outlier pdu tree
			// (get_pdu_json checks both)
			if let Some(local_pdu) = local_pdu {
				pdus.push((local_pdu, None));
			}

			events_in_reverse_order
				.into_iter()
				.rev()
				.stream()
				.fold(pdus, async |mut pdus, (next_id, pdu_json)| {
					if self
						.is_suppressed(
							Context::Auth,
							&next_id,
							Duration::from_mins(5)..Duration::from_hours(24),
						)
						.await
						.is_deny()
					{
						return pdus;
					}

					let outlier = async {
						let value = serde_json::from_slice(&pdu_json)?;
						// recursion cycle
						Box::pin(self.handle_outlier_pdu(
							origin,
							room_id,
							&next_id,
							value,
							room_version,
							expected!(recursion_level + 1),
							budget,
							true,
						))
						.await
					};

					if let Ok((pdu, json)) = outlier
						.await
						.inspect_err(|e| warn!("Authentication of event {next_id} failed: {e:?}"))
					{
						if next_id == id {
							pdus.push((pdu, Some(json)));
						}
						self.record_success(Context::Auth, &next_id).await;
					} else {
						self.record_outcome(Context::Auth, &next_id, Disposition::Transient);
					}

					pdus
				})
				.await
		})
		.await;

	Ok(pdus)
}

#[implement(super::Service)]
#[tracing::instrument(
	name = "chain",
	level = "trace",
	skip_all,
	fields(%event_id),
)]
#[expect(clippy::type_complexity)]
async fn fetch_auth_chain(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	event_id: &EventId,
	room_version: &RoomVersionId,
	budget: &AuthFetchBudget,
) -> Result<(OwnedEventId, Option<PduEvent>, Vec<(OwnedEventId, Vec<u8>)>)> {
	// a. Look in the main timeline (pduid_pdu tree)
	// b. Look at outlier pdu tree
	// (get_pdu_json checks both)
	if let Ok(local_pdu) = self.services.timeline.get_pdu(event_id).await {
		trace!(?event_id, "Found in database");
		return Ok((event_id.to_owned(), Some(local_pdu), vec![]));
	}

	// c. Ask origin server over federation
	// We also handle its auth chain here so we don't get a stack overflow in
	// handle_outlier_pdu.
	let mut events_all = HashSet::new();
	let mut events_in_reverse_order = Vec::new();
	let mut todo_auth_events: VecDeque<_> = [event_id.to_owned()].into();
	while let Some(next_id) = todo_auth_events.pop_front() {
		if events_all.contains(&next_id) {
			continue;
		}

		if self
			.is_suppressed(
				Context::Fetch,
				&next_id,
				Duration::from_mins(2)..Duration::from_hours(8),
			)
			.await
			.is_deny()
		{
			debug_warn!("Backed off from {next_id}");
			continue;
		}

		if self.services.timeline.pdu_exists(&next_id).await {
			trace!(?next_id, "Found in database");
			continue;
		}

		if self.services.server.check_running().is_err() {
			debug_warn!(?next_id, "Server shutting down");
			break;
		}

		budget.check_count(events_in_reverse_order.len())?;

		debug!("Fetching {next_id} over federation.");
		let opts = Opts::new(Op::AuthEvent, room_id.to_owned())
			.event_id(next_id.clone())
			.hint(origin.to_owned())
			.room_version(room_version.to_owned())
			.attempt_limit(super::EVENT_FETCH_ATTEMPT_LIMIT)
			.fanout_for_op();

		let Ok(outcome) = self
			.services
			.fetcher
			.fetch(opts)
			.inspect_err(|e| debug_error!(?next_id, "Failed to fetch event: {e}"))
			.await
		else {
			debug_warn!("Backing off from {next_id}");
			self.record_outcome(Context::Fetch, &next_id, Disposition::Transient);
			continue;
		};

		let Some((value, pdu_json)) = parse_fetched_pdu(&outcome.bytes) else {
			self.record_outcome(Context::Fetch, &next_id, Disposition::Transient);
			continue;
		};

		budget.charge(pdu_json.len())?;

		debug!("Got {next_id} over federation");
		self.record_success(Context::Fetch, &next_id)
			.await;
		value
			.get("auth_events")
			.and_then(CanonicalJsonValue::as_array)
			.into_iter()
			.flatten()
			.filter_map(|auth_event| auth_event.try_into().ok())
			.take(MAX_AUTH_EVENTS)
			.for_each(|auth_event: &EventId| {
				todo_auth_events.push_back(auth_event.to_owned());
			});

		events_in_reverse_order.push((next_id.clone(), pdu_json));
		events_all.insert(next_id);
	}

	Ok((event_id.to_owned(), None, events_in_reverse_order))
}

/// Parse an event fetched by the auth chain walk, returning it both parsed and
/// as canonical JSON. `unsigned` is removed, as handle_outlier_pdu removes it
/// before checking the PDU size limit, and an event still larger than that
/// limit is rejected. The walk keeps only the JSON, as a parsed event can take
/// many times its serialized size in memory.
fn parse_fetched_pdu(bytes: &[u8]) -> Option<(CanonicalJsonObject, Vec<u8>)> {
	let mut value: CanonicalJsonObject = serde_json::from_slice(bytes).ok()?;
	value.remove("unsigned");

	let pdu_json = serde_json::to_vec(&value).ok()?;
	(pdu_json.len() <= MAX_PDU_BYTES).then_some((value, pdu_json))
}

#[cfg(test)]
mod tests;
