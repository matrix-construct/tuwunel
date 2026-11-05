#![cfg(test)]

use std::{pin::pin, time::Duration};

use futures::future::{Either, select};
use serde_json::{Value, json, value::RawValue as RawJsonValue};
use tokio::time::sleep;
use tuwunel_core::{
	Err, Result, implement,
	ruma::{
		CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId,
		OwnedServerName, RoomId, RoomVersionId, TransactionId, UserId,
		api::{
			error::ErrorKind,
			federation::{
				membership::{RawStrippedState, create_invite::v2::Request as InviteRequest},
				transactions::send_transaction_message::v1::Request as TransactionRequest,
			},
		},
		events::{
			StateEventType,
			room::{
				member::{MembershipState, RoomMemberEventContent},
				message::RoomMessageEventContent,
			},
		},
	},
};
use tuwunel_matrix::pdu::PduBuilder;
use tuwunel_service::{Services, membership::Join};

use self::{
	client::{Client, register},
	federation::{prepare, send},
	fixture::boot,
	sign::sign,
};

mod client;
mod fixture;

#[path = "client/event.rs"]
mod event;

#[path = "client/put.rs"]
mod put;

#[expect(dead_code)] // Membership routes are not driven here.
mod federation;

#[expect(dead_code)] // Only the direct actions are used from the add-on.
#[path = "client/membership.rs"]
mod membership;

#[path = "fixture/sign.rs"]
mod sign;

const JOIN_REFUSAL: &str = "You are banned from the room.";
const KNOCK_REFUSAL: &str = "You cannot knock on a room you are banned from.";

const OWNER_TOKEN: &str = "federation-left-room-owner-access-token";
const MEMBER_TOKEN: &str = "federation-left-room-member-access-token";

/// How long a delivery must stay parked on the federation mutex.
///
/// It only has to outlast an unlocked delivery, since the lock holder releases
/// when the test says so, however slow the host.
const PARKED: Duration = Duration::from_millis(500);

type Signed = (OwnedEventId, Box<RawJsonValue>);

struct Accounts<'a> {
	services: &'a Services,
	base: &'a str,
	owner: Client<'a>,
	member: Client<'a>,
	owner_id: &'a UserId,
	member_id: &'a UserId,
}

/// A pushed PDU stops at a room this server left and at nothing else.
///
/// A left room keeps its stored events and frozen state, so a pushed event
/// could otherwise resolve against them and write a local member back into
/// the room. Backfill keeps reaching that room, a resident room keeps taking
/// pushes, and a push landing during a join waits for the join.
#[test]
fn pushed_pdus_stop_at_a_left_room() -> Result { boot("federation-left-room", [], exercise) }

async fn exercise(services: &Services, base: &str) -> Result {
	let owner_id = register(services, "left-room-owner", OWNER_TOKEN).await?;
	let member_id = register(services, "left-room-member", MEMBER_TOKEN).await?;
	let accounts = Accounts {
		services,
		base,
		owner: Client { services, base, token: OWNER_TOKEN },
		member: Client { services, base, token: MEMBER_TOKEN },
		owner_id: &owner_id,
		member_id: &member_id,
	};

	replayed_join_is_dropped(&accounts).await?;
	resident_room_takes_a_push(&accounts).await?;
	backfill_reaches_a_left_room(&accounts).await?;
	push_waits_out_a_join(&accounts).await?;
	frozen_ban_defers_to_the_room(&accounts).await?;
	stored_kick_spares_a_newer_invitation(&accounts).await?;
	rescissions_follow_the_invite(&accounts, true).await?;
	rescissions_follow_the_invite(&accounts, false).await?;
	malformed_and_legacy_invite_tails(&accounts).await
}

/// An old join replayed into a left room writes nothing.
///
/// The transaction still answers success for it, so the sender stops retrying.
async fn replayed_join_is_dropped(accounts: &Accounts<'_>) -> Result {
	let &Accounts { services, member_id, .. } = accounts;
	let room_id = accounts.room().await?;
	let replay = prepare(services, member_id, &room_id, MembershipState::Join).await?;

	accounts.leave(&room_id).await?;

	let before = services
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	let results = accounts.push(replay.pdu).await?;
	let membership = services
		.state_cache
		.user_membership(member_id, &room_id)
		.await;

	let stored = services.timeline.get_pdu(&replay.event_id).await;
	let after = services
		.state
		.get_room_shortstatehash(&room_id)
		.await?;

	assert_handled(&results, &replay.event_id);
	assert_eq!(membership, Some(MembershipState::Leave), "the replay moved the member");
	assert!(stored.is_err(), "the replay was stored");
	assert_eq!(after, before, "the replay moved the room's state");

	Ok(())
}

async fn resident_room_takes_a_push(accounts: &Accounts<'_>) -> Result {
	let room_id = accounts.room().await?;
	let (event_id, pdu) = accounts
		.signed_message(accounts.owner_id, &room_id)
		.await?;

	let results = accounts.push(pdu).await?;

	assert_handled(&results, &event_id);
	assert!(accounts.is_timeline(&event_id).await, "a resident push was not stored");

	Ok(())
}

async fn backfill_reaches_a_left_room(accounts: &Accounts<'_>) -> Result {
	let &Accounts { services, .. } = accounts;
	let (room_id, (event_id, pdu)) = accounts.left_with_held_message().await?;
	let backfilled = services
		.timeline
		.backfill_pdu(&room_id, services.globals.server_name(), pdu)
		.await?;

	assert!(backfilled, "backfill turned the event away");
	assert!(accounts.is_timeline(&event_id).await, "the backfilled event was not stored");

	Ok(())
}

/// A push landing during a join waits for it and then finds the server resident.
///
/// A remote join holds the federation mutex from before `make_join` until the
/// join event is appended. The test holds that mutex in its place and joins
/// locally, which takes only the state mutex. The event's sender is the user
/// who rejoins, since it is checked against current state.
async fn push_waits_out_a_join(accounts: &Accounts<'_>) -> Result {
	let (room_id, (event_id, pdu)) = accounts.left_with_held_message().await?;
	let federation_lock = accounts
		.services
		.event_handler
		.mutex_federation
		.lock(&room_id)
		.await;

	let delivery = pin!(accounts.push(pdu));
	let Either::Right(((), delivery)) = select(delivery, pin!(sleep(PARKED))).await else {
		return Err!("the transaction was answered while the join held the room");
	};

	accounts.member.act(&room_id, "join").await?;
	drop(federation_lock);

	let results = delivery.await?;

	assert_handled(&results, &event_id);
	assert!(accounts.is_timeline(&event_id).await, "the push was lost to the join window");

	Ok(())
}

/// A ban in a left room's frozen state no longer speaks for the room.
///
/// The room can lift a ban without telling a server that left, and such a push
/// is dropped, so only a resident server refuses on its own view. With no
/// server to ask, event auth over the frozen state still turns the user away.
async fn frozen_ban_defers_to_the_room(accounts: &Accounts<'_>) -> Result {
	let room_id = accounts.room().await?;

	accounts
		.owner
		.act_on(&room_id, "ban", accounts.member_id)
		.await?;

	let refused = accounts.refused_locally(&room_id).await;

	assert_eq!(refused, [true, true], "a resident room must refuse the join and the knock");

	accounts.owner.act(&room_id, "leave").await?;

	let refused = accounts.refused_locally(&room_id).await;

	assert_eq!(refused, [false, false], "a left room must not refuse on its frozen ban");

	Ok(())
}

/// A kick the room accepted cannot rescind an invitation that came after it.
///
/// The kick's sender is the new invitation's inviter and its signature holds,
/// which is all a rescission asks of an event the server has not seen. The
/// timeline holds this one, so delivering it again is only a duplicate.
async fn stored_kick_spares_a_newer_invitation(accounts: &Accounts<'_>) -> Result {
	let &Accounts { services, member_id, .. } = accounts;
	let room_id = accounts.room().await?;

	accounts
		.owner
		.act_on(&room_id, "kick", member_id)
		.await?;

	let member_key = member_id.as_str();
	let (kick_id, kick) = accounts
		.stored_state(&room_id, &StateEventType::RoomMember, member_key)
		.await?;

	let invite = accounts.signed_invite(&room_id).await?;

	assert!(accounts.is_timeline(&kick_id).await, "the kick was not stored");

	accounts.abandon(&room_id).await?;
	accounts.deliver_invite(&room_id, invite).await?;

	let invite_count = services
		.state_cache
		.get_invite_count(&room_id, member_id)
		.await?;

	let results = accounts.push(kick).await?;
	let membership = services
		.state_cache
		.user_membership(member_id, &room_id)
		.await;

	let left = services
		.state_cache
		.get_left_count(&room_id, member_id)
		.await;

	let replayed_count = services
		.state_cache
		.get_invite_count(&room_id, member_id)
		.await;

	assert_handled(&results, &kick_id);
	assert_eq!(membership, Some(MembershipState::Invite), "the replay rescinded the invitation");
	assert!(left.is_err(), "the replay wrote a leave");
	assert_eq!(replayed_count.ok(), Some(invite_count), "the replay moved the invitation");

	Ok(())
}

/// Replayed and unseen kicks name the old invite, never its replacement.
///
/// The first case applies the kick before re-inviting; the second holds it.
/// Both then deliver a genuine rescission naming the current invite.
async fn rescissions_follow_the_invite(accounts: &Accounts<'_>, applied: bool) -> Result {
	let &Accounts { services, member_id, .. } = accounts;
	let cache = &services.state_cache;
	let room = accounts.room().await?;

	accounts.member.act(&room, "leave").await?;

	let first = accounts.signed_invite(&room).await?;
	let reason = if applied { "replayed" } else { "unseen" };
	let old_kick = accounts
		.signed_rescission(&room, &first.0, reason)
		.await?;

	accounts
		.owner
		.send_text(&room, reason, "between invitations")
		.await?;

	let second = accounts.signed_invite(&room).await?;
	let second_id = second.0.clone();

	assert_ne!(first.0, second_id, "the fixture needs distinct invitations");

	accounts.abandon(&room).await?;
	accounts.deliver_invite(&room, first).await?;

	if applied {
		assert_handled(&accounts.push(old_kick.1.clone()).await?, &old_kick.0);
		assert_eq!(
			cache.user_membership(member_id, &room).await,
			Some(MembershipState::Leave),
			"the first rescission was not applied"
		);
	}

	assert!(
		services
			.timeline
			.get_pdu(&old_kick.0)
			.await
			.is_err(),
		"the old kick was stored"
	);

	accounts.deliver_invite(&room, second).await?;

	assert_eq!(
		cache.get_invite_event(&room, member_id).await?,
		Some(second_id.clone()),
		"the federation invite did not record its real ID"
	);

	assert_handled(&accounts.push(old_kick.1).await?, &old_kick.0);
	assert_eq!(
		cache.user_membership(member_id, &room).await,
		Some(MembershipState::Invite),
		"{reason} kick rescinded the newer invitation"
	);

	let current = accounts
		.signed_rescission(&room, &second_id, "current")
		.await?;

	assert_handled(&accounts.push(current.1).await?, &current.0);
	assert_eq!(
		cache.user_membership(member_id, &room).await,
		Some(MembershipState::Leave),
		"the current invitation could not be rescinded"
	);

	Ok(())
}

/// A damaged ID fails closed without hiding the invite from sync.
///
/// Replacing the value with its legacy eight-byte count restores the old
/// permissive rescission, even when the kick names another invitation.
async fn malformed_and_legacy_invite_tails(accounts: &Accounts<'_>) -> Result {
	let &Accounts { services, member_id, .. } = accounts;
	let cache = &services.state_cache;
	let room = accounts.room().await?;

	accounts.member.act(&room, "leave").await?;

	let invite = accounts.signed_invite(&room).await?;
	let kick = accounts
		.signed_rescission(&room, &invite.0, "malformed")
		.await?;

	accounts.abandon(&room).await?;
	accounts.deliver_invite(&room, invite).await?;

	let count = cache.get_invite_count(&room, member_id).await?;
	let map = &services.db["roomuserid_invitecount"];

	map.put((&room, member_id), (count, "invalid"));

	let sync: Value = services
		.client
		.clients
		.default
		.get(accounts.member.url("sync?timeout=0"))
		.bearer_auth(MEMBER_TOKEN)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	assert!(
		sync["rooms"]["invite"][room.as_str()].is_object(),
		"malformed tail hid the invite"
	);

	let response = accounts.push(kick.1).await?;

	assert!(response[kick.0.as_str()]["error"].is_string(), "malformed tail did not error");
	assert_eq!(cache.user_membership(member_id, &room).await, Some(MembershipState::Invite));

	map.put_raw((&room, member_id), count.to_be_bytes());

	assert_eq!(cache.get_invite_count(&room, member_id).await?, count);
	assert_eq!(cache.get_invite_event(&room, member_id).await?, None);

	let unrelated = EventId::parse("$earlier")?;
	let legacy = accounts
		.signed_rescission(&room, &unrelated, "legacy")
		.await?;

	assert_handled(&accounts.push(legacy.1).await?, &legacy.0);
	assert_eq!(
		cache.user_membership(member_id, &room).await,
		Some(MembershipState::Leave),
		"the legacy invitation lost its permissive rescission"
	);

	Ok(())
}

#[implement(Accounts, params = "<'_>")]
async fn signed_rescission(
	&self,
	room: &RoomId,
	invite: &EventId,
	reason: &str,
) -> Result<Signed> {
	let version = self.services.state.get_room_version(room).await?;
	let event = json!({
		"type": "m.room.member", "state_key": self.member_id, "sender": self.owner_id,
		"room_id": room, "content": { "membership": "leave", "reason": reason },
		"origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
		"depth": 2, "prev_events": [], "auth_events": [invite],
	});

	let (id, event) = sign_rescission(self.services, event, &version)?;
	let event = self
		.services
		.federation
		.format_pdu_into(event, Some(&version))
		.await;

	Ok((id, event))
}

fn sign_rescission(
	services: &Services,
	event: Value,
	version: &RoomVersionId,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
	let mut event = serde_json::from_value(event)?;
	let id = services
		.server_keys
		.gen_id_hash_and_sign_event(&mut event, version)?;

	Ok((id, event))
}

#[implement(Accounts, params = "<'_>")]
async fn room(&self) -> Result<OwnedRoomId> {
	let room_id = self
		.owner
		.create_room(&json!({ "preset": "public_chat" }))
		.await?;

	self.member.act(&room_id, "join").await?;

	Ok(room_id)
}

#[implement(Accounts, params = "<'_>")]
async fn leave(&self, room_id: &RoomId) -> Result {
	self.member.act(room_id, "leave").await?;
	self.abandon(room_id).await
}

#[implement(Accounts, params = "<'_>")]
async fn abandon(&self, room_id: &RoomId) -> Result {
	self.owner.act(room_id, "leave").await?;

	let resident = self
		.services
		.state_cache
		.server_in_room(self.services.globals.server_name(), room_id)
		.await;

	assert!(!resident, "every local user left, so the server is not in the room");

	Ok(())
}

#[implement(Accounts, params = "<'_>")]
async fn push(&self, pdu: Box<RawJsonValue>) -> Result<Value> {
	let origin = self.services.globals.server_name().to_owned();
	let (status, body) = send(self.services, self.base, transaction(origin, pdu)).await?;

	assert_eq!(status, 200, "the transaction was refused: {body}");

	let response: Value = serde_json::from_str(&body)?;

	Ok(response["pdus"].clone())
}

#[implement(Accounts, params = "<'_>")]
async fn stored_state(
	&self,
	room_id: &RoomId,
	kind: &StateEventType,
	state_key: &str,
) -> Result<Signed> {
	let &Self { services, .. } = self;
	let event = services
		.state_accessor
		.room_state_get(room_id, kind, state_key)
		.await?;

	let json = services
		.timeline
		.get_pdu_json(&event.event_id)
		.await?;

	let pdu = services
		.federation
		.format_pdu_into(json, None)
		.await;

	Ok((event.event_id, pdu))
}

#[implement(Accounts, params = "<'_>")]
async fn signed_invite(&self, room_id: &RoomId) -> Result<Signed> {
	let content = RoomMemberEventContent::new(MembershipState::Invite);
	let builder = PduBuilder::state(self.member_id.to_string(), &content);

	self.signed(self.owner_id, room_id, builder).await
}

#[implement(Accounts, params = "<'_>")]
async fn signed_message(&self, user_id: &UserId, room_id: &RoomId) -> Result<Signed> {
	let content = RoomMessageEventContent::text_plain("held back");
	let builder = PduBuilder::timeline(&content);

	self.signed(user_id, room_id, builder).await
}

#[implement(Accounts, params = "<'_>")]
async fn signed(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	builder: PduBuilder,
) -> Result<Signed> {
	let (event, json) = sign(self.services, user_id, room_id, builder).await?;
	let pdu = self
		.services
		.federation
		.format_pdu_into(json, None)
		.await;

	Ok((event.event_id, pdu))
}

/// Invite the member over the federation route, as a remote inviter's server
/// does once this one has left.
///
/// The stripped state carries the create event, which is where a rescission
/// reads the room version.
#[implement(Accounts, params = "<'_>")]
async fn deliver_invite(&self, room_id: &RoomId, (event_id, event): Signed) -> Result {
	let &Self { services, base, .. } = self;
	let room_version = services.state.get_room_version(room_id).await?;
	let (_, create) = self
		.stored_state(room_id, &StateEventType::RoomCreate, "")
		.await?;

	let request = InviteRequest {
		room_id: room_id.to_owned(),
		event_id,
		room_version,
		event,
		invite_room_state: vec![RawStrippedState::Pdu(create)],
		via: None,
	};

	let (status, body) = send(services, base, request).await?;

	assert_eq!(status, 200, "the invitation was refused: {body}");

	Ok(())
}

/// Whether an event was accepted into the room rather than held as an outlier.
///
/// A read failure counts as not accepted.
#[implement(Accounts, params = "<'_>")]
async fn is_timeline(&self, event_id: &EventId) -> bool {
	self.services
		.timeline
		.get_pdu_id(event_id)
		.await
		.is_ok()
}

/// A room the server has left, with a member's message held from before then.
///
/// The event descends from the room as it stood, so it can still be delivered
/// after the room moved on.
#[implement(Accounts, params = "<'_>")]
async fn left_with_held_message(&self) -> Result<(OwnedRoomId, Signed)> {
	let room_id = self.room().await?;
	let held = self
		.signed_message(self.member_id, &room_id)
		.await?;

	self.leave(&room_id).await?;

	Ok((room_id, held))
}

/// Join and knock as the member, and report which the server refused itself.
///
/// No server is named to ask, so both must fail; the answers say whether each
/// failed on the local ban refusal rather than on event auth.
#[implement(Accounts, params = "<'_>")]
async fn refused_locally(&self, room_id: &RoomId) -> [bool; 2] {
	let &Self { services, member_id, .. } = self;
	let join = Join {
		sender_user: member_id,
		room_id,
		orig_room_id: None,
		reason: None,
		servers: &[],
		is_appservice: false,
		extra_content: None,
	};

	let joined = services.membership.join(join).await;
	let state_lock = services.state.mutex.lock(room_id).await;
	let knocked = services
		.membership
		.knock(member_id, room_id, None, None, &[], &state_lock)
		.await;

	[refused_with(joined, JOIN_REFUSAL), refused_with(knocked, KNOCK_REFUSAL)]
}

fn assert_handled(results: &Value, event_id: &EventId) {
	assert_eq!(results[event_id.as_str()], json!({}), "unexpected result: {results}");
}

fn transaction(origin: OwnedServerName, pdu: Box<RawJsonValue>) -> TransactionRequest {
	let now = MilliSecondsSinceUnixEpoch::now();
	let mut request = TransactionRequest::new(TransactionId::new(), origin, now); // non_exhaustive

	request.pdus = vec![pdu];
	request
}

fn refused_with(outcome: Result, message: &str) -> bool {
	let error = outcome.expect_err("the banned user was let in");

	assert!(matches!(error.kind(), ErrorKind::Forbidden), "unexpected refusal: {error}");

	error.to_string().contains(message)
}
