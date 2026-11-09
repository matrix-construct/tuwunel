//! Builds and appends locally authored room events.
//!
//! Event construction, authorization, signing, state advancement, and timeline
//! insertion are coordinated under the caller's room state guard. Successfully
//! persisted events are then queued for federation to participating servers.

use std::{collections::HashSet, iter::once};

use futures::{FutureExt, StreamExt};
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, MilliSecondsSinceUnixEpoch, OwnedEventId,
	OwnedServerName, RoomId, UserId,
	events::{
		TimelineEventType,
		room::member::{MembershipState, RoomMemberEventContent},
	},
	room_id,
};
use serde_json::value::to_raw_value;
use tuwunel_core::{
	Err, Result, err, implement,
	utils::{BoolExt, IterStream, random_string},
};
use tuwunel_matrix::{
	StateKey,
	event::Event,
	pdu::{PduBuilder, PduEvent},
};

use super::RoomMutexGuard;

const NONCE_KEY: &str = "woof";

const NONCE_LENGTH: usize = 8;

/// Builds, signs, validates, and persists a locally authored room event.
///
/// The caller's state guard serializes state derivation and advancement while
/// the event is inserted into the accepted timeline. Federation enqueue occurs
/// after persistence, so its failure can be returned after the event and room
/// state have already been stored.
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
			.boxed() // size firewall
			.await?;
	}

	let (pdu, pdu_json) = self
		.create_hash_and_sign_event(pdu_builder, sender, room_id, state_lock)
		.boxed() // query-depth firewall
		.await?;

	self.append_signed_pdu(pdu, pdu_json, sender, state_lock)
		.boxed() // size firewall
		.await
}

/// Builds, signs, and persists a room's create event under the placeholder
/// room lock every hashed-ID create takes.
///
/// A client that fixed the timestamp or named the room ID it expects has
/// predicted the hash, so a collision is refused. A collision on a
/// server-chosen timestamp is retried once with a nonce key in the content,
/// which MSC4291 allows.
#[implement(super::Service)]
#[tracing::instrument(
	name = "build_and_append_create"
	level = "debug",
	skip(self, content),
	ret,
)]
pub async fn build_and_append_create_pdu(
	&self,
	content: CanonicalJsonObject,
	sender: &UserId,
	timestamp: Option<MilliSecondsSinceUnixEpoch>,
	expected_room_id: Option<&str>,
) -> Result<OwnedEventId> {
	let room_id = room_id!("!thiswillbereplaced");
	let state_lock = self.services.state.mutex.lock(room_id).await;
	let (pdu, pdu_json) = self
		.hash_create_unique(content, sender, timestamp, expected_room_id, room_id, &state_lock)
		.boxed() // query-depth firewall
		.await?;

	self.append_signed_pdu(pdu, pdu_json, sender, &state_lock)
		.boxed() // size firewall
		.await
}

#[implement(super::Service)]
async fn hash_create_unique(
	&self,
	content: CanonicalJsonObject,
	sender: &UserId,
	timestamp: Option<MilliSecondsSinceUnixEpoch>,
	expected_room_id: Option<&str>,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let (pdu, pdu_json) = self
		.hash_create_content(&content, sender, timestamp, room_id, state_lock)
		.await?;

	let mismatched = expected_room_id.is_some_and(|expected| pdu.room_id.as_str() != expected);
	if mismatched {
		return Err!(Request(InvalidParam(
			"The create event hashes to a different room ID than expected."
		)));
	}

	let predicted = timestamp.is_some() || expected_room_id.is_some();
	let collided = !predicted
		&& self
			.services
			.short
			.get_shortroomid(&pdu.room_id)
			.await
			.is_ok();

	if !collided {
		return Ok((pdu, pdu_json));
	}

	let content = with_nonce(content);

	self.hash_create_content(&content, sender, timestamp, room_id, state_lock)
		.await
}

#[implement(super::Service)]
async fn hash_create_content(
	&self,
	content: &CanonicalJsonObject,
	sender: &UserId,
	timestamp: Option<MilliSecondsSinceUnixEpoch>,
	room_id: &RoomId,
	state_lock: &RoomMutexGuard,
) -> Result<(PduEvent, CanonicalJsonObject)> {
	let pdu_builder = PduBuilder {
		event_type: TimelineEventType::RoomCreate,
		content: to_raw_value(content)?.into(),
		state_key: Some(StateKey::new()),
		timestamp,
		..Default::default()
	};

	self.create_hash_and_sign_event(pdu_builder, sender, room_id, state_lock)
		.await
}

fn with_nonce(mut content: CanonicalJsonObject) -> CanonicalJsonObject {
	let nonce = CanonicalJsonValue::String(random_string(NONCE_LENGTH));
	content.insert(NONCE_KEY.into(), nonce);
	content
}

/// Validates and persists an event built by `create_hash_and_sign_event`
/// under the state guard it was built under.
///
/// The event is checked, signed by the room's policy server, and stored before
/// it is queued for federation, so a queueing failure is returned for an event
/// already in the room.
#[implement(super::Service)]
#[tracing::instrument(
	name = "append_signed"
	level = "debug",
	skip(self, pdu_json, state_lock),
	ret,
)]
async fn append_signed_pdu(
	&self,
	pdu: PduEvent,
	pdu_json: CanonicalJsonObject,
	sender: &UserId,
	state_lock: &RoomMutexGuard,
) -> Result<OwnedEventId> {
	if self
		.services
		.admin
		.is_admin_room(pdu.room_id())
		.await
	{
		self.check_pdu_for_admin_room(&pdu, sender)
			.boxed() // cold arm: admin room
			.await?;
	}

	if *pdu.kind() == TimelineEventType::RoomRedaction {
		self.check_redaction_authorized(&pdu)
			.boxed() // query-depth firewall
			.await?;
	}

	let pdu_json = self.with_policy_signature(pdu_json, &pdu).await?;

	//TODO: Use proper room version here
	if *pdu.kind() == TimelineEventType::RoomCreate && pdu.room_id().server_name().is_none() {
		self.reserve_room_id(pdu.room_id()).await?;
	}

	// Store the event snapshot first; append_pdu installs current state after
	// the timeline row exists and before publishing its stream count.
	let statehashid = self.services.state.append_to_state(&pdu).await?;

	let pdu_id = self
		.append_pdu(
			&pdu,
			pdu_json,
			// Since this PDU references all pdu_leaves we can update the leaves
			// of the room
			once(pdu.event_id()),
			Some(statehashid),
			state_lock,
		)
		.boxed() // size firewall
		.await?;

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

	// Remove our server from the server list since it will be added to it by
	// room_servers() and/or the if statement above
	servers.remove(self.services.globals.server_name());

	self.services
		.sending
		.send_pdu_servers(servers.iter().map(AsRef::as_ref).stream(), &pdu_id)
		.await?;

	Ok(pdu.event_id().to_owned())
}

/// Has the room's policy server, if any, sign an event before it is federated.
///
/// Per [MSC4284](https://github.com/matrix-org/matrix-spec-proposals/pull/4284)
/// a refusal is an error, while a transport failure lets the event go out
/// unsigned.
#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug")]
async fn with_policy_signature(
	&self,
	mut pdu_json: CanonicalJsonObject,
	pdu: &PduEvent,
) -> Result<CanonicalJsonObject> {
	self.services
		.event_handler
		.sign_outgoing_pdu(&mut pdu_json, pdu)
		.boxed() // size firewall
		.await?;

	Ok(pdu_json)
}

/// Allocates the short ID of a room named by its create event's hash.
///
/// A short ID already present belongs to an earlier create event with the
/// same hash, so the new room is refused rather than merged into it.
#[implement(super::Service)]
#[tracing::instrument(skip(self), level = "debug")]
async fn reserve_room_id(&self, room_id: &RoomId) -> Result {
	let short = &self.services.short;
	if short.get_shortroomid(room_id).await.is_ok() {
		return Err!(Request(RoomInUse("A room with this create event already exists.")));
	}

	short.get_or_create_shortroomid(room_id).await;

	Ok(())
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
			self.check_admin_room_member(pdu, sender).await?;
		},
		| _ => {},
	}

	Ok(())
}

/// Refuses a leave or ban that would take the server user, or the last active
/// admin, out of the admins room.
///
/// Deactivation runs the same last-admin check under this room's state lock;
/// see `lock_admin_room`.
#[implement(super::Service)]
async fn check_admin_room_member<Pdu>(&self, pdu: &Pdu, sender: &UserId) -> Result
where
	Pdu: Event,
{
	let content: RoomMemberEventContent = pdu.get_content()?;

	let (server_user_refusal, last_admin_refusal) = match content.membership {
		| MembershipState::Leave => (
			"Server user cannot leave the admins room.",
			"Last admin cannot leave the admins room.",
		),
		| MembershipState::Ban if pdu.state_key().is_some() => (
			"Server cannot be banned from admins room.",
			"Last admin cannot be banned from admins room.",
		),
		| _ => return Ok(()),
	};

	// A key that is no user ID names no admin; the auth rules refuse it later.
	let Ok(target) = pdu
		.state_key()
		.filter(|v| v.starts_with('@'))
		.map_or(Ok(sender), TryInto::try_into)
	else {
		return Ok(());
	};

	if target == self.services.globals.server_user {
		return Err!(Request(Forbidden(error!("{server_user_refusal}"))));
	}

	if self
		.services
		.admin
		.user_is_last_admin(target)
		.boxed() // cold arm: admin membership check
		.await
	{
		return Err!(Request(Forbidden(error!("{last_admin_refusal}"))));
	}

	Ok(())
}

#[implement(super::Service)]
#[tracing::instrument(skip_all, level = "debug")]
async fn check_redaction_authorized<Pdu>(&self, pdu: &Pdu) -> Result
where
	Pdu: Event,
{
	let room_rules = self
		.services
		.state
		.get_room_version_rules(pdu.room_id())
		.await?;

	let Some(redacts_id) = pdu.redacts_id(&room_rules) else {
		return Ok(());
	};

	self.services
		.state_accessor
		.user_can_redact(&redacts_id, pdu.sender(), pdu.room_id(), false)
		.await?
		.into_option()
		.ok_or_else(|| err!(Request(Forbidden("User cannot redact this event."))))
}
