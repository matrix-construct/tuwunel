use futures::FutureExt;
use ruma::{
	OwnedServerName, RoomId, UserId,
	api::federation::membership::create_invite,
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
	let (pdu, pdu_json, invite_room_state) = {
		let state_lock = self.services.state.mutex.lock(room_id).await;

		let content = RoomMemberEventContent {
			displayname: self
				.services
				.users
				.displayname(user_id)
				.await
				.ok(),
			avatar_url: self.services.users.avatar_url(user_id).await.ok(),
			is_direct: Some(is_direct),
			reason: reason.cloned(),
			..RoomMemberEventContent::new(MembershipState::Invite)
		};

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

		let invite_room_state = self.services.state.summary_stripped(&pdu).await;

		drop(state_lock);

		(pdu, pdu_json, invite_room_state)
	};

	let room_version_id = self
		.services
		.state
		.get_room_version(room_id)
		.await?;

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
				.map(Into::into)
				.collect(),
			via: self
				.services
				.state_cache
				.servers_route_via(room_id)
				.await
				.ok(),
		})
		.await?;

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

	let content = RoomMemberEventContent {
		displayname: self
			.services
			.users
			.displayname(user_id)
			.await
			.ok(),
		avatar_url: self.services.users.avatar_url(user_id).await.ok(),
		blurhash: self.services.users.blurhash(user_id).await.ok(),
		is_direct: Some(is_direct),
		reason: reason.cloned(),
		..RoomMemberEventContent::new(MembershipState::Invite)
	};

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
