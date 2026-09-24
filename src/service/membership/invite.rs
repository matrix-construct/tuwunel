use futures::{FutureExt, future::join};
use ruma::{
	OwnedServerName, RoomId, UserId,
	api::{
		error::ErrorKind,
		federation::membership::{RawStrippedState, create_invite},
	},
	events::{
		invite_permission_config::InvitePermission,
		room::{
			join_rules::JoinRule,
			member::{MembershipState, RoomMemberEventContent},
		},
	},
};
use tuwunel_core::{
	Err, Result, at, err, implement,
	matrix::event::gen_event_id_canonical_json,
	pdu::PduBuilder,
	utils::future::{ReadyBoolExt, and4},
};

use super::Service;

#[implement(Service)]
#[tracing::instrument(
    level = "debug",
    skip_all,
    fields(%sender_user, %room_id, %user_id)
)]
pub async fn invite(
	&self,
	sender_user: &UserId,
	user_id: &UserId,
	room_id: &RoomId,
	reason: Option<&String>,
	is_direct: bool,
) -> Result {
	if self.services.globals.user_is_local(user_id) {
		self.local_invite(sender_user, user_id, room_id, reason, is_direct)
			.boxed()
			.await?;
	} else {
		self.remote_invite(sender_user, user_id, room_id, reason, is_direct)
			.boxed()
			.await?;
	}

	Ok(())
}

/// Reports whether a user must be invited before joining a room.
///
/// True when this server is in the room, the user is neither joined nor
/// invited, and the join rule would refuse the user uninvited: a public rule
/// admits anyone, and a restricted rule admits members of the rooms it
/// allows. When this server is not in the room its view of the join rule may
/// be stale or absent, so the remote join is left to decide.
#[implement(Service)]
pub async fn join_needs_invite(&self, room_id: &RoomId, user_id: &UserId) -> bool {
	let server_in_room = self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), room_id);

	let joined = self
		.services
		.state_cache
		.is_joined(user_id, room_id);

	let invited = self
		.services
		.state_cache
		.is_invited(user_id, room_id);

	let admitted = self
		.services
		.state_accessor
		.get_join_rules(room_id)
		.then(async |rule| self.admits_uninvited(&rule, user_id).await);

	// and4 polls in order, so a point read refusing at once spares the join-rule reads.
	and4(server_in_room, joined.is_false(), invited.is_false(), admitted.is_false()).await
}

#[implement(Service)]
async fn admits_uninvited(&self, rule: &JoinRule, user_id: &UserId) -> bool {
	match rule {
		| JoinRule::Public => true,
		| JoinRule::Restricted(_) | JoinRule::KnockRestricted(_) =>
			self.services
				.state_cache
				.is_joined_any(user_id, rule.allowed_room_ids())
				.await,
		| _ => false,
	}
}

#[implement(Service)]
#[tracing::instrument(name = "remote", level = "debug", skip_all)]
async fn remote_invite(
	&self,
	sender_user: &UserId,
	user_id: &UserId,
	room_id: &RoomId,
	reason: Option<&String>,
	is_direct: bool,
) -> Result {
	let (pdu, pdu_json, invite_room_state, room_version_id) = {
		let state_lock = self.services.state.mutex.lock(room_id).await;

		let content = self
			.services
			.profile
			.fill_content(user_id, RoomMemberEventContent {
				is_direct,
				reason: reason.cloned(),
				..RoomMemberEventContent::new(MembershipState::Invite)
			})
			.await;

		let event = self.services.timeline.create_hash_and_sign_event(
			PduBuilder::state(user_id.to_string(), &content),
			sender_user,
			room_id,
			&state_lock,
		);

		let room_version_id = self.services.state.get_room_version(room_id);
		let (event, room_version_id) = join(event, room_version_id).await;
		let (pdu, pdu_json) = event?;
		let room_version_id = room_version_id?;

		let invite_room_state = self
			.services
			.state
			.summary_pdus(&pdu, &pdu_json, &room_version_id)
			.await;

		drop(state_lock);

		(pdu, pdu_json, invite_room_state, room_version_id)
	};

	let event = self
		.services
		.federation
		.format_pdu_into(pdu_json.clone(), Some(&room_version_id));

	let via = self
		.services
		.state_cache
		.servers_route_via(room_id)
		.map(Result::ok);

	let (event, via) = join(event, via).await;

	let response = self
		.services
		.federation
		.execute(user_id.server_name(), create_invite::v2::Request {
			room_id: room_id.to_owned(),
			event_id: (*pdu.event_id).to_owned(),
			room_version: room_version_id.clone(),
			event,
			invite_room_state: invite_room_state
				.into_iter()
				.map(RawStrippedState::Pdu)
				.collect(),
			via,
		})
		.await
		.map_err(|e| match e.kind() {
			| ErrorKind::IncompatibleRoomVersion { .. } | ErrorKind::UnsupportedRoomVersion =>
				err!(Request(UnsupportedRoomVersion(
					"Server {} does not support room version {room_version_id}.",
					user_id.server_name(),
				))),
			// MSC4311: the remote rejected our well-formed invite over create-event
			// validation; the client cannot make it succeed, so surface a 5xx.
			| ErrorKind::MissingParam => err!(BadServerResponse(
				"Remote server could not validate the invite's create event."
			)),
			| _ => e,
		})?;

	// We do not add the event_id field to the pdu here because of signature and
	// hashes checks
	let (event_id, value) = gen_event_id_canonical_json(&response.event, &room_version_id)
		.map_err(|e| {
			err!(Request(BadJson(warn!("Could not convert event to canonical JSON: {e}"))))
		})?;

	if pdu.event_id != event_id {
		return Err!(Request(BadJson(warn!(
			%pdu.event_id, %event_id,
			"Server {} sent event with wrong event ID",
			user_id.server_name()
		))));
	}

	let origin: OwnedServerName = serde_json::from_value(serde_json::to_value(
		value
			.get("origin")
			.ok_or_else(|| err!(Request(BadJson("Event missing origin field."))))?,
	)?)
	.map_err(|e| {
		err!(Request(BadJson(warn!("Origin field in event is not a valid server name: {e}"))))
	})?;

	let pdu_id = self
		.services
		.event_handler
		.handle_incoming_pdu(&origin, room_id, &event_id, value, true)
		.await?
		.map(at!(0))
		.ok_or_else(|| {
			err!(Request(InvalidParam("Could not accept incoming PDU as timeline event.")))
		})?;

	self.services
		.sending
		.send_pdu_room(room_id, &pdu_id)
		.await?;

	Ok(())
}

#[implement(Service)]
#[tracing::instrument(name = "local", level = "debug", skip_all)]
async fn local_invite(
	&self,
	sender_user: &UserId,
	user_id: &UserId,
	room_id: &RoomId,
	reason: Option<&String>,
	is_direct: bool,
) -> Result {
	let blocked = self
		.services
		.users
		.invite_permission(sender_user, user_id)
		.map(|permission| permission.eq(&InvitePermission::Block));

	let joined = self
		.services
		.state_cache
		.is_joined(sender_user, room_id);

	let (blocked, joined) = join(blocked, joined).await;

	if blocked {
		return Err!(Request(InviteBlocked("{user_id} has blocked this invite.")));
	}

	if !joined {
		return Err!(Request(Forbidden(
			"You must be joined in the room you are trying to invite from."
		)));
	}

	let state_lock = self.services.state.mutex.lock(room_id).await;

	let content = self
		.services
		.profile
		.fill_content(user_id, RoomMemberEventContent {
			is_direct,
			reason: reason.cloned(),
			..RoomMemberEventContent::new(MembershipState::Invite)
		})
		.await;

	self.services
		.timeline
		.build_and_append_pdu(
			PduBuilder::state(user_id.to_string(), &content),
			sender_user,
			room_id,
			&state_lock,
		)
		.await?;

	drop(state_lock);

	Ok(())
}
