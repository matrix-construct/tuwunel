#![cfg(test)]

use std::sync::Arc;

use serde_json::{Value, json};
use tuwunel_core::{
	Result, err, implement,
	ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId},
	utils::BoolExt,
};
use tuwunel_service::Services;

use self::{
	account::account,
	client::{Client, field},
	device_lists::{device_list_changed, listed_on_both},
	fixture::boot,
	require::require,
	sync::joined_room,
};

mod client;
mod fixture;
mod sliding_sync;

#[path = "client/account.rs"]
mod account;

#[path = "client/device_lists.rs"]
mod device_lists;

#[path = "client/e2ee.rs"]
mod e2ee;

#[path = "client/event.rs"]
mod event;

#[path = "client/get.rs"]
mod get;

#[path = "client/membership.rs"]
mod membership;

#[path = "client/put.rs"]
mod put;

#[path = "client/require.rs"]
mod require;

#[path = "client/sync.rs"]
mod sync;

/// A `/sync` next-batch token and a sliding sync position, taken together.
///
/// `open_syncs` returns both before a case acts, and `resume_syncs` resumes
/// both surfaces from them afterwards.
type Tokens = (String, String);

/// An account to register: its localpart and the token its device holds.
///
/// `restorable` registers a case's user and peer from one each.
type Account<'a> = (&'a str, &'a str);

/// A user and a peer sharing an encrypted room, with a saved state of the room
/// that holds the user's join.
///
/// A case restores that join by forcing the room back onto the saved state.
struct Restorable<'a> {
	user_id: OwnedUserId,
	user: Client<'a>,
	peer_id: OwnedUserId,
	peer: Client<'a>,
	room: OwnedRoomId,
	joined_state: u64,
}

const READER_TOKEN: &str = "sync-join-detection-reader-token";
const JOINER_TOKEN: &str = "sync-join-detection-joiner-token";
const INVITER_TOKEN: &str = "sync-join-detection-own-inviter-token";
const INVITEE_TOKEN: &str = "sync-join-detection-own-invitee-token";
const REJOINER_TOKEN: &str = "sync-join-detection-rejoiner-token";
const REJOIN_PEER_TOKEN: &str = "sync-join-detection-rejoin-peer-token";
const FORCED_RENAMER_TOKEN: &str = "sync-join-detection-forced-renamer-token";
const FORCED_RENAMER_PEER_TOKEN: &str = "sync-join-detection-forced-renamer-peer-token";
const LEAVE_RENAMER_TOKEN: &str = "sync-join-detection-leave-renamer-token";
const LEAVE_RENAMER_PEER_TOKEN: &str = "sync-join-detection-leave-renamer-peer-token";
const WINDOW_RENAMER_TOKEN: &str = "sync-join-detection-window-renamer-token";
const WINDOW_RENAMER_PEER_TOKEN: &str = "sync-join-detection-window-renamer-peer-token";
const RESTORED_TOKEN: &str = "sync-join-detection-restored-token";
const RESTORED_PEER_TOKEN: &str = "sync-join-detection-restored-peer-token";

/// Checks what `/sync` and the sliding sync e2ee extension count as a join.
///
/// The cases share one server.
#[test]
fn joins_are_read_from_membership() -> Result { boot("sync-join-detection", [], exercise) }

async fn exercise(services: &Services, base: &str) -> Result {
	joiners_sharing_another_encrypted_room(services, base).await?;
	own_join(services, base).await?;
	own_rejoin(services, base).await?;
	forced_rename(services, base).await?;
	forced_rename_at_leave(services, base).await?;
	forced_rename_in_window(services, base).await?;
	forced_rejoin_in_window(services, base).await
}

/// An invitee joins two encrypted rooms between two of a reader's syncs.
///
/// Once both joins land, each room's join leaves the reader sharing the other
/// encrypted room with the joiner, and that must not hide the join. Both
/// surfaces list the joiner in `device_lists.changed`, so the reader's client
/// fetches any device the joiner made while only invited.
async fn joiners_sharing_another_encrypted_room(services: &Services, base: &str) -> Result {
	let (_, reader) = account(services, base, "joinersreader", READER_TOKEN).await?;
	let (joiner_id, joiner) = account(services, base, "joinersjoiner", JOINER_TOKEN).await?;
	let first = reader.create_encrypted_room().await?;
	let second = reader.create_encrypted_room().await?;

	reader
		.act_on(&first, "invite", &joiner_id)
		.await?;

	reader
		.act_on(&second, "invite", &joiner_id)
		.await?;

	let tokens = reader.open_syncs("").await?;

	joiner.act(&first, "join").await?;
	joiner.act(&second, "join").await?;

	let (sync, sliding) = reader.resume_syncs(&tokens, "").await?;
	let (listed, e2ee_listed) = listed_on_both(&sync, &sliding, &joiner_id);

	require(listed, "/sync withheld a joiner who shares another encrypted room")?;
	require(e2ee_listed, "sliding sync withheld a joiner who shares another encrypted room")
}

/// A user joins an encrypted room between two of their own syncs.
///
/// The join is the user's own membership transition, so `/sync` sends the
/// room's full state and both surfaces list the room's other member in
/// `device_lists.changed`.
async fn own_join(services: &Services, base: &str) -> Result {
	let (inviter_id, inviter) = account(services, base, "owninviter", INVITER_TOKEN).await?;
	let (invitee_id, invitee) = account(services, base, "owninvitee", INVITEE_TOKEN).await?;
	let room = inviter.create_encrypted_room().await?;

	inviter
		.act_on(&room, "invite", &invitee_id)
		.await?;

	let tokens = invitee.open_syncs("").await?;

	invitee.act(&room, "join").await?;

	let (sync, sliding) = invitee.resume_syncs(&tokens, "").await?;
	let (listed, e2ee_listed) = listed_on_both(&sync, &sliding, &inviter_id);
	let joined = joined_room(&sync, &room)?;

	require(resends_state(joined), "/sync sent a delta for the user's own join")?;
	require(listed, "/sync withheld the room's member after the user's own join")?;
	require(e2ee_listed, "sliding sync withheld the room's member after the user's own join")
}

/// A user leaves and rejoins between two of their own syncs.
///
/// The user's filter keeps one timeline event, so the leave never reaches the
/// timeline and the membership at the since token is a join. The rejoin is
/// still the user's own fresh join: `/sync` sends the room's full state and
/// both surfaces list the room's other member in `device_lists.changed`.
async fn own_rejoin(services: &Services, base: &str) -> Result {
	let (rejoiner_id, rejoiner) = account(services, base, "rejoiner", REJOINER_TOKEN).await?;
	let (peer_id, peer) = account(services, base, "rejoinpeer", REJOIN_PEER_TOKEN).await?;
	let room = peer
		.encrypted_room_with(&rejoiner, &rejoiner_id)
		.await?;

	let filter = json!({ "room": { "timeline": { "limit": 1 } } });
	let created = rejoiner
		.post(&format!("user/{rejoiner_id}/filter"), &filter)
		.await?;

	let query = format!("&filter={}", field(&created, "filter_id")?);
	let tokens = rejoiner.open_syncs(&query).await?;

	rejoiner.act(&room, "leave").await?;
	peer.admit(&room, &rejoiner, &rejoiner_id).await?;

	peer.send_text(&room, "first", "one").await?;
	peer.send_text(&room, "second", "two").await?;

	let (sync, sliding) = rejoiner.resume_syncs(&tokens, &query).await?;
	let (listed, e2ee_listed) = listed_on_both(&sync, &sliding, &peer_id);
	let joined = joined_room(&sync, &room)?;
	let timeline = &joined["timeline"];

	require(timeline["limited"] == true, "the filter left the timeline whole")?;
	require(carries(timeline, "m.room.member").is_false(), "the filter showed the rejoin")?;
	require(resends_state(joined), "/sync sent a delta for a rejoin behind the timeline")?;
	require(listed, "/sync withheld the room's member after a rejoin")?;
	require(e2ee_listed, "sliding sync withheld the room's member after a rejoin")
}

/// A forced state replay restores a user's join after their leave and a peer's
/// message, and the user renames before syncing again.
///
/// The since token rests on the peer's message, taken while the user was gone.
async fn forced_rename(services: &Services, base: &str) -> Result {
	let user = ("forcedrenamer", FORCED_RENAMER_TOKEN);
	let peer = ("forcedrenamerpeer", FORCED_RENAMER_PEER_TOKEN);
	let case = restorable(services, base, user, peer).await?;

	case.user.act(&case.room, "leave").await?;
	case.peer
		.send_text(&case.room, "settle", "left")
		.await?;

	case.rename_after_restore().await
}

/// A forced state replay restores a user's join after their leave, and the user
/// renames before syncing again.
///
/// The since token rests on the leave itself.
async fn forced_rename_at_leave(services: &Services, base: &str) -> Result {
	let user = ("leaverenamer", LEAVE_RENAMER_TOKEN);
	let peer = ("leaverenamerpeer", LEAVE_RENAMER_PEER_TOKEN);
	let case = restorable(services, base, user, peer).await?;

	case.user.act(&case.room, "leave").await?;

	case.rename_after_restore().await
}

/// A user leaves after a sync, a forced state replay restores their join, and
/// the user renames before syncing again.
///
/// The user was joined at the since token and left inside the window.
async fn forced_rename_in_window(services: &Services, base: &str) -> Result {
	let user = ("windowrenamer", WINDOW_RENAMER_TOKEN);
	let peer = ("windowrenamerpeer", WINDOW_RENAMER_PEER_TOKEN);
	let case = restorable(services, base, user, peer).await?;
	let tokens = case.user.open_syncs("").await?;

	case.user.act(&case.room, "leave").await?;
	case.rename_after_restore_since(&tokens).await
}

/// A user leaves after a sync, and a forced state replay restores their join.
///
/// The room returning is the user's fresh join: `/sync` sends full state and
/// lists the room's other member. The sliding sync extension is left out: it
/// skips a room whose state snapshot predates the connection's position, as a
/// restored one does.
async fn forced_rejoin_in_window(services: &Services, base: &str) -> Result {
	let user = ("restored", RESTORED_TOKEN);
	let peer = ("restoredpeer", RESTORED_PEER_TOKEN);
	let Restorable {
		user, peer_id, peer, room, joined_state, ..
	} = restorable(services, base, user, peer).await?;

	let tokens = user.open_syncs("").await?;

	user.act(&room, "leave").await?;
	force_state(services, &room, joined_state).await?;
	peer.send_text(&room, "restored", "back").await?;

	let (sync, _) = user.resume_syncs(&tokens, "").await?;
	let listed = device_list_changed(&sync["device_lists"], &peer_id);
	let joined = joined_room(&sync, &room)?;

	require(resends_state(joined), "/sync sent a delta for a join a forced state restored")?;
	require(
		listed,
		"/sync withheld the room's member after a forced state restored the join",
	)
}

/// Register a user and a peer, and have the peer admit the user to a new
/// encrypted room whose state is then saved.
///
/// A message after the join anchors the saved state, so restoring it restores
/// the join.
async fn restorable<'a>(
	services: &'a Services,
	base: &'a str,
	(localpart, token): Account<'a>,
	(peer_localpart, peer_token): Account<'a>,
) -> Result<Restorable<'a>> {
	let (user_id, user) = account(services, base, localpart, token).await?;
	let (peer_id, peer) = account(services, base, peer_localpart, peer_token).await?;
	let room = peer.encrypted_room_with(&user, &user_id).await?;
	let anchor = peer.send_text(&room, "anchor", "joined").await?;
	let joined_state = services.state.pdu_shortstatehash(&anchor).await?;

	Ok(Restorable {
		user_id,
		user,
		peer_id,
		peer,
		room,
		joined_state,
	})
}

/// Take the since tokens, then restore the user's join and rename them.
///
/// A case whose since token must precede its own steps takes the tokens itself
/// and calls `rename_after_restore_since`.
#[implement(Restorable, params = "<'_>")]
async fn rename_after_restore(&self) -> Result {
	let tokens = self.user.open_syncs("").await?;

	self.rename_after_restore_since(&tokens).await
}

/// Restore the room's state holding the user's join, rename the user, and resume
/// both syncs from the tokens.
///
/// The room returning is the user's fresh join: `/sync` sends full state and
/// both surfaces list the peer.
#[implement(Restorable, params = "<'_>")]
async fn rename_after_restore_since(&self, tokens: &Tokens) -> Result {
	let displayname = json!({ "displayname": "restored" });

	force_state(self.user.services, &self.room, self.joined_state).await?;

	self.user
		.put(&format!("profile/{}/displayname", self.user_id), &displayname)
		.await?;

	let (sync, sliding) = self.user.resume_syncs(tokens, "").await?;
	let (listed, e2ee_listed) = listed_on_both(&sync, &sliding, &self.peer_id);
	let joined = joined_room(&sync, &self.room)?;

	require(carries(&joined["timeline"], "m.room.member"), "the rename missed the timeline")?;
	require(resends_state(joined), "/sync sent a delta for a rename after a restored join")?;
	require(listed, "/sync withheld the room's member after a restored join")?;
	require(e2ee_listed, "sliding sync withheld the room's member after a restored join")
}

/// Take a `/sync` token and open a sliding sync, so a case can resume both.
///
/// The query extends the `/sync` request, which lets a case apply a filter.
#[implement(Client, params = "<'_>")]
async fn open_syncs(&self, query: &str) -> Result<Tokens> {
	let sync = self
		.get(&format!("sync?timeout=0{query}"))
		.await?;

	let sliding = self.sync_e2ee(None, &json!([])).await?;
	let since = field(&sync, "next_batch")?;
	let pos = field(&sliding, "pos")?;

	Ok((since.to_owned(), pos.to_owned()))
}

/// Resume both syncs from their tokens and return the two responses.
///
/// The `/sync` response comes first and the sliding sync response second; the
/// query extends the `/sync` request as it did when the tokens were taken. A
/// sliding sync answer without the e2ee extension fails here, so a case's
/// negative check on it cannot pass for want of the extension.
#[implement(Client, params = "<'_>")]
async fn resume_syncs(&self, tokens: &Tokens, query: &str) -> Result<(Value, Value)> {
	let (since, pos) = tokens;
	let sync = self
		.get(&format!("sync?timeout=0&since={since}{query}"))
		.await?;

	let sliding = self.sync_e2ee(Some(pos), &json!([])).await?;
	let extended = sliding["extensions"]["e2ee"].is_object();

	require(extended, "sliding sync answered without the e2ee extension")?;

	Ok((sync, sliding))
}

/// Whether a joined room's `/sync` state section resends the room's full state.
///
/// Only full state carries the create event, which no later state delta holds.
fn resends_state(joined: &Value) -> bool { carries(&joined["state"], "m.room.create") }

fn carries(section: &Value, event_type: &str) -> bool {
	section["events"]
		.as_array()
		.into_iter()
		.flatten()
		.any(|event| event["type"] == event_type)
}

/// Create an encrypted invite-only room and admit one member to it.
///
/// The member joins through an invite, as every join in these cases does.
#[implement(Client, params = "<'_>")]
async fn encrypted_room_with(
	&self,
	member: &Client<'_>,
	member_id: &UserId,
) -> Result<OwnedRoomId> {
	let room = self.create_encrypted_room().await?;

	self.admit(&room, member, member_id).await?;

	Ok(room)
}

/// Force a room back onto an earlier state snapshot, as a state reset does.
///
/// The whole snapshot replays as added state, so each member event in it
/// records its membership again under a fresh joined count.
async fn force_state(services: &Services, room_id: &RoomId, shortstatehash: u64) -> Result {
	let full_state = services
		.state_compressor
		.load_shortstatehash_info(shortstatehash)
		.await?
		.last()
		.map(|layer| Arc::clone(&layer.full_state))
		.ok_or_else(|| err!("the snapshot loaded no layers"))?;

	let state_lock = services.state.mutex.lock(room_id).await;

	services
		.state
		.force_state(room_id, shortstatehash, full_state, Arc::default(), &state_lock)
		.await
}
