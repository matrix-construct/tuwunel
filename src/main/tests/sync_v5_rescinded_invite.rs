#![cfg(test)]

use serde_json::{Value, json};
use tuwunel_core::{
	Result, implement,
	ruma::{
		CanonicalJsonObject, EventId, MilliSecondsSinceUnixEpoch, OwnedEventId, OwnedRoomId,
		RoomId, RoomVersionId, UserId,
		events::{
			AnyStrippedStateEvent, StateEventType,
			room::member::{MembershipState, RoomMemberEventContent},
		},
		serde::Raw,
	},
	utils::result::NotFound,
};
use tuwunel_matrix::pdu::{PduBuilder, into_outgoing_federation};
use tuwunel_service::{
	Services,
	rooms::{short::ShortStateHash, state_cache::MembershipUpdate},
};

use self::{
	client::{Client, field, register},
	fixture::boot,
	member_event::find_member_event,
	sign::sign,
	state_cache::membership_update,
};

mod client;
mod fixture;
mod sliding_sync;

#[path = "client/event.rs"]
mod event;

#[path = "client/put.rs"]
mod put;

#[path = "client/member_event.rs"]
mod member_event;

#[expect(dead_code)] // Only the direct actions are used from the add-on.
#[path = "client/membership.rs"]
mod membership;

#[path = "sliding_sync/resume.rs"]
mod resume;

#[path = "fixture/sign.rs"]
mod sign;

#[path = "fixture/state_cache.rs"]
mod state_cache;

const INVITER_TOKEN: &str = "sync-v5-rescinded-invite-inviter-access-token";
const INVITEE_TOKEN: &str = "sync-v5-rescinded-invite-invitee-access-token";
const BYSTANDER_TOKEN: &str = "sync-v5-rescinded-invite-bystander-access-token";
const ROOM_VERSION: RoomVersionId = RoomVersionId::V11;

/// A room the server left, and an invitee's sync position from its life.
type LeftRoom = (OwnedRoomId, String);

struct Accounts<'a> {
	services: &'a Services,
	inviter: &'a UserId,
	invitee: &'a UserId,
	inviter_client: Client<'a>,
	invitee_client: Client<'a>,
}

struct History {
	joined: bool,
	unreadable: bool,
}

/// A federated invite rescission reaches sync as the invitee's own leave.
///
/// The server is not resident in the room, so the kick reaches no room state.
/// Sliding-sync clients read the departure from a member event rather than
/// from `membership`, so the retracted room must carry one, as the legacy left
/// room already does.
#[test]
fn rescinded_invite_carries_leave_member() -> Result {
	boot("sync-v5-rescinded-invite", [], exercise)
}

async fn exercise(services: &Services, base: &str) -> Result {
	let inviter = register(services, "rescinder", INVITER_TOKEN).await?;
	let invitee = register(services, "rescindee", INVITEE_TOKEN).await?;
	let bystander = register(services, "rescind-bystander", BYSTANDER_TOKEN).await?;
	let accounts = Accounts {
		services,
		inviter: &inviter,
		invitee: &invitee,
		inviter_client: Client { services, base, token: INVITER_TOKEN },
		invitee_client: Client { services, base, token: INVITEE_TOKEN },
	};

	let unknown = RoomId::parse("!rescinded:remote.test")?;

	accounts.plant_invite(&unknown).await?;
	accounts
		.rescission_reaches_sync(&unknown, "rescind")
		.await?;

	let visited = accounts.left_room_once_visited().await?;

	accounts
		.rescission_reaches_sync(&visited, "rescind-visited")
		.await?;

	let never_visited = accounts.left_room().await?;

	accounts
		.rescission_stands_alone(never_visited, "rescind-never-visited")
		.await?;

	let invited = accounts.left_room_with_invite().await?;

	accounts
		.rescission_stands_alone(invited, "rescind-invited")
		.await?;

	accounts
		.left_history(History { joined: true, unreadable: false })
		.await?;

	accounts
		.left_history(History { joined: false, unreadable: false })
		.await?;

	accounts
		.left_history(History { joined: true, unreadable: true })
		.await?;

	accounts.forgotten_member().await?;
	accounts.recorded_ban_stays_a_ban().await?;
	accounts
		.missing_snapshot_keeps_the_timeline()
		.await?;

	accounts
		.unreadable_last_event_keeps_the_timeline()
		.await?;

	accounts
		.other_sender_is_ignored(&bystander)
		.await?;

	accounts.held_kick_still_rescinds().await
}

#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn left_history(&self, History { joined, unreadable }: History) -> Result {
	let &Self { services, inviter, invitee, .. } = self;
	let owner = &self.inviter_client;
	let client = &self.invitee_client;
	let room_id = self.public_room().await?;

	owner.act_on(&room_id, "invite", invitee).await?;
	if joined {
		client.act(&room_id, "join").await?;
	}

	let before = self.position().await?;

	client.act(&room_id, "leave").await?;
	let leave = self.member_event_id(&room_id, invitee).await?;
	let past = self.position().await?;
	let message = owner
		.send_text(&room_id, "later", "after departure")
		.await?;

	let name = json!({ "name": "after departure" });
	let path = format!("rooms/{room_id}/state/m.room.name");
	let name = owner.put_event(&path, &name).await?;

	self.abandon_with_invite(&room_id).await?;
	self.push_kick(&room_id, inviter).await?;
	if unreadable {
		let pdu: Value = services.timeline.get(&leave).await?;
		let bytes = serde_json::to_vec(&unreadable_reason(pdu))?;
		let pdu_id = services.timeline.get_pdu_id(&leave).await?;

		services.db["pduid_pdu"].insert(pdu_id.as_ref(), bytes);
	}

	let bounded = joined && !unreadable;

	for position in [&before, &past] {
		let reply = client.sync_v3(Some(position)).await?;
		let room = &reply["rooms"]["leave"][room_id.as_str()];
		let timeline = &room["timeline"]["events"];

		assert!(room.is_object(), "left room missing: {reply}");
		assert_eq!(has_event(timeline, &message), !bounded, "history bound: {reply}");
		if bounded && position == &before {
			assert_eq!(timeline.as_array().unwrap().last().unwrap()["event_id"], leave.as_str());
		}

		if bounded && position == &past {
			let member = own_member(&room["state"]["events"], invitee);

			assert_eq!(member["content"]["membership"], "leave", "late notification: {reply}");
		}

		let query = format!("{position}&org.matrix.msc4222.use_state_after=true");
		let after = client.sync_v3(Some(&query)).await?;
		let state = &after["rooms"]["leave"][room_id.as_str()]["org.matrix.msc4222.state_after"]
			["events"];

		assert_eq!(has_event(state, &name), !bounded, "state bound: {after}");

		let resumed = client
			.sync_v3(Some(field(&reply, "next_batch")?))
			.await?;

		let rooms = &resumed["rooms"]["leave"];

		assert!(rooms.get(room_id.as_str()).is_none(), "repeated notification: {resumed}");
	}

	Ok(())
}

fn unreadable_reason(mut pdu: Value) -> Value {
	pdu["content"]["reason"] = json!(false);

	pdu
}

fn has_event(events: &Value, event_id: &OwnedEventId) -> bool {
	events
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["event_id"] == event_id.as_str())
}

#[implement(Accounts, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn forgotten_member(&self) -> Result {
	let &Self { services, inviter, invitee, .. } = self;
	let room_id = self.joined_room().await?;
	let member = self.member_event_id(&room_id, invitee).await?;

	self.move_left_rows(&room_id, &[inviter, invitee])
		.await?;

	services.state_cache.forget(&room_id, invitee);

	let request = json!({ "room_subscriptions": {
		room_id.as_str(): { "required_state": [["m.room.member", "$ME"]], "timeline_limit": 0 }
	} });

	let reply = self.invitee_client.sync(None, &request).await?;
	let room = &reply["rooms"][room_id.as_str()];

	assert_eq!(room["membership"], "ban", "forgotten membership: {reply}");
	assert_eq!(
		room["required_state"][0]["event_id"],
		member.as_str(),
		"forgotten member lost: {reply}"
	);

	let pdu_id = services.timeline.get_pdu_id(&member).await?;

	for missing in [true, false] {
		if missing {
			services.db["pduid_pdu"].del(pdu_id.as_ref());
		} else {
			services.db["pduid_pdu"].insert(pdu_id.as_ref(), b"{");
		}

		let reply = self.invitee_client.sync(None, &request).await?;
		let room = &reply["rooms"][room_id.as_str()];

		assert_eq!(room["membership"], "ban", "fallback membership: {reply}");
		assert!(
			room["required_state"]
				.as_array()
				.is_none_or(Vec::is_empty),
			"fallback synthesized leave: {reply}"
		);
	}

	Ok(())
}

/// Store an out-of-band invite the way a federated invite leaves it.
///
/// Its stripped state names the room version and the inviter, and no room
/// state is written.
#[implement(Accounts, params = "<'_>")]
async fn plant_invite(&self, room_id: &RoomId) -> Result {
	let &Self { services, inviter, invitee, .. } = self;
	let create = json!({
		"type": "m.room.create",
		"state_key": "",
		"sender": inviter,
		"content": { "room_version": ROOM_VERSION },
	});

	let member = json!({
		"type": "m.room.member",
		"state_key": invitee,
		"sender": inviter,
		"content": { "membership": "invite" },
	});

	let last_state: Vec<Raw<AnyStrippedStateEvent>> = [create, member]
		.into_iter()
		.map(serde_json::from_value)
		.collect::<Result<_, _>>()?;

	let count = services.globals.next_count();
	let invite = membership_update(room_id, invitee, MembershipState::Invite, *count);

	services
		.state_cache
		.update_membership(MembershipUpdate {
			sender: inviter,
			last_state: Some(last_state),
			update_joined_count: false,
			..invite
		})
		.await
}

/// Deliver the inviter's kick of a pending invite and read the departure back.
///
/// A room the server left holds state, yet takes the rescission the same way:
/// the legacy left room carries the invitee's own leave and none of that state.
#[implement(Accounts, params = "<'_>")]
async fn rescission_reaches_sync(&self, room_id: &RoomId, connection: &str) -> Result {
	let &Self { inviter, invitee, .. } = self;
	let client = &self.invitee_client;
	let room = room_id.as_str();
	let request = list(connection);
	let invited = client.sync(None, &request).await?;
	let invited_v3 = client.sync_v3(None).await?;
	let before = self.state_hash(room_id).await;

	assert_eq!(invited["rooms"][room]["membership"], "invite", "invite missing: {invited}");

	self.push_kick(room_id, inviter).await?;

	let membership = self.membership(room_id).await;
	let after = self.state_hash(room_id).await;

	assert_eq!(membership, Some(MembershipState::Leave), "rescission not applied");
	assert_eq!(after, before, "room state moved");

	let left = client.sync(Some(&invited), &request).await?;
	let retracted = &left["rooms"][room];
	let member = own_member(&retracted["required_state"], invitee);

	assert_eq!(retracted["membership"], "leave", "room not retracted: {left}");
	assert_eq!(member["content"]["membership"], "leave", "no own leave event: {left}");

	self.lone_leave(room_id, field(&invited_v3, "next_batch")?)
		.await
}

#[implement(Accounts, params = "<'_>")]
async fn state_hash(&self, room_id: &RoomId) -> Option<ShortStateHash> {
	self.services
		.state
		.get_room_shortstatehash(room_id)
		.await
		.ok()
}

/// Push a sender's kick of the invitee.
///
/// Returns its ID and whether the handler reported a timeline position for it.
#[implement(Accounts, params = "<'_>")]
async fn push_kick(&self, room_id: &RoomId, sender: &UserId) -> Result<(OwnedEventId, bool)> {
	let &Self { services, invitee, .. } = self;
	let invite_event = services
		.state_cache
		.get_invite_event(room_id, invitee)
		.await
		.optional()?
		.flatten();

	let (event_id, kick) =
		sign_kick(services, room_id, sender, invitee, invite_event.as_deref())?;

	let handled = self.deliver(room_id, &event_id, kick).await?;

	Ok((event_id, handled))
}

#[implement(Accounts, params = "<'_>")]
async fn deliver(
	&self,
	room_id: &RoomId,
	event_id: &EventId,
	event: CanonicalJsonObject,
) -> Result<bool> {
	let &Self { services, .. } = self;
	let handled = services
		.event_handler
		.handle_pushed_pdu(services.globals.server_name(), room_id, event_id, event)
		.await?;

	Ok(handled.is_some())
}

#[implement(Accounts, params = "<'_>")]
async fn membership(&self, room_id: &RoomId) -> Option<MembershipState> {
	self.services
		.state_cache
		.user_membership(self.invitee, room_id)
		.await
}

#[implement(Accounts, params = "<'_>")]
async fn lone_leave(&self, room_id: &RoomId, since: &str) -> Result {
	let events = self.left_section(room_id, since, "state").await?;

	assert_eq!(events.as_array().map(Vec::len), Some(1), "room state was served: {events}");

	Ok(())
}

/// One section of a legacy left room, which must hold the invitee's own leave.
///
/// The section's events come back for the caller to inspect.
#[implement(Accounts, params = "<'_>")]
async fn left_section(&self, room_id: &RoomId, since: &str, section: &str) -> Result<Value> {
	let reply = self.invitee_client.sync_v3(Some(since)).await?;
	let events = &reply["rooms"]["leave"][room_id.as_str()][section]["events"];
	let member = own_member(events, self.invitee);

	assert_eq!(member["content"]["membership"], "leave", "no legacy leave event: {reply}");

	Ok(events.clone())
}

/// A room the invitee joined and left, then the server, with an invite pending.
///
/// The room's frozen state holds their own departure.
#[implement(Accounts, params = "<'_>")]
async fn left_room_once_visited(&self) -> Result<OwnedRoomId> {
	let room_id = self.joined_room().await?;

	self.invitee_client.act(&room_id, "leave").await?;
	self.abandon_with_invite(&room_id).await?;

	Ok(room_id)
}

#[implement(Accounts, params = "<'_>")]
async fn joined_room(&self) -> Result<OwnedRoomId> {
	let room_id = self.public_room().await?;

	self.invitee_client.act(&room_id, "join").await?;

	Ok(room_id)
}

#[implement(Accounts, params = "<'_>")]
async fn public_room(&self) -> Result<OwnedRoomId> {
	let body = json!({ "preset": "public_chat", "room_version": ROOM_VERSION });

	self.inviter_client.create_room(&body).await
}

#[implement(Accounts, params = "<'_>")]
async fn abandon_with_invite(&self, room_id: &RoomId) -> Result {
	self.abandon(room_id).await?;
	self.plant_invite(room_id).await
}

#[implement(Accounts, params = "<'_>")]
async fn abandon(&self, room_id: &RoomId) -> Result {
	let &Self { services, .. } = self;

	self.inviter_client.act(room_id, "leave").await?;

	let resident = services
		.state_cache
		.server_in_room(services.globals.server_name(), room_id)
		.await;

	assert!(!resident, "every local user left, so the server is not in the room");
	assert!(services.metadata.exists(room_id).await, "a left room keeps its events");

	Ok(())
}

/// A room the server left whose state never held the invitee, an invite pending.
///
/// The invitee's sync position from inside the room's life comes back with it.
#[implement(Accounts, params = "<'_>")]
async fn left_room(&self) -> Result<LeftRoom> {
	let room_id = self.public_room().await?;
	let position = self.position().await?;

	self.abandon_with_invite(&room_id).await?;

	Ok((room_id, position))
}

#[implement(Accounts, params = "<'_>")]
async fn position(&self) -> Result<String> {
	let sync = self.invitee_client.sync_v3(None).await?;

	field(&sync, "next_batch").map(ToOwned::to_owned)
}

/// A rescission that a client further back reads as the same lone leave.
///
/// Whatever the room did after that position, the state holds no departure of
/// the invitee's to send that client.
#[implement(Accounts, params = "<'_>")]
async fn rescission_stands_alone(
	&self,
	(room_id, position): LeftRoom,
	connection: &str,
) -> Result {
	self.rescission_reaches_sync(&room_id, connection)
		.await?;

	self.lone_leave(&room_id, &position).await
}

/// A public room the server left while the invitee's invite stood in its state.
///
/// The invitee's sync position from before the invite comes back with it.
#[implement(Accounts, params = "<'_>")]
async fn left_room_with_invite(&self) -> Result<LeftRoom> {
	let room_id = self.public_room().await?;
	let position = self.position().await?;

	self.inviter_client
		.act_on(&room_id, "invite", self.invitee)
		.await?;

	self.abandon(&room_id).await?;

	Ok((room_id, position))
}

/// A ban the room recorded reads as a ban, whatever its row's count says.
///
/// A server can leave with no event after the ban. A row can sit at a count no
/// event holds, as a migrated or forcibly replayed one does, with the server
/// gone or still there, and a client can be past the ban when it does.
#[implement(Accounts, params = "<'_>")]
async fn recorded_ban_stays_a_ban(&self) -> Result {
	let &Self { inviter, invitee, .. } = self;

	self.ban_survives_row_moves("ban-last-event", &[inviter])
		.await?;

	self.ban_survives_row_moves("ban-non-event-count", &[inviter, invitee])
		.await?;

	self.delivered_ban_survives_row_moves("ban-resident", &[invitee])
		.await?;

	self.delivered_ban_survives_row_moves("ban-delivered", &[inviter, invitee])
		.await
}

/// A client yet to see a ban reads it, after these users' rows have moved.
///
/// The inviter's row takes the server out of the room with no event, and the
/// invitee's puts the ban's row at a count no event holds.
#[implement(Accounts, params = "<'_>")]
async fn ban_survives_row_moves(&self, connection: &str, users: &[&UserId]) -> Result {
	let request = list(connection);
	let (room_id, joined, position) = self.ban_after_join(&request).await?;

	self.move_left_rows(&room_id, users).await?;
	self.ban_in_v5(&room_id, &joined, &request)
		.await?;

	self.ban_in_v3(&room_id, &position, "timeline")
		.await
}

/// A client past a ban still reads it, after these users' rows have moved.
///
/// Nothing a read can see separates this from an invite rescinded over a frozen
/// ban, so that rescission reads as a ban too.
#[implement(Accounts, params = "<'_>")]
async fn delivered_ban_survives_row_moves(&self, connection: &str, users: &[&UserId]) -> Result {
	let request = list(connection);
	let (room_id, joined, _) = self.ban_after_join(&request).await?;
	let delivered = self
		.invitee_client
		.sync(Some(&joined), &request)
		.await?;

	let position = self.position().await?;

	self.move_left_rows(&room_id, users).await?;
	self.ban_in_v5(&room_id, &delivered, &request)
		.await?;

	self.ban_in_v3(&room_id, &position, "state").await
}

/// A room the invitee joined and was banned from, the inviter still in it.
///
/// A sliding sync reply and a legacy position from before the ban come back.
#[implement(Accounts, params = "<'_>")]
async fn ban_after_join(&self, request: &Value) -> Result<(OwnedRoomId, Value, String)> {
	let room_id = self.joined_room().await?;
	let joined = self.invitee_client.sync(None, request).await?;
	let position = self.position().await?;

	self.inviter_client
		.act_on(&room_id, "ban", self.invitee)
		.await?;

	Ok((room_id, joined, position))
}

/// Move these users' left rows to fresh counts with no event behind them.
///
/// A count no event holds is what a migrated or forcibly replayed row has.
#[implement(Accounts, params = "<'_>")]
async fn move_left_rows(&self, room_id: &RoomId, users: &[&UserId]) -> Result {
	let &Self { services, .. } = self;

	for user_id in users {
		let count = services.globals.next_count();
		let update = membership_update(room_id, user_id, MembershipState::Leave, *count);

		services
			.state_cache
			.update_membership(update)
			.await?;
	}

	Ok(())
}

#[implement(Accounts, params = "<'_>")]
async fn ban_in_v5(&self, room_id: &RoomId, previous: &Value, request: &Value) -> Result {
	let reply = self
		.invitee_client
		.sync(Some(previous), request)
		.await?;

	let required_state = &reply["rooms"][room_id.as_str()]["required_state"];

	assert_ban(required_state, self.invitee, &reply);

	Ok(())
}

#[implement(Accounts, params = "<'_>")]
async fn ban_in_v3(&self, room_id: &RoomId, since: &str, section: &str) -> Result {
	let reply = self.invitee_client.sync_v3(Some(since)).await?;
	let events = &reply["rooms"]["leave"][room_id.as_str()][section]["events"];

	assert_ban(events, self.invitee, &reply);

	Ok(())
}

/// An event with no snapshot after the client's position is still an event.
///
/// So the departure is a recorded one, and the room stays with the ordinary
/// loader, which serves its timeline without that snapshot.
#[implement(Accounts, params = "<'_>")]
async fn missing_snapshot_keeps_the_timeline(&self) -> Result {
	let &Self { services, invitee, .. } = self;
	let room_id = self.joined_room().await?;
	let joined = self.position().await?;

	self.invitee_client.act(&room_id, "leave").await?;

	let leave = self.member_event_id(&room_id, invitee).await?;
	let shorteventid = services.short.get_shorteventid(&leave).await?;

	services.db["shorteventid_shortstatehash"].del(shorteventid);

	self.left_section(&room_id, &joined, "timeline")
		.await
		.map(drop)
}

/// A failed read of the room's last event says nothing of the user's departure.
///
/// So the departure stays a recorded one, and the ordinary loader serves its
/// timeline, which ends before the unreadable event.
#[implement(Accounts, params = "<'_>")]
async fn unreadable_last_event_keeps_the_timeline(&self) -> Result {
	let &Self { services, inviter, .. } = self;
	let room_id = self.joined_room().await?;
	let joined = self.position().await?;

	self.invitee_client.act(&room_id, "leave").await?;
	self.abandon(&room_id).await?;

	let last = self.member_event_id(&room_id, inviter).await?;
	let pdu_id = services.timeline.get_pdu_id(&last).await?;

	services.db["pduid_pdu"].insert(pdu_id.as_ref(), b"{");

	self.left_section(&room_id, &joined, "timeline")
		.await
		.map(drop)
}

#[implement(Accounts, params = "<'_>")]
async fn member_event_id(&self, room_id: &RoomId, user_id: &UserId) -> Result<OwnedEventId> {
	let event = self
		.services
		.state_accessor
		.room_state_get(room_id, &StateEventType::RoomMember, user_id.as_str())
		.await?;

	Ok(event.event_id)
}

/// In a left room a kick from anyone but the inviter changes nothing.
///
/// The server cannot judge that sender's authority without current state, so
/// the invite stands and the kick is not stored.
#[implement(Accounts, params = "<'_>")]
async fn other_sender_is_ignored(&self, bystander: &UserId) -> Result {
	let &Self { services, .. } = self;
	let (room_id, _) = self.left_room().await?;
	let (event_id, handled) = self.push_kick(&room_id, bystander).await?;
	let membership = self.membership(&room_id).await;
	let stored = services.timeline.get_pdu(&event_id).await;

	assert!(!handled, "a dropped kick reported a timeline position");
	assert_eq!(membership, Some(MembershipState::Invite), "another sender cleared the invite");
	assert!(stored.is_err(), "a dropped kick was stored");

	Ok(())
}

/// A kick held only as an outlier was never applied, so it can still rescind.
///
/// Ingestion stores an outlier ahead of timeline processing, which can fail
/// and leave the invite for a retry to clear after the server has left.
#[implement(Accounts, params = "<'_>")]
async fn held_kick_still_rescinds(&self) -> Result {
	let &Self { services, inviter, invitee, .. } = self;
	let room_id = self.public_room().await?;
	let content = RoomMemberEventContent::new(MembershipState::Leave);
	let builder = PduBuilder::state(invitee.to_string(), &content);
	let (event, kick) = sign(services, inviter, &room_id, builder).await?;
	let kick = into_outgoing_federation(kick, &ROOM_VERSION);
	let event_id = event.event_id;

	self.abandon_with_invite(&room_id).await?;

	services
		.event_handler
		.handle_incoming_pdu(
			services.globals.server_name(),
			&room_id,
			&event_id,
			kick.clone(),
			false,
		)
		.await?;

	let held = services
		.timeline
		.outlier_pdu_exists(&event_id)
		.await;

	self.deliver(&room_id, &event_id, kick).await?;

	let membership = self.membership(&room_id).await;

	assert!(held.is_ok(), "ingestion did not hold the kick as an outlier");
	assert_eq!(membership, Some(MembershipState::Leave), "an outlier kept the invite");

	Ok(())
}

fn assert_ban(events: &Value, invitee: &UserId, reply: &Value) {
	let member = own_member(events, invitee);

	assert_eq!(member["content"]["membership"], "ban", "ban lost: {reply}");
}

fn list(connection: &str) -> Value {
	json!({
		"conn_id": connection,
		"lists": { "all": {
			"ranges": [[0, 9]], "timeline_limit": 1,
			"required_state": [["m.room.member", "$ME"]]
		} },
	})
}

#[implement(Client, params = "<'_>")]
async fn sync_v3(&self, since: Option<&str>) -> Result<Value> {
	let since = since
		.map(|since| format!("&since={since}"))
		.unwrap_or_default();

	let response = self
		.services
		.client
		.clients
		.default
		.get(self.url(&format!("sync?timeout=0{since}")))
		.bearer_auth(self.token)
		.send()
		.await?
		.error_for_status()?
		.json()
		.await?;

	Ok(response)
}

/// A sender's kick of the invitee, signed by the sender's server.
///
/// The sender is local, so that server is this one and the kick verifies
/// against its own key, as a remote sender's would against the remote's.
fn sign_kick(
	services: &Services,
	room_id: &RoomId,
	sender: &UserId,
	invitee: &UserId,
	invite_event: Option<&EventId>,
) -> Result<(OwnedEventId, CanonicalJsonObject)> {
	let kick = json!({
		"type": "m.room.member",
		"state_key": invitee,
		"sender": sender,
		"room_id": room_id,
		"content": { "membership": "leave", "reason": "withdrawn" },
		"origin_server_ts": MilliSecondsSinceUnixEpoch::now(),
		"depth": 2,
		"prev_events": [],
		"auth_events": invite_event.as_slice(),
	});

	let mut kick: CanonicalJsonObject = serde_json::from_value(kick)?; // signer takes &mut
	let event_id = services
		.server_keys
		.gen_id_hash_and_sign_event(&mut kick, &ROOM_VERSION)?;

	Ok((event_id, into_outgoing_federation(kick, &ROOM_VERSION)))
}

/// The invitee's own membership event among a section's events.
///
/// An absent event reads as null, so the caller's assertion reports it.
fn own_member<'a>(events: &'a Value, invitee: &UserId) -> &'a Value {
	find_member_event(events, invitee).unwrap_or(&Value::Null)
}
