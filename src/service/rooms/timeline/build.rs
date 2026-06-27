use std::{collections::HashSet, iter::once};

use futures::{FutureExt, StreamExt};
use ruma::{
	OwnedEventId, OwnedServerName, RoomId, UserId,
	events::{
		TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
};
use serde_json::value::to_raw_value;
use tuwunel_core::{
	Err, Result, implement,
	matrix::{event::Event, pdu::PduBuilder, room_version},
	utils::{IterStream, ReadyExt},
};

use super::RoomMutexGuard;

/// Creates a new persisted data unit and adds it to a room. This function
/// takes a roomid_mutex_state, meaning that only this function is able to
/// mutate the room state.
#[implement(super::Service)]
#[tracing::instrument(
	name = "build_and_append"
	level = "debug",
	skip(self, state_lock),
	ret,
)]
pub async fn build_and_append_pdu(
	&self,
	mut pdu_builder: PduBuilder,
	sender: &UserId,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result<OwnedEventId> {
	if pdu_builder.event_type == TimelineEventType::RoomMember {
		self.sanitize_member_authorisation(&mut pdu_builder, room_id)
			.boxed()
			.await?;
	}

	let (pdu, mut pdu_json) = self
		.create_hash_and_sign_event(pdu_builder, sender, room_id, state_lock)
		.await?;

	self.check_pdu_for_suspended_sender(&pdu)
		.boxed()
		.await?;

	//TODO: Use proper room version here
	if *pdu.kind() == TimelineEventType::RoomCreate && pdu.room_id().server_name().is_none() {
		let _short_id = self
			.services
			.short
			.get_or_create_shortroomid(pdu.room_id())
			.await;
	}

	if self
		.services
		.admin
		.is_admin_room(pdu.room_id())
		.await
	{
		self.check_pdu_for_admin_room(&pdu, sender)
			.boxed()
			.await?;
	}

	// If redaction event is not authorized, do not append it to the timeline
	if *pdu.kind() == TimelineEventType::RoomRedaction {
		let room_version = self
			.services
			.state
			.get_room_version(pdu.room_id())
			.await?;

		let room_rules = room_version::rules(&room_version)?;

		let redacts_id = pdu.redacts_id(&room_rules);

		if let Some(redacts_id) = &redacts_id
			&& !self
				.services
				.state_accessor
				.user_can_redact(redacts_id, pdu.sender(), pdu.room_id(), false)
				.await?
		{
			return Err!(Request(Forbidden("User cannot redact this event.")));
		}
	}

	// MSC4284: ask the room's policy server (if any) to sign this event before
	// federating it. Refusal aborts; fail-open on transport errors.
	self.services
		.event_handler
		.sign_outgoing_pdu(&mut pdu_json, &pdu)
		.boxed()
		.await?;

	// We append to state before appending the pdu, so we don't have a moment in
	// time with the pdu without it's state. This is okay because append_pdu can't
	// fail.
	let statehashid = self.services.state.append_to_state(&pdu).await?;

	let pdu_id = self
		.append_pdu(
			&pdu,
			pdu_json,
			// Since this PDU references all pdu_leaves we can update the leaves
			// of the room
			once(pdu.event_id()),
			state_lock,
		)
		.boxed()
		.await?;

	// We set the room state after inserting the pdu, so that we never have a moment
	// in time where events in the current room state do not exist
	self.services
		.state
		.set_room_state(pdu.room_id(), statehashid, state_lock);

	let mut servers: HashSet<OwnedServerName> = self
		.services
		.state_cache
		.room_servers(pdu.room_id())
		.map(ToOwned::to_owned)
		.collect()
		.await;

	// In case we are kicking or banning a user, we need to inform their server of
	// the change
	if *pdu.kind() == TimelineEventType::RoomMember
		&& let Some(state_key_uid) = &pdu
			.state_key
			.as_ref()
			.and_then(|state_key| UserId::parse(state_key.as_str()).ok())
	{
		servers.insert(state_key_uid.server_name().to_owned());
	}

	// Remove all servers we are authoritative for from the federation list,
	// since they will be added to it by room_servers() and/or the if statement above.
	servers.retain(|s| !self.services.globals.server_is_ours(s));

	self.services
		.sending
		.send_pdu_servers(servers.iter().map(AsRef::as_ref).stream(), &pdu_id)
		.await?;

	Ok(pdu.event_id().to_owned())
}

#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug")]
async fn sanitize_member_authorisation(
	&self,
	pdu_builder: &mut PduBuilder,
	room_id: &RoomId,
) -> Result {
	let content: RoomMemberEventContent = pdu_builder.content.deserialize_as_unchecked()?;

	let Some(authorising_user) = &content.join_authorized_via_users_server else {
		return Ok(());
	};

	if content.membership != MembershipState::Join {
		return Err!(Request(BadJson(
			"join_authorised_via_users_server is only for member joins"
		)));
	}

	// Already joined or invited: strip the inapplicable authorising user.
	if let Some(target) = pdu_builder
		.state_key
		.as_deref()
		.and_then(|key| UserId::parse(key).ok())
		&& self
			.services
			.state_cache
			.user_membership(&target, room_id)
			.await
			.is_some_and(|m| matches!(m, MembershipState::Join | MembershipState::Invite))
	{
		let mut object = pdu_builder.content.deserialize()?;
		object.remove("join_authorised_via_users_server");
		pdu_builder.content = to_raw_value(&object)?.into();

		return Ok(());
	}

	if !self
		.services
		.globals
		.user_is_local(authorising_user)
	{
		return Err!(Request(InvalidParam(
			"Authorising user does not belong to this homeserver"
		)));
	}

	Ok(())
}

#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug")]
async fn check_pdu_for_admin_room<Pdu>(&self, pdu: &Pdu, sender: &UserId) -> Result
where
	Pdu: Event,
{
	match pdu.kind() {
		| TimelineEventType::RoomEncryption => {
			return Err!(Request(Forbidden(error!("Encryption not supported in admins room."))));
		},
		| TimelineEventType::RoomMember => {
			let target = pdu
				.state_key()
				.filter(|v| v.starts_with('@'))
				.unwrap_or(sender.as_str());

			let server_user = &self.services.globals.server_user.to_string();

			let content: RoomMemberEventContent = pdu.get_content()?;
			match content.membership {
				| MembershipState::Leave => {
					if target == server_user {
						return Err!(Request(Forbidden(error!(
							"Server user cannot leave the admins room."
						))));
					}

					let count = self
						.services
						.state_cache
						.room_members(pdu.room_id())
						.ready_filter(|user| self.services.globals.user_is_local(user))
						.ready_filter(|user| *user != target)
						.count()
						.boxed()
						.await;

					if count < 2 {
						return Err!(Request(Forbidden(error!(
							"Last admin cannot leave the admins room."
						))));
					}
				},

				| MembershipState::Ban if pdu.state_key().is_some() => {
					if target == server_user {
						return Err!(Request(Forbidden(error!(
							"Server cannot be banned from admins room."
						))));
					}

					let count = self
						.services
						.state_cache
						.room_members(pdu.room_id())
						.ready_filter(|user| self.services.globals.user_is_local(user))
						.ready_filter(|user| *user != target)
						.count()
						.boxed()
						.await;

					if count < 2 {
						return Err!(Request(Forbidden(error!(
							"Last admin cannot be banned from admins room."
						))));
					}
				},
				| _ => {},
			}
		},
		| _ => {},
	}

	Ok(())
}

/// MSC3823: reject PDUs from a suspended sender, except self-redaction of
/// their own event or self-leave. Synapse only checks `membership == leave`
/// and so lets suspended moderators kick others via /kick or a state PUT.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug")]
async fn check_pdu_for_suspended_sender<Pdu>(&self, pdu: &Pdu) -> Result
where
	Pdu: Event,
{
	if !self
		.services
		.users
		.is_suspended(pdu.sender())
		.await
	{
		return Ok(());
	}

	let allowed = match pdu.kind() {
		| TimelineEventType::RoomRedaction => self.is_self_redaction(pdu).await?,

		| TimelineEventType::RoomMember =>
			pdu.get_content()
				.map(|content: RoomMemberEventContent| {
					content.membership == MembershipState::Leave
						&& pdu.state_key() == Some(pdu.sender().as_str())
				})?,

		| _ => false,
	};

	if allowed {
		return Ok(());
	}

	Err!(Request(UserSuspended("Account is suspended.")))
}

#[implement(super::Service)]
async fn is_self_redaction<Pdu>(&self, pdu: &Pdu) -> Result<bool>
where
	Pdu: Event,
{
	let room_version = self
		.services
		.state
		.get_room_version(pdu.room_id())
		.await?;

	let room_rules = room_version::rules(&room_version)?;

	let Some(target_id) = pdu.redacts_id(&room_rules) else {
		return Ok(false);
	};

	let is_self = self
		.get_pdu(&target_id)
		.await
		.is_ok_and(|target| target.sender() == pdu.sender());

	Ok(is_self)
}
