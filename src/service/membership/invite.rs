use futures::FutureExt;
use ruma::{
	OwnedServerName, RoomId, UserId,
	api::{
		error::ErrorKind,
		federation::membership::{RawStrippedState, create_invite},
	},
	events::room::member::{MembershipState, RoomMemberEventContent},
};
use tuwunel_core::{
	Err, Result, at, err, implement, matrix::event::gen_event_id_canonical_json, pdu::PduBuilder,
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

		let mut content = RoomMemberEventContent {
			is_direct,
			reason: reason.cloned(),
			..RoomMemberEventContent::new(MembershipState::Invite)
		};

		self.services
			.profile
			.fill_profile_data(user_id, &mut content)
			.await;

		let (pdu, pdu_json) = self
			.services
			.timeline
			.create_hash_and_sign_event(
				PduBuilder::state(user_id.to_string(), &content),
				sender_user,
				room_id,
				&state_lock,
			)
			.await?;

		let room_version_id = self
			.services
			.state
			.get_room_version(room_id)
			.await?;

		let invite_room_state = self
			.services
			.state
			.summary_pdus(&pdu, &pdu_json, &room_version_id)
			.await;

		drop(state_lock);

		(pdu, pdu_json, invite_room_state, room_version_id)
	};

	let response = self
		.services
		.federation
		.execute(user_id.server_name(), create_invite::v2::Request {
			room_id: room_id.to_owned(),
			event_id: (*pdu.event_id).to_owned(),
			room_version: room_version_id.clone(),
			event: self
				.services
				.federation
				.format_pdu_into(pdu_json.clone(), Some(&room_version_id))
				.await,
			invite_room_state: invite_room_state
				.into_iter()
				.map(RawStrippedState::Pdu)
				.collect(),
			via: self
				.services
				.state_cache
				.servers_route_via(room_id)
				.await
				.ok(),
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
	if self.services.users.invites_blocked(user_id).await {
		return Err!(Request(InviteBlocked("{user_id} has blocked invites.")));
	}

	if !self
		.services
		.state_cache
		.is_joined(sender_user, room_id)
		.await
	{
		return Err!(Request(Forbidden(
			"You must be joined in the room you are trying to invite from."
		)));
	}

	let state_lock = self.services.state.mutex.lock(room_id).await;

	let mut content = RoomMemberEventContent {
		is_direct,
		reason: reason.cloned(),
		..RoomMemberEventContent::new(MembershipState::Invite)
	};

	self.services
		.profile
		.fill_profile_data(user_id, &mut content)
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
