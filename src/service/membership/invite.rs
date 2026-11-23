use std::time::Duration;

use futures::{
	FutureExt,
	future::{join, join3},
};
use ruma::{
	CanonicalJsonObject, EventId, OwnedEventId, OwnedUserId, RoomId, RoomVersionId, ServerName,
	ServerSigningKeyId, UserId,
	api::{
		error::ErrorKind,
		federation::membership::{RawStrippedState, create_invite::v2::Request as InviteRequest},
	},
	canonical_json::redact,
	events::{
		TimelineEventType,
		invite_permission_config::InvitePermission,
		room::{
			join_rules::JoinRule,
			member::{MembershipState, RoomMemberEventContent},
		},
	},
	signatures::verify_json,
};
use serde_json::value::to_raw_value;
use tokio::time::timeout;
use tuwunel_core::{
	Err, Result, async_noinline, at, err, implement,
	utils::future::{ReadyBoolExt, and4},
};
use tuwunel_matrix::{
	event::gen_event_id_canonical_json,
	pdu::{
		Content, PduBuilder, Unsigned, into_outgoing_federation, server_signatures,
		with_only_server_signatures, with_server_signatures,
	},
	room_version::rules as room_version_rules,
};

use super::Service;
use crate::{rooms::state::RoomMutexGuard, server_keys::PubKeyMap};

/// A member event inviting a user on another server, with what sending it
/// needs.
///
/// `federate_invite` consumes it. Without `answer_within`, the wait for the
/// invitee's server has no bound of its own.
pub struct RemoteInvite<'a> {
	/// The event's sender.
	pub sender: &'a UserId,

	/// The invited user, the event's state key as `remote_invitee` returns it.
	pub invitee: &'a UserId,

	/// The room the user is invited to.
	pub room_id: &'a RoomId,

	/// The member event, not yet built.
	pub invite: PduBuilder,

	/// The room's state guard, released once the event is built.
	pub state_lock: RoomMutexGuard,

	/// How long the invitee's server may take to countersign, when bounded.
	pub answer_within: Option<Duration>,
}

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
			.await?;
	} else {
		self.remote_invite(sender_user, user_id, room_id, reason.map(String::as_str), is_direct)
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
	reason: Option<&str>,
	is_direct: bool,
) -> Result {
	let content = RoomMemberEventContent {
		is_direct,
		reason: reason.map(ToOwned::to_owned),
		..RoomMemberEventContent::new(MembershipState::Invite)
	};

	let invite = self
		.services
		.profile
		.fill_content(user_id, content)
		.map(|content| PduBuilder::state(user_id.as_str(), &content))
		.await;

	let state_lock = self.services.state.mutex.lock(room_id).await;

	self.federate_invite(RemoteInvite {
		sender: sender_user,
		invitee: user_id,
		room_id,
		invite,
		state_lock,
		answer_within: None,
	})
	.await
	.and_then(at!(1))
}

/// Invites a user on another server with a member event the caller built.
///
/// The caller hands over the room's state guard, which is released once the
/// event is built, so no federation request waits under it.
///
/// The room's policy server, if any, signs the event and the invitee's server
/// must countersign it: a refusal from either, an answer without the invitee
/// server's valid signature, or no answer within `answer_within` is the outer
/// error and stores nothing. Once stored, the event keeps the unsigned data it
/// was built with, and a failure to send it on to the room's servers is the
/// inner error. The content goes out as built, unknown keys included.
#[implement(Service)]
// cold arm
#[async_noinline]
#[tracing::instrument(
	name = "federate",
	level = "debug",
	skip_all,
	fields(
		%sender,
		%room_id,
		%invitee,
	)
)]
pub async fn federate_invite<'a>(
	&'a self,
	RemoteInvite {
		sender,
		invitee,
		room_id,
		invite,
		state_lock,
		answer_within,
	}: RemoteInvite<'a>,
) -> Result<(OwnedEventId, Result)> {
	let event =
		self.services
			.timeline
			.create_hash_and_sign_event(invite, sender, room_id, &state_lock);

	let room_version_id = self.services.state.get_room_version(room_id);
	let shortstatehash = self
		.services
		.state
		.get_room_shortstatehash(room_id);

	let (event, room_version_id, shortstatehash) =
		join3(event, room_version_id, shortstatehash).await;

	drop(state_lock);

	let (pdu, pdu_json) = event?;
	let room_version_id = room_version_id?;

	// MSC4284: the policy server signs before the invitee's server sees the event.
	let pdu_json = self
		.services
		.event_handler
		.sign_outgoing_pdu(pdu_json, &pdu)
		.boxed() // size firewall
		.await?;

	let invite_room_state = self
		.services
		.state
		.summary_pdus_at(&pdu, &pdu_json, &room_version_id, shortstatehash)
		.await;

	let outgoing = into_outgoing_federation(pdu_json, &room_version_id);
	let event = to_raw_value(&outgoing)?;
	let via = self
		.services
		.state_cache
		.servers_route_via(room_id)
		.map(Result::ok)
		.await;

	let invite_room_state = invite_room_state
		.into_iter()
		.map(RawStrippedState::Pdu)
		.collect();

	let request = InviteRequest {
		room_id: room_id.to_owned(),
		event_id: pdu.event_id.clone(),
		room_version: room_version_id,
		event,
		invite_room_state,
		via,
	};

	let server = invitee.server_name();
	let handshake = self.handshake(server, request, outgoing);
	let countersigned = match answer_within {
		| None => handshake.await,
		| Some(limit) => timeout(limit, handshake)
			.await
			.unwrap_or_else(|_| {
				Err!("{server} did not answer the invite within {}s", limit.as_secs())
			}),
	}?;

	let event_id = pdu.event_id;

	self.accept_invite(room_id, &event_id, countersigned, pdu.unsigned)
		.await
		.map(|sent| (event_id, sent))
}

/// Sends an invite to the invitee's server and returns it countersigned.
///
/// A refusal, an answer for an event other than the one sent, or one without
/// the server's valid countersignature is an error.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn handshake(
	&self,
	server: &ServerName,
	request: InviteRequest,
	outgoing: CanonicalJsonObject,
) -> Result<CanonicalJsonObject> {
	let room_version_id = request.room_version.clone();
	let sent_id = request.event_id.clone();
	let response = self
		.services
		.federation
		.execute(server, request)
		.await
		.map_err(|e| match e.kind() {
			| ErrorKind::IncompatibleRoomVersion { .. } | ErrorKind::UnsupportedRoomVersion =>
				err!(Request(UnsupportedRoomVersion(
					"Server {server} does not support room version {room_version_id}."
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
	let (event_id, answer) = gen_event_id_canonical_json(&response.event, &room_version_id)
		.map_err(|e| {
			err!(Request(BadJson(warn!("Could not convert event to canonical JSON: {e}"))))
		})?;

	if sent_id != event_id {
		return Err!(Request(BadJson(debug_warn!(
			%server, %sent_id, %event_id,
			"Server sent event with wrong event ID"
		))));
	}

	self.countersigned(outgoing, &answer, server, &room_version_id)
		.await
}

/// The event this server sent, with the invitee server's signatures from its
/// answer.
///
/// Only that server's signatures under keys it publishes are taken from the
/// answer, so every other field stays as this server built it. At least one of
/// them must verify over the event as sent; an answer without a valid one is
/// the peer's fault.
///
/// A signature the event already holds under that server's name (the room's
/// policy signature, when that server is the room's policy server) is kept as
/// built and its key is never looked up, so every countersignature that
/// verifies survives the merge.
#[implement(Service)]
async fn countersigned(
	&self,
	outgoing: CanonicalJsonObject,
	answer: &CanonicalJsonObject,
	server: &ServerName,
	room_version_id: &RoomVersionId,
) -> Result<CanonicalJsonObject> {
	let held = server_signatures(&outgoing, server);
	let added = server_signatures(answer, server)
		.ok_or_else(|| {
			err!(BadServerResponse(debug_warn!(
				%server,
				"Invite answer lacks the invitee server's signature."
			)))
		})?
		.iter()
		.filter(|&(key_id, _)| held.is_none_or(|held| !held.contains_key(key_id)));

	let key_ids = added
		.clone()
		.filter_map(|(key_id, _)| <&ServerSigningKeyId>::try_from(key_id.as_str()).ok());

	let public_keys = self
		.services
		.server_keys
		.get_pubkeys_for(server, key_ids)
		.await;

	let signatures: CanonicalJsonObject = added
		.filter(|(key_id, _)| public_keys.contains_key(key_id.as_str()))
		.map(|(key_id, signature)| (key_id.clone(), signature.clone()))
		.collect();

	let rules = room_version_rules(room_version_id)?;
	let redacted = redact(outgoing.clone(), &rules.redaction, None)
		.map_err(|error| err!(error!(%error, "Invite could not be redacted.")))?;

	let invitee_signed = with_only_server_signatures(redacted, server, signatures.clone());
	let public_keys = PubKeyMap::from([(server.as_str().to_owned(), public_keys)]);

	verify_json(&public_keys, &invitee_signed).map_err(|error| {
		err!(BadServerResponse(debug_warn!(
			%server,
			%error,
			"Invite answer's countersignature failed verification."
		)))
	})?;

	Ok(with_server_signatures(outgoing, server, signatures))
}

/// Accepts a countersigned invite, built by this server, into its room.
///
/// The event enters through the receive path's entry for events this server
/// built, so the gates that path applies to peers do not refuse it after the
/// invitee has accepted, and it keeps its unsigned data. An outer error means
/// the invite was not stored. Once it is, the event is sent to the room's
/// servers; a failure there, or one the receive path reported after storing
/// it, is the inner error.
#[implement(Service)]
async fn accept_invite(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	countersigned: CanonicalJsonObject,
	unsigned: Option<Unsigned>,
) -> Result<Result> {
	let accepted = self
		.services
		.event_handler
		.handle_local_pdu(room_id, event_id, countersigned, unsigned)
		.await;

	let (pdu_id, accepted) = match accepted {
		| Ok(Some((pdu_id, _))) => (pdu_id, Ok(())),
		| Ok(None) =>
			return Err!(Request(InvalidParam(
				"Could not accept incoming PDU as timeline event."
			))),
		| Err(error) => match self.services.timeline.get_pdu_id(event_id).await {
			| Ok(pdu_id) => (pdu_id, Err(error)),
			| Err(_) => return Err(error),
		},
	};

	let sent = self
		.services
		.sending
		.send_pdu_room(room_id, &pdu_id)
		.await;

	Ok(accepted.and(sent))
}

/// The invitee of a member event inviting a user on another server.
///
/// Such an event goes out through `federate_invite`. Appended directly, it
/// reaches the invitee's server only in a transaction, which that server drops
/// while it is not in the room.
#[implement(Service)]
pub fn remote_invitee(&self, pdu: &PduBuilder) -> Option<OwnedUserId> {
	pdu.state_key
		.as_deref()
		.filter(|_| pdu.event_type == TimelineEventType::RoomMember)
		.and_then(|state_key| <&UserId>::try_from(state_key).ok())
		.filter(|user_id| !self.services.globals.user_is_local(user_id))
		.filter(|_| is_invite(&pdu.content))
		.map(ToOwned::to_owned)
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

fn is_invite(content: &Content) -> bool {
	content
		.get_field("membership")
		.ok()
		.flatten()
		.is_some_and(|membership: MembershipState| membership == MembershipState::Invite)
}
