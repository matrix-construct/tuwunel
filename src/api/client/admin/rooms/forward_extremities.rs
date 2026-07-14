use axum::extract::State;
use futures::{
	StreamExt,
	future::{join, join_all},
};
use ruma::{EventId, OwnedEventId, UInt};
use synapse_admin_api::rooms::forward_extremities::{
	delete::{Request as DeleteRequest, Response as DeleteResponse},
	get::{ForwardExtremity, Request as GetRequest, Response as GetResponse},
};
use tuwunel_core::{Result, smallvec::SmallVec};

use crate::{Ruma, client::admin::require_admin};

type Extremities = SmallVec<[OwnedEventId; 1]>;

/// # `GET /_synapse/admin/v1/rooms/{room_id_or_alias}/forward_extremities`
///
/// Lists the room's forward extremities. `received_ts` stands in as the event's
/// `origin_server_ts` (arrival time is not stored) and `state_group` is the
/// event's state hash or null.
pub(crate) async fn admin_get_forward_extremities_route(
	State(services): State<crate::State>,
	body: Ruma<GetRequest>,
) -> Result<GetResponse> {
	require_admin(&services, body.sender_user()).await?;

	let (room_id, _) = services
		.alias
		.maybe_resolve_with_servers(&body.room_id_or_alias, None)
		.await?;

	let extremities = services
		.state
		.get_forward_extremities(&room_id)
		.map(ToOwned::to_owned)
		.collect::<Extremities>()
		.await;

	let results = join_all(
		extremities
			.iter()
			.map(|event_id| forward_extremity(&services, event_id)),
	)
	.await;

	Ok(GetResponse {
		count: super::usize_to_uint(results.len()),
		results,
	})
}

/// # `DELETE /_synapse/admin/v1/rooms/{room_id_or_alias}/forward_extremities`
///
/// Collapses the room to a single forward extremity, keeping the one furthest
/// along in stream order, and reports how many were removed.
pub(crate) async fn admin_delete_forward_extremities_route(
	State(services): State<crate::State>,
	body: Ruma<DeleteRequest>,
) -> Result<DeleteResponse> {
	require_admin(&services, body.sender_user()).await?;

	let (room_id, _) = services
		.alias
		.maybe_resolve_with_servers(&body.room_id_or_alias, None)
		.await?;

	let state_lock = services.state.mutex.lock(&room_id).await;

	let deleted = services
		.state
		.collapse_forward_extremities(&room_id, &state_lock)
		.await;

	Ok(DeleteResponse { deleted: super::usize_to_uint(deleted) })
}

async fn forward_extremity(services: &crate::State, event_id: &EventId) -> ForwardExtremity {
	let (pdu, shortstatehash) =
		join(services.timeline.get_pdu(event_id), services.state.pdu_shortstatehash(event_id))
			.await;

	let (depth, received_ts) = pdu
		.map(|pdu| (pdu.depth, pdu.origin_server_ts))
		.unwrap_or_default();

	forward_extremity_row(event_id, depth, received_ts, shortstatehash.ok())
}

/// Assembles a forward-extremity row. `state_group` carries the event's state
/// hash when it fits `UInt`, else null; `depth` and `received_ts` pass through.
fn forward_extremity_row(
	event_id: &EventId,
	depth: UInt,
	received_ts: UInt,
	shortstatehash: Option<u64>,
) -> ForwardExtremity {
	ForwardExtremity {
		event_id: event_id.to_owned(),
		state_group: shortstatehash.and_then(|hash| UInt::try_from(hash).ok()),
		depth,
		received_ts,
	}
}

#[cfg(test)]
mod tests {
	use ruma::{event_id, uint};
	use serde_json::json;

	use super::forward_extremity_row;

	#[test]
	fn state_group_is_the_state_hash_when_it_fits() {
		let row =
			forward_extremity_row(event_id!("$abc:example.org"), uint!(7), uint!(1000), Some(42));

		let value = serde_json::to_value(row).unwrap();

		assert_eq!(value["state_group"], json!(42));
		assert_eq!(value["depth"], json!(7));
		assert_eq!(value["received_ts"], json!(1000));
	}

	#[test]
	fn state_group_is_null_when_the_hash_overflows_uint() {
		let row = forward_extremity_row(
			event_id!("$abc:example.org"),
			uint!(0),
			uint!(0),
			Some(u64::MAX),
		);

		let value = serde_json::to_value(row).unwrap();

		assert_eq!(value["state_group"], json!(null));
	}
}
