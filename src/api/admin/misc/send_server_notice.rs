use axum::extract::State;
use futures::{FutureExt, StreamExt, TryFutureExt, TryStreamExt, future::ready};
use ruma::{
	DeviceId, OwnedEventId, OwnedRoomId, RoomId, RoomVersionId, TransactionId, UserId,
	events::{
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
		tag::TagName,
	},
	serde::Raw,
};
use synapse_admin_api::server_notices::send::{
	by_txn,
	v1::{self, Response},
};
use tuwunel_api_client::room_is_notice;
use tuwunel_core::{
	Err, Result, err,
	utils::{
		BoolExt, FutureBoolExt,
		future::{ReadyBoolExt, TryExtExt},
		str_from_bytes,
		stream::{IterStream, ReadyExt},
	},
	warn,
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::Services;

use crate::RumaAdmin;

/// Sends a notice through `POST /_synapse/admin/v1/send_server_notice`.
///
/// Sends a server notice into the target user's system room, creating the room
/// on demand, and returns the sent event's ID.
/// Server notices are always enabled because tuwunel has no separate
/// enablement setting.
pub(crate) async fn admin_send_server_notice_route(
	State(services): State<crate::State>,
	body: RumaAdmin<v1::Request>,
) -> Result<Response> {
	let request = body.body;

	send_notice(
		&services,
		&request.user_id,
		request.event_type.as_deref(),
		request.state_key.as_deref(),
		request.content,
	)
	.map_ok(Response::new)
	.await
}

/// Sends a notice through `PUT /_synapse/admin/v1/send_server_notice/{txn_id}`.
///
/// Sends a server notice once for each transaction ID and returns the recorded
/// event ID on replay.
pub(crate) async fn admin_send_server_notice_txn_route(
	State(services): State<crate::State>,
	body: RumaAdmin<by_txn::Request>,
) -> Result<Response> {
	let sender_user = body
		.sender_user
		.expect("user must be authenticated for this handler");

	let sender_device = body.sender_device;
	let request = body.body;

	check_existing_txnid(&services, &sender_user, sender_device.as_deref(), &request.txn_id)
		.await
		.map(|response| ready(response).right_future())
		.unwrap_or_else(|| {
			send_notice_txn(&services, &sender_user, sender_device.as_deref(), request)
				.left_future()
		})
		.await
}

/// Sends a new transaction and records its event ID for subsequent retries.
///
/// The caller checks administrator authorization and existing transactions first.
async fn send_notice_txn(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	request: by_txn::Request,
) -> Result<Response> {
	let event_id = send_notice(
		services,
		&request.user_id,
		request.event_type.as_deref(),
		request.state_key.as_deref(),
		request.content,
	)
	.await?;

	services.transaction_ids.add_txnid(
		sender_user,
		sender_device,
		&request.txn_id,
		event_id.as_bytes(),
	);

	Ok(Response::new(event_id))
}

/// Sends an event from the server identity into the target's notice room.
///
/// Reuses prior rooms and invites recipients who are neither joined nor invited.
/// Membership and notice events are appended under the same room lock.
async fn send_notice(
	services: &Services,
	target: &UserId,
	event_type: Option<&str>,
	state_key: Option<&str>,
	content: Raw<RoomMessageEventContent>,
) -> Result<OwnedEventId> {
	if !services.globals.user_is_local(target) {
		return Err!(Request(InvalidParam("Server notices can only be sent to local users")));
	}

	if !services.users.exists(target).await {
		return Err!(Request(NotFound("User not found")));
	}

	let room_id = match find_notice_room(services, target).await {
		| Some(room_id) => room_id,
		| None =>
			create_notice_room(services, target)
				.boxed() // cold arm
				.await?,
	};

	let server_user = services.globals.server_user.as_ref();
	let state_lock = services.state.mutex.lock(&room_id).await;

	let is_joined = services.state_cache.is_joined(target, &room_id);
	let is_invited = services.state_cache.is_invited(target, &room_id);
	let needs_invite = is_joined.is_false().and(is_invited.is_false());

	if needs_invite.await {
		let pdu = PduBuilder::state(
			target.as_str(),
			&RoomMemberEventContent::new(MembershipState::Invite),
		);

		services
			.timeline
			.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
			.boxed() // Cold invitation layout cut.
			.await?;
	}

	let content = Raw::from_raw_value(content.json());

	let pdu = PduBuilder {
		event_type: event_type.unwrap_or("m.room.message").into(),
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

/// Finds the first marked room in joined, invited, then left membership order.
///
/// Owns each cursor item before awaiting marker reads and stops at the first
/// match without collecting candidate rooms.
#[tracing::instrument(level = "trace", skip_all)]
async fn find_notice_room(services: &Services, target: &UserId) -> Option<OwnedRoomId> {
	let server_user = services.globals.server_user.as_ref();
	let admin_room = services
		.admin
		.get_admin_room()
		.map(Result::ok)
		.await;

	services
		.state_cache
		.get_shared_rooms(server_user, target)
		.map(ToOwned::to_owned)
		.chain(
			services
				.state_cache
				.rooms_invited(target)
				.map(ToOwned::to_owned),
		)
		.chain(
			services
				.state_cache
				.rooms_left(target)
				.map(ToOwned::to_owned),
		)
		.ready_filter(|room_id| admin_room.as_ref() != Some(room_id))
		.filter_map(async |room_id| {
			room_is_notice(services, target, &room_id)
				.inspect_err(
					|error| warn!(%room_id, %error, "Unreadable notice marker, skipping room"),
				)
				.unwrap_or(false)
				.await
				.then_some(room_id)
		})
		.take(1)
		.ready_fold(None, |_, room_id| Some(room_id))
		.await
}

/// Creates a private room whose recipient can read notices but cannot post.
///
/// Initial state is appended in authorization order before the target's room tag
/// is written. Invitation is left to the caller after the room lock is released.
#[tracing::instrument(level = "debug", skip_all)]
async fn create_notice_room(services: &Services, target: &UserId) -> Result<OwnedRoomId> {
	let room_id = RoomId::new_v1(services.globals.server_name());

	let _short_id = services
		.short
		.get_or_create_shortroomid(&room_id)
		.await;

	let state_lock = services.state.mutex.lock(&room_id).await;
	let server_user: &UserId = services.globals.server_user.as_ref();

	let content = RoomCreateEventContent {
		room_version: RoomVersionId::V11,
		..RoomCreateEventContent::new_v11()
	};

	[
		PduBuilder::state(String::new(), &content),
		PduBuilder::state(
			server_user.as_str(),
			&RoomMemberEventContent::new(MembershipState::Join),
		),
		PduBuilder::state(String::new(), &notice_power_levels(server_user)),
		PduBuilder::state(String::new(), &RoomJoinRulesEventContent::new(JoinRule::Invite)),
		PduBuilder::state(
			String::new(),
			&RoomHistoryVisibilityEventContent::new(HistoryVisibility::Shared),
		),
		PduBuilder::state(String::new(), &RoomGuestAccessEventContent::new(GuestAccess::CanJoin)),
		PduBuilder::state(String::new(), &RoomNameEventContent::new("Server Notices".to_owned())),
	]
	.into_iter()
	.try_stream()
	.try_for_each(|pdu| {
		services
			.timeline
			.build_and_append_pdu(pdu, server_user, &room_id, &state_lock)
			.map_ok(|_| ())
	})
	.await?;

	drop(state_lock);

	services
		.account_data
		.set_room_tag(target, &room_id, TagName::ServerNotice, None)
		.await?;

	Ok(room_id)
}

/// Reserves posting and room administration for the server identity.
///
/// Recipients retain the ability to join and subsequently leave the room.
fn notice_power_levels(server_user: &UserId) -> RoomPowerLevelsEventContent {
	RoomPowerLevelsEventContent {
		users: [(server_user.into(), 100.into())].into(),
		users_default: (-10).into(),
		..Default::default()
	}
}

/// Replays a stored notice response for the requesting user and device.
///
/// Empty transaction data belongs to an incompatible endpoint; invalid event IDs
/// indicate corrupt stored data rather than a fresh transaction.
async fn check_existing_txnid(
	services: &Services,
	sender_user: &UserId,
	sender_device: Option<&DeviceId>,
	txn_id: &TransactionId,
) -> Option<Result<Response>> {
	services
		.transaction_ids
		.existing_txnid(sender_user, sender_device, txn_id)
		.map_ok(|response| notice_response(&response))
		.map(Result::ok)
		.await
}

/// Decodes a cached event ID while distinguishing incompatible transaction data.
///
/// Empty values identify to-device transactions; malformed IDs are database errors.
fn notice_response(response: &[u8]) -> Result<Response> {
	response
		.is_empty()
		.is_false()
		.ok_or_else(|| {
			err!(Request(InvalidParam(
				"Tried to use txn_id already used for an incompatible endpoint."
			)))
		})
		.and_then(|()| {
			str_from_bytes(response)
				.ok()
				.and_then(|event_id| event_id.try_into().ok())
				.map(Response::new)
				.ok_or_else(|| err!(Database("Invalid event_id in txn_id data: {response:?}.")))
		})
}

#[cfg(test)]
mod tests {
	use ruma::{Int, user_id};

	use super::notice_power_levels;

	#[test]
	fn power_levels_mute_the_target() {
		let server = user_id!("@server:example.com");
		let power_levels = notice_power_levels(server);

		assert_eq!(power_levels.users.get(server), Some(&Int::from(100)));
		assert_eq!(power_levels.users_default, Int::from(-10));
		assert_eq!(power_levels.events_default, Int::from(0));
		assert!(power_levels.users_default < power_levels.events_default);
	}
}
