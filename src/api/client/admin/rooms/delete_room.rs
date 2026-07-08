use axum::extract::State;
use ruma::{RoomId, UserId};
use synapse_admin_api::rooms::delete_room::{
	v1::{Request as V1Request, Response as V1Response, ShutdownRoom},
	v2::{Request as V2Request, Response as V2Response},
};
use tuwunel_core::Result;
use tuwunel_service::rooms::delete::ShutdownRoom as Summary;

use crate::{Ruma, client::admin::require_admin};

/// # `DELETE /_synapse/admin/v1/rooms/{room_id}`
///
/// Synchronously evicts the room's local users, optionally purges its storage
/// (`purge`, default true) and blocks it (`block`), returning the shutdown
/// outcome. The replacement-room fields are accepted and ignored, so no
/// redirect room is created (`new_room_id` is always null).
pub(crate) async fn admin_delete_room_v1_route(
	State(services): State<crate::State>,
	body: Ruma<V1Request>,
) -> Result<V1Response> {
	require_admin(&services, body.sender_user()).await?;

	let summary =
		run_shutdown(&services, &body.room_id, body.sender_user(), body.block, body.purge).await;

	Ok(V1Response { result: into_response(summary) })
}

/// # `DELETE /_synapse/admin/v2/rooms/{room_id}`
///
/// Schedules the same shutdown as a background task and returns its id at once;
/// the outcome is retrieved from the delete-status endpoints.
pub(crate) async fn admin_delete_room_v2_route(
	State(services): State<crate::State>,
	body: Ruma<V2Request>,
) -> Result<V2Response> {
	require_admin(&services, body.sender_user()).await?;

	let room_id = body.room_id.clone();
	let sender = body.sender_user().to_owned();
	let (block, purge) = (body.block, body.purge);

	let work = async move {
		let summary = run_shutdown(&services, &room_id, &sender, block, purge).await;

		Ok(serde_json::to_value(summary)?)
	};

	let delete_id = services
		.tasks
		.spawn(super::DELETE_ROOM_ACTION, body.room_id.to_string(), work)
		.to_string();

	Ok(V2Response { delete_id })
}

/// Runs the shutdown, purging storage when asked and recording the room as
/// blocked by the requesting admin. `force_purge` is not mapped: tuwunel always
/// evicts local users before purging.
async fn run_shutdown(
	services: &crate::State,
	room_id: &RoomId,
	sender: &UserId,
	block: bool,
	purge: bool,
) -> Summary {
	let state_lock = services.state.mutex.lock(room_id).await;

	let summary = if purge {
		services
			.delete
			.delete_room(room_id, false, state_lock)
			.await
			.unwrap_or_default()
	} else {
		services
			.delete
			.shutdown_room(room_id, &state_lock)
			.await
	};

	if block {
		services.metadata.block_room(room_id, sender);
	}

	summary
}

fn into_response(summary: Summary) -> ShutdownRoom {
	ShutdownRoom {
		kicked_users: summary.kicked_users,
		failed_to_kick_users: summary.failed_to_kick_users,
		local_aliases: summary
			.local_aliases
			.iter()
			.map(ToString::to_string)
			.collect(),
		new_room_id: summary.new_room_id,
	}
}

#[cfg(test)]
mod tests {
	use ruma::{room_alias_id, user_id};
	use tuwunel_service::rooms::delete::ShutdownRoom as Summary;

	use super::{ShutdownRoom, into_response};

	/// The synchronous v1 delete maps the summary's fields directly, while the
	/// async v2 delete stores the summary as JSON and the status endpoint
	/// deserializes it back. Both must yield the same wire shape.
	#[test]
	fn v1_field_map_agrees_with_v2_json_round_trip() {
		let summary = Summary {
			kicked_users: vec![user_id!("@alice:example.org").to_owned()],
			failed_to_kick_users: vec![user_id!("@bob:example.org").to_owned()],
			local_aliases: vec![room_alias_id!("#lounge:example.org").to_owned()],
			new_room_id: None,
		};

		let via_field_map = into_response(summary.clone());

		let via_json: ShutdownRoom =
			serde_json::from_value(serde_json::to_value(&summary).unwrap()).unwrap();

		assert_eq!(
			serde_json::to_value(&via_field_map).unwrap(),
			serde_json::to_value(&via_json).unwrap(),
		);
	}
}
