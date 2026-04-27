use std::{
	collections::{HashSet, VecDeque},
	ops::Range,
	time::Duration,
};

use futures::{FutureExt, StreamExt, TryFutureExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, OwnedEventId, RoomId, RoomVersionId,
	ServerName, api::federation::event::get_event,
};
use tuwunel_core::{
	debug, debug_error, debug_warn, expected, implement,
	matrix::{PduEvent, event::gen_event_id_canonical_json, pdu::MAX_AUTH_EVENTS},
	trace,
	utils::stream::{BroadbandExt, IterStream, ReadyExt},
	warn,
};

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
) -> Vec<(PduEvent, Option<CanonicalJsonObject>)>
where
	Events: Iterator<Item = &'a EventId> + Clone + Send,
{
	let events_with_auth_events: Vec<_> = events
		.stream()
		.broad_then(|event_id| self.fetch_auth_chain(origin, room_id, event_id, room_version))
		.collect()
		.boxed()
		.await;

	events_with_auth_events
		.into_iter()
		.stream()
		.fold(Vec::new(), async |mut pdus, (id, local_pdu, events_in_reverse_order)| {
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
				.ready_filter(|(next_id, _)| {
					let backed_off = self.is_backed_off(next_id, Range {
						start: Duration::from_mins(5),
						end: Duration::from_hours(24),
					});

					!backed_off
				})
				.fold(pdus, async |mut pdus, (next_id, value)| {
					let outlier = Box::pin(self.handle_outlier_pdu(
						origin,
						room_id,
						&next_id,
						value.clone(),
						room_version,
						expected!(recursion_level + 1),
						true,
					));

					if let Ok((pdu, json)) = outlier
						.await
						.inspect_err(|e| warn!("Authentication of event {next_id} failed: {e:?}"))
					{
						if next_id == id {
							pdus.push((pdu, Some(json)));
						}
						self.cancel_back_off(&next_id);
					} else {
						self.back_off(&next_id);
					}

					pdus
				})
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
	_room_id: &RoomId,
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

		if self.is_backed_off(&next_id, Range {
			start: Duration::from_mins(2),
			end: Duration::from_hours(8),
		}) {
			debug_warn!("Backed off from {next_id}");
			continue;
		}

		if self.services.timeline.pdu_exists(&next_id).await {
			trace!(?next_id, "Found in database");
			continue;
		}

		debug!("Fetching {next_id} over federation.");
		let Ok(res) = self
			.services
			.federation
			.execute(origin, get_event::v1::Request { event_id: next_id.clone() })
			.inspect_err(|e| debug_error!(?next_id, "Failed to fetch event: {e}"))
			.inspect_ok(|_| {
				self.cancel_back_off(&next_id);
			})
			.await
		else {
			debug_warn!("Backing off from {next_id}");
			self.back_off(&next_id);
			continue;
		};

		debug!("Got {next_id} over federation");
		let Ok((calculated_event_id, value)) =
			gen_event_id_canonical_json(&res.pdu, room_version)
		else {
			self.back_off(&next_id);
			continue;
		};

		if calculated_event_id != next_id {
			warn!(
				"Server returned wrong event id: requested {next_id}, got \
				 {calculated_event_id}. Event: {:?}",
				&res.pdu
			);

			self.back_off(&next_id);
			continue;
		}

		self.cancel_back_off(&next_id);
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
