use std::{
	collections::{HashMap, HashSet},
	iter::once,
};

use futures::{FutureExt, StreamExt, stream::FuturesOrdered};
use ruma::{
	CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, RoomId,
	RoomVersionId, ServerName, int, uint,
};
use tuwunel_core::{
	Result, debug_warn, err, implement,
	matrix::{Event, PduEvent},
	state_res::{self},
};

use super::check_room_id;

#[implement(super::Service)]
#[tracing::instrument(
	level = "debug",
	skip_all,
	fields(%origin),
)]
#[expect(clippy::type_complexity)]
pub(super) async fn fetch_prev<'a, Events>(
	&self,
	origin: &ServerName,
	room_id: &RoomId,
	initial_set: Events,
	room_version: &RoomVersionId,
	first_ts_in_room: MilliSecondsSinceUnixEpoch,
) -> Result<(Vec<OwnedEventId>, HashMap<OwnedEventId, (PduEvent, CanonicalJsonObject)>)>
where
	Events: Iterator<Item = &'a EventId> + Send,
{
	let mut todo_outlier_stack: FuturesOrdered<_> = initial_set
		.map(ToOwned::to_owned)
		.map(async |event_id| {
			let fetch = self.fetch_auth(origin, room_id, once(event_id.as_ref()), room_version);

			(event_id.clone(), fetch.await)
		})
		.map(FutureExt::boxed)
		.collect();

	let mut amount = 0;
	let mut eventid_info = HashMap::new();
	let mut graph: HashMap<OwnedEventId, _> = HashMap::with_capacity(todo_outlier_stack.len());
	while let Some((prev_event_id, mut outlier)) = todo_outlier_stack.next().await {
		let Some((pdu, mut json_opt)) = outlier.pop() else {
			// Fetch and handle failed
			graph.insert(prev_event_id.clone(), HashSet::new());
			continue;
		};

		check_room_id(room_id, &pdu)?;

		let limit = self.services.server.config.max_fetch_prev_events;
		if amount > limit {
			debug_warn!(?limit, "Max prev event limit reached!");
			graph.insert(prev_event_id.clone(), HashSet::new());
			continue;
		}

		if json_opt.is_none() {
			json_opt = self
				.services
				.timeline
				.get_outlier_pdu_json(&prev_event_id)
				.await
				.ok();
		}

		let Some(json) = json_opt else {
			// Get json failed, so this was not fetched over federation
			graph.insert(prev_event_id.clone(), HashSet::new());
			continue;
		};

		if pdu.origin_server_ts() > first_ts_in_room {
			amount = amount.saturating_add(1);
			for prev_prev in pdu.prev_events() {
				if !graph.contains_key(prev_prev) {
					let prev_prev = prev_prev.to_owned();
					let fetch = async move {
						let fetch = self.fetch_auth(
							origin,
							room_id,
							once(prev_prev.as_ref()),
							room_version,
						);

						(prev_prev.clone(), fetch.await)
					};

					todo_outlier_stack.push_back(fetch.boxed());
				}
			}

			graph.insert(
				prev_event_id.clone(),
				pdu.prev_events().map(ToOwned::to_owned).collect(),
			);
		} else {
			// Time based check failed
			graph.insert(prev_event_id.clone(), HashSet::new());
		}

		eventid_info.insert(prev_event_id.clone(), (pdu, json));
		self.services.server.check_running()?;
	}

	let event_fetch = async |event_id: OwnedEventId| {
		let origin_server_ts = eventid_info
			.get(&event_id)
			.map_or_else(|| uint!(0), |info| info.0.origin_server_ts().get());

		// This return value is the key used for sorting events,
		// events are then sorted by power level, time,
		// and lexically by event_id.
		Ok((int!(0).into(), MilliSecondsSinceUnixEpoch(origin_server_ts)))
	};

	let sorted = state_res::topological_sort(&graph, &event_fetch)
		.await
		.map_err(|e| err!(Database(error!("Error sorting prev events: {e}"))))?;

	Ok((sorted, eventid_info))
}
