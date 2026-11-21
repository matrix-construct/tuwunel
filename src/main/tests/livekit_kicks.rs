#![cfg(test)]
#![cfg(feature = "livekit")]

//! Tests that leaving a room or a ban kicks a user's LiveKit participants
//! (MSC4195), on a booted server.
//!
//! A stand-in LiveKit creates rooms and removes participants as each case
//! sets it. A member who leaves the room or is banned from it loses every
//! participant it was issued a token for there, while a member with no token,
//! or one joined again, loses none, and a token whose user departs while it is
//! issued is refused.

use std::{collections::BTreeSet, net::TcpListener, time::Duration};

use axum::{
	Json, Router,
	body::to_bytes,
	extract::{Request, State},
	http::{StatusCode, request::Parts},
	response::{IntoResponse, Response as AxumResponse},
};
use axum_server::from_tcp;
use futures::{
	StreamExt, TryStreamExt,
	future::{join, ready},
	stream::try_unfold,
};
use reqwest::Response;
use serde_json::{Value, from_slice, json};
use tokio::{
	sync::{
		mpsc::{UnboundedReceiver, UnboundedSender, unbounded_channel},
		watch::{Receiver, Sender, channel},
	},
	task::JoinSet,
	time::timeout,
};
use tuwunel_core::{
	Result, err,
	http::header::AUTHORIZATION,
	implement,
	ruma::{OwnedUserId, RoomId, UserId},
	utils::time::now_secs,
};
use tuwunel_database::Interfix;
use tuwunel_service::Services;

use self::{
	client::{Client, accepted, field, poll_until, register},
	common::{API_KEY, FOLDED_SLOT, SECRET, SLOT, claims, derive, refusal},
	fixture::boot,
};

mod client;
mod fixture;

#[expect(
	dead_code,
	reason = "The request editor serves the other LiveKit tests."
)]
#[path = "livekit/common.rs"]
mod common;

#[path = "client/membership.rs"]
mod membership;

const OWNER_TOKEN: &str = "livekit-kicks-owner-token-000001";
const STAYS_TOKEN: &str = "livekit-kicks-stays-token-000001";
const PLAIN_TOKEN: &str = "livekit-kicks-plain-token-000001";
const LEAVER_TOKEN: &str = "livekit-kicks-leaver-token-00001";
const BANNED_TOKEN: &str = "livekit-kicks-banned-token-00001";
const LAST_TOKEN: &str = "livekit-kicks-last-token-0000001";
const RACER_TOKEN: &str = "livekit-kicks-racer-token-000001";

const SIDE_SLOT: &str = "m.call#side";
const MEMBER_ID: &str = "memberABC";
const LATE_MEMBER_ID: &str = "memberLATE";
const CREATE_ROOM: &str = "/twirp/livekit.RoomService/CreateRoom";
const REMOVE_PARTICIPANT: &str = "/twirp/livekit.RoomService/RemoveParticipant";

const KICK_DEADLINE: Duration = Duration::from_secs(10);

// Ample for a kick that reads one row and asks LiveKit nothing.
const GRACE: Duration = Duration::from_millis(500);

#[derive(Clone, Copy, Eq, PartialEq)]
enum Answer {
	/// The room it was asked for, or the participant removed.
	Success,

	/// A 500 with a Twirp error.
	Failure,

	/// No answer until the case sets another, which is then given.
	Hold,
}

#[derive(Clone)]
struct StandIn {
	created: UnboundedSender<()>,
	removals: UnboundedSender<Removal>,
	creation: Receiver<Answer>,
	removal: Receiver<Answer>,
}

struct Case<'a> {
	services: &'a Services,
	base: &'a str,
	owner: Client<'a>,
	room_id: &'a RoomId,
	url: &'a str,
	creation: &'a Sender<Answer>,
	removal: &'a Sender<Answer>,
}

struct Member<'a> {
	client: Client<'a>,
	user_id: OwnedUserId,
}

struct Removal {
	authorization: String,
	body: Value,
}

#[test]
fn kicks_departed_users() -> Result {
	let listener = TcpListener::bind(("127.0.0.1", 0))?;
	let url = format!("ws://{}", listener.local_addr()?);
	let options = [
		format!(r#"livekit.url="{url}""#),
		format!(r#"livekit.api_key="{API_KEY}""#),
		format!(r#"livekit.api_secret="{SECRET}""#),
	];

	boot(
		"livekit-kicks",
		options.iter().map(String::as_str),
		async |services: &Services, base: &str| exercise(services, base, listener, &url).await,
	)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn exercise(services: &Services, base: &str, listener: TcpListener, url: &str) -> Result {
	listener.set_nonblocking(true)?;

	let (created, mut creations) = unbounded_channel();
	let (removals, mut seen) = unbounded_channel();
	let (creation, creation_answers) = channel(Answer::Success);
	let (removal, removal_answers) = channel(Answer::Success);
	let state = StandIn {
		created,
		removals,
		creation: creation_answers,
		removal: removal_answers,
	};

	let app = Router::new().fallback(stand_in).with_state(state);

	let serving = from_tcp(listener)?.serve(app.into_make_service());
	let mut tasks = JoinSet::new(); // The set aborts the stand-in when the exercise exits.

	tasks.spawn_on(serving, services.server.runtime());

	register(services, "lkowner", OWNER_TOKEN).await?;

	let owner = Client { services, base, token: OWNER_TOKEN };
	let room_id = owner.create_room(&json!({})).await?;
	let case = Case {
		services,
		base,
		owner,
		room_id: &room_id,
		url,
		creation: &creation,
		removal: &removal,
	};

	let stays = case.member("lkstays", STAYS_TOKEN).await?;
	let plain = case.member("lkplain", PLAIN_TOKEN).await?;
	let leaver = case.member("lkleaver", LEAVER_TOKEN).await?;
	let banned = case.member("lkbanned", BANNED_TOKEN).await?;
	let last = case.member("lklast", LAST_TOKEN).await?;
	let racer = case.member("lkracer", RACER_TOKEN).await?;

	for (member, slot) in [
		(&stays, SLOT),
		(&leaver, SLOT),
		(&leaver, SLOT),
		(&leaver, SIDE_SLOT),
		(&banned, SLOT),
		(&banned, SIDE_SLOT),
		(&last, SLOT),
	] {
		case.issue(member, slot, MEMBER_ID).await?;
	}

	leave_removes_issued_participants(&case, &leaver, &mut seen).await?;
	ban_removes_them_whatever_livekit_answers(&case, &banned, &mut seen).await?;
	nothing_to_kick_kicks_nothing(&case, &stays, &plain, &last, &mut seen).await?;
	departure_during_issuance_refuses_the_token(&case, &racer, &mut creations).await
}

/// A kick stops removing a member's participants once the member joins again,
/// and the member's next departure removes those left.
///
/// LiveKit is asked to remove a derived participant from its derived room,
/// with a token that may administer that room and the moment of the call as
/// the revocation cutoff, and each row stays until LiveKit has answered for
/// its participant. The member joins again and is issued tokens while that
/// answer is held, so the kick finishes that removal and asks for no other.
/// The next departure removes every participant left, the one issued during
/// the kick included, each once though one holds two tokens.
#[tracing::instrument(level = "debug", skip_all)]
async fn leave_removes_issued_participants(
	case: &Case<'_>,
	leaver: &Member<'_>,
	seen: &mut UnboundedReceiver<Removal>,
) -> Result {
	let user_id = &leaver.user_id;
	let since = now_secs();
	let call = case.participant(user_id, SLOT, MEMBER_ID);
	let side = case.participant(user_id, SIDE_SLOT, MEMBER_ID);

	case.removal.send_replace(Answer::Hold);
	leaver.client.act(case.room_id, "leave").await?;

	let first = removed(&next_removal(seen).await?, since)?;

	assert!(first == call || first == side, "LiveKit was asked to remove {first:?}");

	let kept = case.rows(user_id, SLOT, MEMBER_ID).await == 2
		&& case.rows(user_id, SIDE_SLOT, MEMBER_ID).await == 1;

	assert!(kept, "a row was deleted before LiveKit answered for its participant");

	case.owner
		.admit(case.room_id, &leaver.client, user_id)
		.await?;

	case.issue(leaver, SLOT, MEMBER_ID).await?;
	case.issue(leaver, SLOT, LATE_MEMBER_ID).await?;
	case.removal.send_replace(Answer::Success);

	// Only the row read for the answered participant goes.
	let (slot, left) = if first == side { (SIDE_SLOT, 0) } else { (SLOT, 2) };

	assert!(case.settles(user_id, slot, MEMBER_ID, left).await, "the answered row was kept");
	assert!(
		timeout(GRACE, seen.recv()).await.is_err(),
		"a member joined again lost a participant"
	);

	assert_eq!(case.rows(user_id, SLOT, LATE_MEMBER_ID).await, 1, "a later token lost its row");

	leaver.client.act(case.room_id, "leave").await?;

	let expected = [call, case.participant(user_id, SLOT, LATE_MEMBER_ID)]
		.into_iter()
		.chain(first.ne(&side).then_some(side))
		.collect();

	removals(seen, expected, since).await?;

	let settled = case.settles(user_id, SLOT, MEMBER_ID, 0).await
		&& case
			.settles(user_id, SIDE_SLOT, MEMBER_ID, 0)
			.await
		&& case
			.settles(user_id, SLOT, LATE_MEMBER_ID, 0)
			.await;

	assert!(settled, "a removed participant's row was kept");
	assert!(
		timeout(GRACE, seen.recv()).await.is_err(),
		"a participant issued two tokens was removed twice"
	);

	Ok(())
}

/// LiveKit is asked to remove a banned member's participants in both slots.
///
/// Their rows are deleted even when LiveKit fails the removals, since a kick
/// is never retried.
#[tracing::instrument(level = "debug", skip_all)]
async fn ban_removes_them_whatever_livekit_answers(
	case: &Case<'_>,
	banned: &Member<'_>,
	seen: &mut UnboundedReceiver<Removal>,
) -> Result {
	let user_id = &banned.user_id;
	let since = now_secs();

	case.removal.send_replace(Answer::Failure);
	case.owner
		.act_on(case.room_id, "ban", user_id)
		.await?;

	let expected = BTreeSet::from([
		case.participant(user_id, SLOT, MEMBER_ID),
		case.participant(user_id, SIDE_SLOT, MEMBER_ID),
	]);

	removals(seen, expected, since).await?;

	let settled = case.settles(user_id, SLOT, MEMBER_ID, 0).await
		&& case
			.settles(user_id, SIDE_SLOT, MEMBER_ID, 0)
			.await;

	assert!(settled, "a failed removal kept its row");

	Ok(())
}

/// A departure with nothing to kick queues nothing, and a kick finding its
/// user joined again removes nothing.
///
/// A member with no token leaves, and a departure is queued for a member who
/// is still joined. Neither reaches LiveKit or deletes a row, while the member
/// who leaves after them is kicked.
#[tracing::instrument(level = "debug", skip_all)]
async fn nothing_to_kick_kicks_nothing(
	case: &Case<'_>,
	stays: &Member<'_>,
	plain: &Member<'_>,
	last: &Member<'_>,
	seen: &mut UnboundedReceiver<Removal>,
) -> Result {
	let livekit = &case.services.livekit;
	let since = now_secs();

	case.removal.send_replace(Answer::Success);
	plain.client.act(case.room_id, "leave").await?;

	assert!(
		!livekit
			.on_departure(case.room_id, &plain.user_id)
			.await,
		"a departure without tokens queued a kick"
	);

	assert!(
		livekit
			.on_departure(case.room_id, &stays.user_id)
			.await,
		"a token holder's departure queued no kick"
	);

	last.client.act(case.room_id, "leave").await?;

	let removal = next_removal(seen).await?;

	assert_eq!(removed(&removal, since)?, case.participant(&last.user_id, SLOT, MEMBER_ID));
	assert!(
		case.settles(&last.user_id, SLOT, MEMBER_ID, 0)
			.await,
		"a removed participant's row was kept"
	);

	// The kicks queued ahead of the last one have had their time.
	assert!(
		timeout(GRACE, seen.recv()).await.is_err(),
		"a member with nothing to kick lost a participant"
	);

	assert_eq!(
		case.rows(&stays.user_id, SLOT, MEMBER_ID).await,
		1,
		"a joined member's row was deleted"
	);

	Ok(())
}

/// A token whose user departs while LiveKit creates its room is refused.
///
/// The departure finds no row to kick yet, so issuance reads the membership
/// again once its row is written, deletes the row and answers the same 403 as
/// any refusal.
#[tracing::instrument(level = "debug", skip_all)]
async fn departure_during_issuance_refuses_the_token(
	case: &Case<'_>,
	racer: &Member<'_>,
	creations: &mut UnboundedReceiver<()>,
) -> Result {
	while creations.try_recv().is_ok() {}

	case.creation.send_replace(Answer::Hold);

	let request = case.request(racer, SLOT, MEMBER_ID);
	let (response, departed) = join(request, leave_during_creation(case, racer, creations)).await;

	departed?;
	assert_eq!(refusal(response?).await?, (403, "M_FORBIDDEN".into()));
	assert_eq!(
		case.rows(&racer.user_id, SLOT, MEMBER_ID).await,
		0,
		"a refused token kept its row"
	);

	Ok(())
}

/// Has a member leave the room while LiveKit holds its answer to CreateRoom,
/// then lets LiveKit answer.
///
/// Waiting for the CreateRoom request puts the departure after issuance has
/// checked the membership and before it writes its row.
#[tracing::instrument(level = "debug", skip_all)]
async fn leave_during_creation(
	case: &Case<'_>,
	racer: &Member<'_>,
	creations: &mut UnboundedReceiver<()>,
) -> Result {
	timeout(KICK_DEADLINE, creations.recv())
		.await
		.ok()
		.flatten()
		.ok_or_else(|| err!("LiveKit was asked to create no room"))?;

	racer.client.act(case.room_id, "leave").await?;
	case.creation.send_replace(Answer::Success);

	Ok(())
}

/// Registers a user and joins it to the room through the owner's invite.
///
/// The member ends up joined through the requests a real client sends.
#[implement(Case, generics = "<'a>", params = "<'a>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn member(&self, localpart: &str, token: &'a str) -> Result<Member<'a>> {
	let user_id = register(self.services, localpart, token).await?;
	let client = Client {
		services: self.services,
		base: self.base,
		token,
	};

	self.owner
		.admit(self.room_id, &client, &user_id)
		.await?;

	Ok(Member { client, user_id })
}

#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip(self, member))]
async fn issue(&self, member: &Member<'_>, slot: &str, member_id: &str) -> Result {
	let response = self.request(member, slot, member_id).await?;
	let reply = accepted(response).await?;

	field(&reply, "jwt").map(drop)
}

#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip(self, member))]
async fn request(&self, member: &Member<'_>, slot: &str, member_id: &str) -> Result<Response> {
	let body = json!({
		"url": self.url,
		"room_id": self.room_id,
		"slot_id": slot,
		"member_id": member_id,
	});

	let url = member
		.client
		.client_url("v1/", "rtc/livekit/get_token");

	member.client.post_url(&url, &body).await
}

/// The LiveKit room and identity of a member's participant in a slot.
///
/// Both are derived here by MSC4195's construction, apart from the server's
/// own code.
#[implement(Case, params = "<'_>")]
fn participant(&self, user_id: &UserId, slot: &str, member_id: &str) -> (String, String) {
	// The unstable call slot shares the room of MSC4196's spelling.
	let slot = if slot == SLOT { FOLDED_SLOT } else { slot };

	(derive(self.room_id.as_str(), slot), derive(user_id.as_str(), member_id))
}

/// Counts the rows the server keeps for a member's participant in a slot, one
/// per token.
///
/// The prefix is derived here by MSC4195's construction, so rows under any
/// other room or identity are not counted.
#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "trace", skip_all)]
async fn rows(&self, user_id: &UserId, slot: &str, member_id: &str) -> usize {
	let (room, identity) = self.participant(user_id, slot, member_id);
	let room_id = self.room_id.as_str();
	let prefix = (room_id, user_id.as_str(), room.as_str(), identity.as_str(), Interfix);

	self.services.db["roomuserid_lkparticipant"]
		.keys_prefix_raw(&prefix)
		.count()
		.await
}

/// Polls until a member's rows for a participant come to `count`, and says
/// whether they did before the deadline.
///
/// A kick deletes each row only once LiveKit has answered, so the rows are
/// polled rather than counted once.
#[implement(Case, params = "<'_>")]
#[tracing::instrument(level = "debug", skip_all)]
async fn settles(&self, user_id: &UserId, slot: &str, member_id: &str, count: usize) -> bool {
	poll_until(KICK_DEADLINE, async || self.rows(user_id, slot, member_id).await == count).await
}

/// The stand-in LiveKit: creates rooms and removes participants as the case
/// set, recording each request it recognizes first.
///
/// A held answer waits for the case to set another, and gives that one.
#[tracing::instrument(level = "debug", skip_all)]
async fn stand_in(State(stand_in): State<StandIn>, request: Request) -> AxumResponse {
	let (parts, body) = request.into_parts();
	let bytes = to_bytes(body, usize::MAX)
		.await
		.unwrap_or_default();

	let body: Value = from_slice(&bytes).unwrap_or_default();
	let path = parts.uri.path();
	let success = match path {
		| CREATE_ROOM => json!({ "name": body["name"] }),
		| _ => json!({}),
	};

	let answer = match path {
		| CREATE_ROOM => {
			stand_in.created.send(()).ok();
			stand_in.creation
		},
		| REMOVE_PARTICIPANT => {
			record_removal(&stand_in.removals, &parts, body);
			stand_in.removal
		},
		| _ => return StatusCode::NOT_FOUND.into_response(),
	};

	let answer = answer
		.clone()
		.wait_for(|answer| *answer != Answer::Hold)
		.await
		.map_or(Answer::Failure, |answer| *answer);

	match answer {
		| Answer::Success => Json(success).into_response(),
		| Answer::Failure | Answer::Hold => {
			let error = json!({ "code": "internal", "msg": "stand-in failure" });

			(StatusCode::INTERNAL_SERVER_ERROR, Json(error)).into_response()
		},
	}
}

// Hands a RemoveParticipant request to the case, before it is answered.
fn record_removal(removals: &UnboundedSender<Removal>, parts: &Parts, body: Value) {
	let authorization = parts
		.headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.unwrap_or_default()
		.to_owned();

	removals
		.send(Removal { authorization, body })
		.ok();
}

/// Takes one RemoveParticipant request for each expected participant, and
/// fails on a request for any other or for one already removed.
///
/// It reads exactly as many requests as there are expected participants.
#[tracing::instrument(level = "debug", skip_all)]
async fn removals(
	seen: &mut UnboundedReceiver<Removal>,
	expected: BTreeSet<(String, String)>,
	since: u64,
) -> Result {
	let count = expected.len();

	try_unfold(seen, async |seen| {
		let removal = removed(&next_removal(seen).await?, since)?;

		Ok(Some((removal, seen)))
	})
	.take(count)
	.try_fold(expected, |mut expected, removal| {
		let unseen = expected.remove(&removal);

		ready(
			unseen.then_some(expected).ok_or_else(|| {
				err!("LiveKit was asked to remove {removal:?} unexpectedly or again")
			}),
		)
	})
	.await
	.map(drop)
}

#[tracing::instrument(level = "debug", skip_all)]
async fn next_removal(seen: &mut UnboundedReceiver<Removal>) -> Result<Removal> {
	timeout(KICK_DEADLINE, seen.recv())
		.await
		.ok()
		.flatten()
		.ok_or_else(|| err!("LiveKit was asked to remove no participant"))
}

/// Checks a RemoveParticipant request and returns the room and identity it
/// names.
///
/// Its token must administer that room, and it must revoke the tokens issued
/// until the moment of the call, which falls between `since` and now.
fn removed(request: &Removal, since: u64) -> Result<(String, String)> {
	let bearer = request
		.authorization
		.strip_prefix("Bearer ")
		.ok_or_else(|| err!("no Bearer token on a RemoveParticipant"))?;

	let grant = &claims(bearer)?["video"];
	let room = field(&request.body, "room")?.to_owned();
	let identity = field(&request.body, "identity")?.to_owned();
	let revoked = request.body["revoke_token_ts"]
		.as_u64()
		.unwrap_or_default();

	assert_eq!((&grant["room"], &grant["roomAdmin"]), (&json!(room), &json!(true)), "{grant}");
	assert!((since..=now_secs()).contains(&revoked), "revoke_token_ts {revoked} is not now");
	assert_eq!(
		request.body,
		json!({ "room": room, "identity": identity, "revoke_token_ts": revoked })
	);

	Ok((room, identity))
}
