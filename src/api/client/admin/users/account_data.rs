use std::collections::BTreeMap;

use axum::extract::State;
use futures::StreamExt;
use ruma::{
	OwnedRoomId, UserId,
	events::{
		AnyGlobalAccountDataEventContent, AnyRawAccountDataEvent, AnyRoomAccountDataEventContent,
	},
	serde::Raw,
};
use synapse_admin_api::users::account_data::v1 as account_data;
use tuwunel_core::{
	Err, Result, extract_variant,
	utils::{ReadyExt, stream::BroadbandExt},
};

use crate::{Ruma, client::admin::require_admin};

type GlobalData = BTreeMap<String, Raw<AnyGlobalAccountDataEventContent>>;
type RoomData = BTreeMap<String, Raw<AnyRoomAccountDataEventContent>>;

/// # `GET /_synapse/admin/v1/users/{user_id}/accountdata`
pub(crate) async fn admin_account_data_route(
	State(services): State<crate::State>,
	body: Ruma<account_data::Request>,
) -> Result<account_data::Response> {
	require_admin(&services, body.sender_user()).await?;

	if !services.globals.user_is_local(&body.user_id) {
		return Err!(Request(InvalidParam("Can only look up local users")));
	}

	if !services.users.exists(&body.user_id).await {
		return Err!(Request(NotFound("User not found")));
	}

	let global: GlobalData = services
		.account_data
		.changes_since(None, &body.user_id, 0, None)
		.ready_filter_map(|event| extract_variant!(event, AnyRawAccountDataEvent::Global))
		.ready_filter_map(|event| split_type_content(&event))
		.collect()
		.await;

	// Per-room account data has no single-scan primitive; scan each room the user
	// has a membership row in (rooms with no membership row are missed).
	let rooms = services
		.state_cache
		.user_memberships(&body.user_id, None)
		.map(|(_, room_id)| room_id.to_owned())
		.broad_filter_map(async |room_id| {
			room_account_data(services, &body.user_id, room_id).await
		})
		.collect()
		.await;

	let data = account_data::AccountData { global, rooms };

	Ok(account_data::Response::new(data))
}

async fn room_account_data(
	services: crate::State,
	user_id: &UserId,
	room_id: OwnedRoomId,
) -> Option<(OwnedRoomId, RoomData)> {
	let data: RoomData = services
		.account_data
		.changes_since(Some(&room_id), user_id, 0, None)
		.ready_filter_map(|event| extract_variant!(event, AnyRawAccountDataEvent::Room))
		.ready_filter_map(|event| split_type_content(&event))
		.collect()
		.await;

	(!data.is_empty()).then_some((room_id, data))
}

/// Splits a raw account-data event into its `type` key and the `content`
/// re-cast to the content type, dropping any event missing either field.
fn split_type_content<E, C>(event: &Raw<E>) -> Option<(String, Raw<C>)> {
	let event_type = event.get_field::<String>("type").ok().flatten()?;
	let content = event
		.get_field::<Raw<C>>("content")
		.ok()
		.flatten()?;

	Some((event_type, content))
}
