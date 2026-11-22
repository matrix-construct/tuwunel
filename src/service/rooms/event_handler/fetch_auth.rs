use std::{
	collections::{HashSet, VecDeque},
	time::Duration,
};

use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId, RoomVersionId,
	ServerName,
};
use tuwunel_core::{
	async_noinline, debug, debug_error, debug_warn, expected, implement, trace,
	utils::stream::{BroadbandExt, IterStream, ReadyExt},
	warn,
};
use tuwunel_matrix::{PduEvent, pdu::MAX_AUTH_EVENTS};

use super::{
	backoff::{Context, Disposition},
	eval::Eval,
};
use crate::fetcher::{Op, Opts};

type Fetched = Vec<(PduEvent, Option<CanonicalJsonObject>)>;

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
	eval @ Eval {
		origin,
		room_id,
		room_version,
		recursion_level,
		..
	}: Eval<'_>,
	events: Events,
) -> Fetched
where
	Events: Iterator<Item = &'a EventId> + Clone + Send,
{
	let events_with_auth_events: Vec<_> = events
		.stream()
		.broad_then(|event_id| self.fetch_auth_chain(origin, room_id, event_id, room_version))
		.collect()
		.boxed() // size firewall
		.await;

	events_with_auth_events
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

			self.admit_chain(eval, &id, events_in_reverse_order, pdus)
				.await
		})
		.await
}

#[implement(super::Service)]
#[tracing::instrument(
	name = "chain",
	level = "trace",
	skip_all,
	fields(%event_id),
)]
async fn fetch_auth_chain(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	event_id: &EventId,
	room_version: &RoomVersionId,
) -> (OwnedEventId, Option<PduEvent>, Vec<(OwnedEventId, CanonicalJsonObject)>) {
	// a. Look in the main timeline (pduid_pdu tree)
	// b. Look at outlier pdu tree
	// (get_pdu_json checks both)
	if let Ok(local_pdu) = self.services.timeline.get_pdu(event_id).await {
		trace!(?event_id, "Found in database");
		return (event_id.to_owned(), Some(local_pdu), vec![]);
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

		let Ok(value) = serde_json::from_slice::<CanonicalJsonObject>(&outcome.bytes) else {
			self.record_outcome(Context::Fetch, &next_id, Disposition::Transient);
			continue;
		};

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

		events_in_reverse_order.push((next_id.clone(), value));
		events_all.insert(next_id);
	}

	(event_id.to_owned(), None, events_in_reverse_order)
}

/// Admits a fetched auth chain in the reverse of its discovery order.
///
/// Every event of the chain is admitted, but only the one it was fetched for is
/// added to `pdus`.
#[implement(super::Service)]
async fn admit_chain(
	&self,
	eval: Eval<'_>,
	event_id: &EventId,
	chain_in_reverse_order: Vec<(OwnedEventId, CanonicalJsonObject)>,
	pdus: Fetched,
) -> Fetched {
	chain_in_reverse_order
		.into_iter()
		.rev()
		.stream()
		.filter_map(async |(next_id, pdu_json)| {
			let requested = *next_id == *event_id;

			self.admit_fetched(eval, &next_id, pdu_json)
				.map(|admitted| admitted.filter(|_| requested))
				.await
		})
		.ready_fold(pdus, |mut pdus, (pdu, pdu_json)| {
			pdus.push((pdu, Some(pdu_json)));
			pdus
		})
		.await
}

/// Admits an event fetched with its auth chain as an outlier, one level deeper
/// than `eval` with its auth events known.
///
/// An event backing off from a failed admission is skipped; a failure records a
/// transient backoff.
#[implement(super::Service)]
#[tracing::instrument(name = "admit", level = "trace", skip_all, fields(%event_id))]
async fn admit_fetched(
	&self,
	eval @ Eval { recursion_level, .. }: Eval<'_>,
	event_id: &EventId,
	pdu_json: CanonicalJsonObject,
) -> Option<(PduEvent, CanonicalJsonObject)> {
	if self
		.is_suppressed(Context::Auth, event_id, Duration::from_mins(5)..Duration::from_hours(24))
		.await
		.is_deny()
	{
		return None;
	}

	let fetched = Eval {
		event_id,
		recursion_level: expected!(recursion_level + 1),
		auth_events_known: true,
		..eval
	};

	let admitted = self.admit_outlier(fetched, pdu_json).await;

	match admitted {
		| Some(_) => self.record_success(Context::Auth, event_id).await,
		| None => self.record_outcome(Context::Auth, event_id, Disposition::Transient),
	}

	admitted
}

#[implement(super::Service)]
// recursion cycle, whose Send proof overflows the solver's depth unless erased
#[async_noinline]
async fn admit_outlier<'a>(
	&'a self,
	eval: Eval<'a>,
	pdu_json: CanonicalJsonObject,
) -> Option<(PduEvent, CanonicalJsonObject)> {
	self.handle_outlier_pdu(eval, pdu_json)
		.inspect_err(|error| {
			warn!(event_id = %eval.event_id, ?error, "Authentication of fetched event failed");
		})
		.map(Result::ok)
		.await
}
