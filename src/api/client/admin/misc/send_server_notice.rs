use axum::extract::State;
use futures::{
	FutureExt, StreamExt,
	future::{join, join3},
};
use ruma::{
	DeviceId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId, TransactionId, UserId,
	events::{
		StateEventType,
		room::{
			create::RoomCreateEventContent,
			guest_access::{GuestAccess, RoomGuestAccessEventContent},
			history_visibility::{HistoryVisibility, RoomHistoryVisibilityEventContent},
			join_rules::{JoinRule, RoomJoinRulesEventContent},
			member::{MembershipState, RoomMemberEventContent},
			message::RoomMessageEventContent,
			name::RoomNameEventContent,
			power_levels::RoomPowerLevelsEventContent,
		},
		tag::{TagName, Tags},
	},
	serde::Raw,
};
use synapse_admin_api::server_notices::send::{
	by_txn,
	v1::{self, Response},
};
use tuwunel_core::{
	Err, Result,
	matrix::{Event, pdu::PduBuilder, room_version::rules as get_room_version_rules},
	utils::{stream::ReadyExt, string_from_bytes},
};
use tuwunel_service::Services;

use crate::{Ruma, client::admin::require_admin};

/// # `POST /_synapse/admin/v1/send_server_notice`
///
/// Sends a server notice into the target user's system room, creating the room
/// on demand, and returns the sent event's ID.
/// Server notices are always enabled because tuwunel has no separate
/// enablement setting.
pub(crate) async fn admin_send_server_notice_route(
	State(services): State<crate::State>,
	body: Ruma<v1::Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let request = body.body;

	send_notice(
		&services,
		&request.user_id,
		request.event_type,
		request.state_key,
		request.content,
	)
	.await
	.map(Response::new)
}

/// # `PUT /_synapse/admin/v1/send_server_notice/{txn_id}`
///
/// Sends a server notice once for each transaction ID and returns the recorded
/// event ID on replay.
pub(crate) async fn admin_send_server_notice_txn_route(
	State(services): State<crate::State>,
	body: Ruma<by_txn::Request>,
) -> Result<Response> {
	require_admin(&services, body.sender_user()).await?;

	let sender_user = body
		.sender_user
		.expect("user must be authenticated for this handler");

	let sender_device = body.sender_device;
	let request = body.body;

	if let Some(response) =
		check_existing_txnid(&services, &sender_user, sender_device.as_deref(), &request.txn_id)
			.await
	{
		return response;
	}

	let event_id = send_notice(
		&services,
		&request.user_id,
		request.event_type,
		request.state_key,
		request.content,
	)
	.await?;

	services.transaction_ids.add_txnid(
		&sender_user,
		sender_device.as_deref(),
		&request.txn_id,
		event_id.as_bytes(),
	);

	Ok(Response::new(event_id))
}

async fn send_notice(
	services: &Services,
	target: &UserId,
	event_type: Option<String>,
	state_key: Option<String>,
	content: Raw<RoomMessageEventContent>,
) -> Result<OwnedEventId> {
	if !services.globals.user_is_local(target) {
		return Err!(Request(InvalidParam("Server notices can only be sent to local users")));
	}

	if !services.users.exists(target).await {
		return Err!(Request(NotFound("User not found")));
	}

	let room_id = match find_notice_room(services, target).boxed().await {
		| Some(room_id) => room_id,
		| None =>
			create_notice_room(services, target)
				.boxed()
				.await?,
	};

	let server_user = services.globals.server_user.as_ref();
	let state_lock = services.state.mutex.lock(&room_id).await;

	let is_joined = services.state_cache.is_joined(target, &room_id);
	let is_invited = services.state_cache.is_invited(target, &room_id);
	let (is_joined, is_invited) = join(is_joined, is_invited).await;

	if !is_joined && !is_invited {
		let pdu = PduBuilder::state(
			String::from(target),
			&RoomMemberEventContent::new(MembershipState::Invite),
		);

		services
			.timeline
			.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
			.boxed()
			.await?;
	}

	let content = Raw::from_raw_value(content.json());

	let pdu = PduBuilder {
		event_type: event_type
			.as_deref()
			.unwrap_or("m.room.message")
			.into(),
		content,
		state_key: state_key.map(Into::into),
		..Default::default()
	};

	let event_id = services
		.timeline
		.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
		.await?;

	drop(state_lock);

	Ok(event_id)
}

async fn find_notice_room(services: &Services, target: &UserId) -> Option<OwnedRoomId> {
	let server_user = services.globals.server_user.as_ref();
	let admin_room = services.admin.get_admin_room().await.ok();
	let tag = notice_tag(&services.config.admin_room_tag);

	let joined: Vec<OwnedRoomId> = services
		.state_cache
		.get_shared_rooms(server_user, target)
		.ready_filter(|room_id| admin_room.as_deref() != Some(*room_id))
		.map(ToOwned::to_owned)
		.collect()
		.await;

	if let Some(room_id) =
		find_notice_candidate(services, server_user, target, &tag, joined).await
	{
		return Some(room_id);
	}

	let invited: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_invited(target)
		.ready_filter(|room_id| admin_room.as_deref() != Some(*room_id))
		.map(ToOwned::to_owned)
		.collect()
		.await;

	if let Some(room_id) =
		find_notice_candidate(services, server_user, target, &tag, invited).await
	{
		return Some(room_id);
	}

	let left: Vec<OwnedRoomId> = services
		.state_cache
		.rooms_left(target)
		.ready_filter(|room_id| admin_room.as_deref() != Some(*room_id))
		.map(ToOwned::to_owned)
		.collect()
		.await;

	find_notice_candidate(services, server_user, target, &tag, left).await
}

async fn find_notice_candidate(
	services: &Services,
	server_user: &UserId,
	target: &UserId,
	tag: &TagName,
	candidates: Vec<OwnedRoomId>,
) -> Option<OwnedRoomId> {
	for room_id in candidates {
		if room_is_notice(services, server_user, target, tag, &room_id).await {
			return Some(room_id);
		}
	}

	None
}

async fn room_is_notice(
	services: &Services,
	server_user: &UserId,
	target: &UserId,
	tag: &TagName,
	room_id: &RoomId,
) -> bool {
	let server_joined = services
		.state_cache
		.is_joined(server_user, room_id);

	let tags = services
		.account_data
		.get_room_tags(target, room_id);

	let create = services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomCreate, "");

	let (server_joined, tags, create) = join3(server_joined, tags, create).await;

	server_joined
		&& create.is_ok_and(|create| {
			is_notice_marker(create.sender(), server_user, &tags.unwrap_or_default(), tag)
		})
}

async fn create_notice_room(services: &Services, target: &UserId) -> Result<OwnedRoomId> {
	let room_id = RoomId::new_v1(services.globals.server_name());
	let room_version_id = RoomVersionId::V11;

	let room_version_rules = get_room_version_rules(&room_version_id)?;

	let _short_id = services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let state_lock = services.state.mutex.lock(&room_id).await;
	let server_user: &UserId = services.globals.server_user.as_ref();

	let create_content = if !room_version_rules
		.authorization
		.use_room_create_sender
	{
		RoomCreateEventContent::new_v1(server_user.into())
	} else {
		RoomCreateEventContent::new_v11()
	};

	let content = RoomCreateEventContent {
		room_version: room_version_id,
		..create_content
	};

	let pdu = PduBuilder::state(String::new(), &content);

	services
		.timeline
		.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
		.boxed()
		.await?;

	let pdu = PduBuilder::state(
		String::from(server_user),
		&RoomMemberEventContent::new(MembershipState::Join),
	);

	services
		.timeline
		.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
		.boxed()
		.await?;

	let pdu = PduBuilder::state(String::new(), &notice_power_levels(server_user));

	services
		.timeline
		.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
		.boxed()
		.await?;

	let pdu = PduBuilder::state(String::new(), &RoomJoinRulesEventContent::new(JoinRule::Invite));

	services
		.timeline
		.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
		.boxed()
		.await?;

	let pdu = PduBuilder::state(
		String::new(),
		&RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared),
	);

	services
		.timeline
		.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
		.boxed()
		.await?;

	let pdu =
		PduBuilder::state(String::new(), &RoomGuestAccessEventContent::new(GuestAccess::CanJoin));

	services
		.timeline
		.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
		.boxed()
		.await?;

	let pdu =
		PduBuilder::state(String::new(), &RoomNameEventContent::new("Server Notices".to_owned()));

	services
		.timeline
		.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
		.boxed()
		.await?;

	drop(state_lock);

	services
		.account_data
		.set_room_tag(target, &room_id, notice_tag(&services.config.admin_room_tag), None)
		.await?;

	Ok(room_id)
}

fn notice_tag(tag: &str) -> TagName {
	Some(tag)
		.filter(|tag| !tag.is_empty())
		.map_or(TagName::ServerNotice, Into::into)
}

fn is_notice_marker(
	create_sender: &UserId,
	server_user: &UserId,
	tags: &Tags,
	tag: &TagName,
) -> bool {
	create_sender == server_user && tags.contains_key(tag)
}

fn notice_power_levels(server_user: &UserId) -> RoomPowerLevelsEventContent {
	RoomPowerLevelsEventContent {
		users: [(server_user.into(), 100.into())].into(),
		users_default: (-10).into(),
		..Default::default()
	}
}

async fn check_existing_txnid(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	txn_id: &TransactionId,
) -> Option<Result<Response>> {
	let response = services
		.transaction_ids
		.existing_txnid(sender_user, sender_device, txn_id)
		.await
		.ok()?;

	if response.is_empty() {
		return Some(Err!(Request(InvalidParam(
			"Tried to use txn_id already used for an incompatible endpoint."
		))));
	}

	let Ok(Ok(event_id)) = string_from_bytes(&response).map(TryInto::try_into) else {
		return Some(Err!(Database("Invalid event_id in txn_id data: {response:?}.")));
	};

	Some(Ok(Response::new(event_id)))
}

#[cfg(test)]
mod tests {
	use ruma::{
		Int,
		events::tag::{TagInfo, TagName, Tags},
		user_id,
	};

	use super::{is_notice_marker, notice_power_levels, notice_tag};

	#[test]
	fn power_levels_mute_the_target() {
		let server = user_id!("@server:example.com");
		let power_levels = notice_power_levels(server);

		assert_eq!(power_levels.users.get(server), Some(&Int::from(100)));
		assert_eq!(power_levels.users_default, Int::from(-10));
		assert_eq!(power_levels.events_default, Int::from(0));
		assert!(power_levels.users_default < power_levels.events_default);
	}

	#[test]
	fn marker_requires_create_sender_and_tag() {
		let server = user_id!("@server:example.com");
		let other = user_id!("@other:example.com");
		let tagged = Tags::from([(TagName::ServerNotice, TagInfo::new())]);

		assert!(is_notice_marker(server, server, &tagged, &TagName::ServerNotice));
		assert!(!is_notice_marker(other, server, &tagged, &TagName::ServerNotice));
		assert!(!is_notice_marker(server, server, &Tags::new(), &TagName::ServerNotice));
	}

	#[test]
	fn empty_config_tag_falls_back_to_server_notice() {
		assert_eq!(notice_tag(""), TagName::ServerNotice);
		assert_eq!(notice_tag("m.server_notice"), TagName::ServerNotice);
		assert_eq!(notice_tag("u.custom"), TagName::from("u.custom"));
	}
}
