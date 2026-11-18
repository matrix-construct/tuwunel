use std::future::Future;

use axum::{
	Json,
	extract::{RawQuery, State},
	http::HeaderMap,
	response::IntoResponse,
};
use axum_extra::headers::{Authorization, HeaderMapExt, authorization::Bearer};
use futures::{Stream, StreamExt, pin_mut};
use ruma::{OwnedRoomId, OwnedUserId, ServerName};
use serde::{Deserialize, Serialize};
use tuwunel_core::{Err, Result};
use tuwunel_service::appservice::RegistrationInfo;

/// # `GET /_tuwunel/server_version`
///
/// Tuwunel-specific API to get the server version, results akin to
/// `/_matrix/federation/v1/version`
pub(crate) async fn tuwunel_server_version() -> Result<impl IntoResponse> {
	Ok(Json(serde_json::json!({
		"name": tuwunel_core::version::name(),
		"version": tuwunel_core::version::version(),
	})))
}

/// # `GET /_tuwunel/local_user_count`
///
/// Tuwunel-specific API to return the amount of users registered on this
/// homeserver. Endpoint is disabled if federation is disabled for privacy. This
/// only includes active users (not deactivated, no guests, etc)
pub(crate) async fn tuwunel_local_user_count(
	State(services): State<crate::State>,
) -> Result<impl IntoResponse> {
	let user_count = services.users.list_local_users().count().await;

	Ok(Json(serde_json::json!({
		"count": user_count
	})))
}

/// Most rooms one `shared_rooms` answer carries. When more rooms qualify the
/// answer stops at this many and says `"limited": true`.
pub(crate) const SHARED_ROOMS_CAP: usize = 1000;

/// Membership point-reads kept in flight at once. `buffered` yields results
/// in input order, so this does not change the order of the answer.
const SHARED_ROOMS_READ_AHEAD: usize = 16;

#[derive(Debug, Default, Deserialize)]
struct SharedRoomsQuery {
	user_id: Option<String>,
	with: Option<String>,
	access_token: Option<String>,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct SharedRoomsResponse {
	rooms: Vec<SharedRoom>,
	limited: bool,
}

#[derive(Debug, PartialEq, Eq, Serialize)]
struct SharedRoom {
	room_id: OwnedRoomId,
	with_membership: WithMembership,
}

/// The membership of the `with` user in a room of the answer.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
enum WithMembership {
	Join,
	Invite,
}

/// # `GET /_tuwunel/appservice/v1/shared_rooms?user_id=<V>&with=<R>`
///
/// Tuwunel-specific, read-only. For an appservice: the rooms where `user_id`
/// (a user in the appservice's namespace) is joined and `with` is joined or
/// invited, in the order `GET /_matrix/client/v3/joined_rooms` lists
/// `user_id`'s rooms (both walk `userroomid_joined` forwards, i.e. ascending
/// room id bytes). Replaces "list V's joined rooms, then ask per room whether
/// R is a member" with one request.
///
/// Auth: the `as_token` of a registered appservice, as `Authorization:
/// Bearer` or `?access_token=` (as the regular appservice auth accepts), and
/// `user_id` inside its user namespace; anything else is 403 `M_FORBIDDEN`.
/// Malformed or non-local `user_id`/`with` is 400 `M_INVALID_PARAM`.
///
/// Cost: one forward scan of V's `userroomid_joined` prefix plus up to two
/// point-reads (`userroomid_invitestate`, `userroomid_joined`) per room of V:
/// O(|V's joined rooms|), independent of how many rooms R or the server has.
pub(crate) async fn tuwunel_appservice_shared_rooms(
	State(services): State<crate::State>,
	headers: HeaderMap,
	RawQuery(query): RawQuery,
) -> Result<impl IntoResponse> {
	let query = serde_html_form::from_str::<SharedRoomsQuery>(query.as_deref().unwrap_or(""));

	// Same token sources, in the same precedence, as the regular client auth
	// (`router/auth.rs`): the bearer header, else `?access_token=`.
	let bearer = headers.typed_get::<Authorization<Bearer>>();
	let token = bearer
		.as_ref()
		.map(|Authorization(bearer)| bearer.token())
		.or_else(|| {
			query
				.as_ref()
				.ok()
				.and_then(|query| query.access_token.as_deref())
		});

	// Only appservice tokens are looked up; a user's access token is simply not
	// found here and refused like any other unknown token.
	let appservice = match token {
		| Some(token) => services
			.appservice
			.find_from_access_token(token)
			.await
			.ok(),
		| None => None,
	};

	let (user_id, with) =
		authorize(appservice.as_ref(), query.as_ref().ok(), services.globals.server_name())?;

	let state_cache = &services.state_cache;
	let with = &with;
	let memberships = state_cache
		.rooms_joined(&user_id)
		// Own each id before reading ahead. `rooms_joined` yields keys borrowed
		// from the database iterator's buffer, which the next item overwrites;
		// with up to SHARED_ROOMS_READ_AHEAD reads in flight, a borrowed id
		// named whichever room the iterator was on by then: answers came back
		// with one room repeated and real shared rooms missing.
		.map(ToOwned::to_owned)
		.map(|room_id: OwnedRoomId| async move {
			let membership = read_with_membership(
				state_cache.is_invited(with, &room_id),
				state_cache.is_joined(with, &room_id),
			)
			.await;
			(room_id, membership)
		})
		.buffered(SHARED_ROOMS_READ_AHEAD)
		// Boxing pins the borrowed lifetimes down; without it the handler
		// future fails axum's higher-ranked `Send` check.
		.boxed();

	let response = collect_shared_rooms(memberships, SHARED_ROOMS_CAP).await;

	Ok(Json(response))
}

/// The whole access decision, in the order the checks are made:
/// 1. no registered appservice behind the token -> 403;
/// 2. a query string that does not parse, or `user_id` / `with` missing,
///    malformed or not on this server -> 400;
/// 3. `user_id` outside the appservice's user namespace -> 403.
///
/// The token is checked before the parameters so an unauthenticated caller
/// learns nothing from the parameter errors. The namespace test is the one the
/// regular appservice auth uses for `?user_id=` masquerading
/// (`RegistrationInfo::is_user_match`: the namespace regexes, exclusive or
/// not, plus the appservice's own sender user).
fn authorize(
	appservice: Option<&RegistrationInfo>,
	query: Option<&SharedRoomsQuery>,
	server_name: &ServerName,
) -> Result<(OwnedUserId, OwnedUserId)> {
	let Some(appservice) = appservice else {
		return Err!(Request(Forbidden("A registered appservice's as_token is required.")));
	};

	let Some(query) = query else {
		return Err!(Request(InvalidParam("Query string could not be parsed.")));
	};

	let user_id = local_user_param(query.user_id.as_deref(), "user_id", server_name)?;
	let with = local_user_param(query.with.as_deref(), "with", server_name)?;

	if !appservice.is_user_match(&user_id) {
		return Err!(Request(Forbidden("user_id is not in the appservice's user namespace.")));
	}

	Ok((user_id, with))
}

fn local_user_param(
	value: Option<&str>,
	name: &str,
	server_name: &ServerName,
) -> Result<OwnedUserId> {
	let Some(value) = value else {
		return Err!(Request(InvalidParam("Missing {name}.")));
	};

	let Ok(user_id) = OwnedUserId::parse(value) else {
		return Err!(Request(InvalidParam("{name} is not a valid user id.")));
	};

	if user_id.server_name() != server_name {
		return Err!(Request(InvalidParam("{name} is not a user on this server.")));
	}

	Ok(user_id)
}

/// R's membership in one room, read invite-first.
///
/// Every membership write puts the new index entry before it removes the old
/// one (`mark_as_joined`: put `userroomid_joined`, then remove
/// `userroomid_invitestate`). Reading the invite first means an invite -> join
/// racing with this read is still seen: either the invite read lands before
/// the invite is removed (true), or it lands after, by which point the join
/// entry already exists for the second read. Reading join first could miss
/// both. When both entries are present (mid-transition) the answer is `join`.
async fn read_with_membership<I, J>(is_invited: I, is_joined: J) -> Option<WithMembership>
where
	I: Future<Output = bool>,
	J: Future<Output = bool>,
{
	let invited = is_invited.await;
	let joined = is_joined.await;

	match (joined, invited) {
		| (true, _) => Some(WithMembership::Join),
		| (false, true) => Some(WithMembership::Invite),
		| (false, false) => None,
	}
}

/// Takes R's membership for each of V's joined rooms, in V's joined-rooms
/// order, keeps the rooms R is joined or invited to, and stops after `cap`
/// kept rooms. `limited` is true only when a further qualifying room exists
/// beyond the cap, so an answer is never cut short without saying so.
async fn collect_shared_rooms<R, S>(memberships: S, cap: usize) -> SharedRoomsResponse
where
	R: Into<OwnedRoomId>,
	S: Stream<Item = (R, Option<WithMembership>)> + Send,
{
	pin_mut!(memberships);

	let mut rooms = Vec::new();
	let mut limited = false;
	while let Some((room_id, membership)) = memberships.next().await {
		let Some(with_membership) = membership else {
			continue;
		};

		if rooms.len() >= cap {
			limited = true;
			break;
		}

		rooms.push(SharedRoom {
			room_id: room_id.into(),
			with_membership,
		});
	}

	SharedRoomsResponse { rooms, limited }
}

#[cfg(test)]
mod tests {
	use std::{cell::Cell, collections::HashMap};

	use futures::{FutureExt, future::ready, stream};
	use ruma::{
		OwnedServerName, RoomId, api::appservice::Registration, owned_room_id, server_name,
	};

	use super::*;

	const SERVER: &str = "hs.test";

	/// R's membership as the scan sees it, in every state it can be in.
	#[derive(Clone, Copy)]
	enum Membership {
		Join,
		Invite,
		Leave,
		Ban,
		Knock,
	}

	/// What `is_invited` / `is_joined` read for R in each state: leave, ban and
	/// knock live in other indexes (`userroomid_leftstate`,
	/// `userroomid_knockedstate`) and read false on both.
	fn index_reads(membership: Option<Membership>) -> (bool, bool) {
		match membership {
			| Some(Membership::Invite) => (true, false),
			| Some(Membership::Join) => (false, true),
			| Some(Membership::Leave | Membership::Ban | Membership::Knock) | None =>
				(false, false),
		}
	}

	fn room(n: usize) -> OwnedRoomId {
		format!("!room{n:04}:{SERVER}")
			.try_into()
			.unwrap()
	}

	/// The handler's pipeline with the index reads replaced by `r`.
	async fn run(
		v_joined: &[OwnedRoomId],
		r: &HashMap<OwnedRoomId, Membership>,
		cap: usize,
	) -> SharedRoomsResponse {
		let memberships = stream::iter(v_joined.iter().map(AsRef::as_ref))
			.map(|room_id: &RoomId| {
				let (invited, joined) = index_reads(r.get(room_id).copied());
				read_with_membership(ready(invited), ready(joined))
					.map(move |membership| (room_id, membership))
			})
			.buffered(SHARED_ROOMS_READ_AHEAD);

		collect_shared_rooms(memberships, cap).await
	}

	#[tokio::test]
	async fn keeps_only_rooms_where_with_is_joined_or_invited() {
		let v_joined: Vec<_> = (0..7).map(room).collect();
		let r = HashMap::from([
			(room(0), Membership::Join),
			(room(1), Membership::Invite),
			(room(2), Membership::Leave),
			(room(3), Membership::Ban),
			(room(4), Membership::Knock),
			// room(5): R never in it
			(room(6), Membership::Invite),
		]);

		let response = run(&v_joined, &r, SHARED_ROOMS_CAP).await;

		assert_eq!(response, SharedRoomsResponse {
			rooms: vec![
				SharedRoom {
					room_id: room(0),
					with_membership: WithMembership::Join
				},
				SharedRoom {
					room_id: room(1),
					with_membership: WithMembership::Invite
				},
				SharedRoom {
					room_id: room(6),
					with_membership: WithMembership::Invite
				},
			],
			limited: false,
		});
	}

	#[tokio::test]
	async fn rooms_r_is_in_but_v_is_not_joined_to_are_never_listed() {
		// Only V's joined rooms are walked: R's other rooms (and rooms V is
		// merely invited to / left) never enter the stream.
		let v_joined = vec![room(3)];
		let r = HashMap::from([
			(room(1), Membership::Join),
			(room(2), Membership::Invite),
			(room(3), Membership::Join),
		]);

		let response = run(&v_joined, &r, SHARED_ROOMS_CAP).await;

		assert_eq!(response.rooms, vec![SharedRoom {
			room_id: room(3),
			with_membership: WithMembership::Join
		}]);
	}

	#[tokio::test]
	async fn order_is_the_joined_rooms_order() {
		// Feed V's rooms in an order that is not sorted, with the membership
		// reads completing in reverse: the answer must still follow the input.
		let v_joined: Vec<_> = [9, 2, 7, 0, 5, 1, 8]
			.into_iter()
			.map(room)
			.collect();
		let r: HashMap<_, _> = v_joined
			.iter()
			.cloned()
			.map(|room_id| (room_id, Membership::Join))
			.collect();

		let memberships = stream::iter(v_joined.iter().map(AsRef::as_ref))
			.map(|room_id: &RoomId| {
				let delay = 20_u64.saturating_sub(
					room_id
						.as_str()
						.get(5..9)
						.unwrap()
						.parse::<u64>()
						.unwrap(),
				);
				let (invited, joined) = index_reads(r.get(room_id).copied());
				async move {
					tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
					(room_id, read_with_membership(ready(invited), ready(joined)).await)
				}
			})
			.buffered(SHARED_ROOMS_READ_AHEAD);
		let response = collect_shared_rooms(memberships, SHARED_ROOMS_CAP).await;

		let got: Vec<_> = response
			.rooms
			.into_iter()
			.map(|room| room.room_id)
			.collect();
		assert_eq!(got, v_joined);
	}

	#[tokio::test]
	async fn first_match_is_first_in_joined_rooms_order() {
		// The old client-side scan stops at the first qualifying room it
		// reaches in joined_rooms order; this answer's first element is that
		// room, more than SHARED_ROOMS_READ_AHEAD rooms in.
		let v_joined: Vec<_> = (0..100).map(room).collect();
		let r = HashMap::from([
			(room(40), Membership::Invite),
			(room(41), Membership::Join),
			(room(90), Membership::Join),
		]);

		let response = run(&v_joined, &r, SHARED_ROOMS_CAP).await;

		assert_eq!(response.rooms[0].room_id, room(40));
		assert_eq!(response.rooms.len(), 3);
	}

	#[tokio::test]
	async fn cap_sets_limited_only_when_a_room_is_left_out() {
		let v_joined: Vec<_> = (0..10).map(room).collect();
		let all_join: HashMap<_, _> = v_joined
			.iter()
			.cloned()
			.map(|room_id| (room_id, Membership::Join))
			.collect();

		// Exactly `cap` qualifying rooms: complete, not limited.
		let response = run(&v_joined, &all_join, 10).await;
		assert_eq!(response.rooms.len(), 10);
		assert!(!response.limited);

		// One more qualifying room than the cap: cut, and says so.
		let response = run(&v_joined, &all_join, 9).await;
		assert_eq!(response.rooms.len(), 9);
		assert!(response.limited);
		assert_eq!(response.rooms.last().unwrap().room_id, room(8));

		// More joined rooms than the cap, but few qualify: not limited.
		let few = HashMap::from([(room(2), Membership::Join), (room(9), Membership::Invite)]);
		let response = run(&v_joined, &few, 2).await;
		assert_eq!(response.rooms.len(), 2);
		assert!(!response.limited);

		// No rooms at all (unknown V): empty, not limited.
		let response = run(&[], &all_join, 10).await;
		assert_eq!(response, SharedRoomsResponse { rooms: vec![], limited: false });
	}

	/// The index writes of an invite -> join, in the order `mark_as_joined`
	/// makes them; `step` is how many have landed: 0 invite only, 1 both,
	/// 2 join only.
	fn invite_to_join(step: u8) -> (bool, bool) { (step < 2, step >= 1) }

	/// Reads R's membership while the invite -> join writes land at any point
	/// between and around the two reads.
	#[expect(clippy::future_not_send)]
	async fn read_during_invite_to_join(start: u8, between: u8, invite_first: bool) -> bool {
		let step = Cell::new(start);
		let first = async {
			let (invited, joined) = invite_to_join(step.get());
			step.set(step.get().saturating_add(between).min(2));
			if invite_first { invited } else { joined }
		};
		let second = async {
			let (invited, joined) = invite_to_join(step.get());
			if invite_first { joined } else { invited }
		};

		if invite_first {
			read_with_membership(first, second)
				.await
				.is_some()
		} else {
			// Same conjunction, reads made in the other order.
			let joined = first.await;
			let invited = second.await;
			joined || invited
		}
	}

	#[tokio::test]
	async fn invite_then_join_is_never_missed_mid_transition() {
		for start in 0..=2 {
			for between in 0..=(2 - start) {
				assert!(
					read_during_invite_to_join(start, between, true).await,
					"invite-first read missed R at start={start} between={between}"
				);
			}
		}

		// The opposite read order can miss it: join read before the join is
		// written, invite read after the invite is removed.
		assert!(!read_during_invite_to_join(0, 2, false).await);
	}

	#[tokio::test]
	async fn both_entries_present_reads_as_join() {
		assert_eq!(
			read_with_membership(ready(true), ready(true)).await,
			Some(WithMembership::Join)
		);
	}

	#[test]
	fn response_shape_matches_the_contract() {
		let response = SharedRoomsResponse {
			rooms: vec![
				SharedRoom {
					room_id: owned_room_id!("!abc:hs.test"),
					with_membership: WithMembership::Join,
				},
				SharedRoom {
					room_id: owned_room_id!("!def:hs.test"),
					with_membership: WithMembership::Invite,
				},
			],
			limited: false,
		};

		assert_eq!(
			serde_json::to_value(&response).unwrap(),
			serde_json::json!({
				"rooms": [
					{"room_id": "!abc:hs.test", "with_membership": "join"},
					{"room_id": "!def:hs.test", "with_membership": "invite"},
				],
				"limited": false,
			})
		);
	}

	fn appservice(users_regex: &str) -> RegistrationInfo {
		let registration: Registration = serde_json::from_value(serde_json::json!({
			"id": "bridge",
			"url": null,
			"as_token": "as-secret",
			"hs_token": "hs-secret",
			"sender_localpart": "bridgebot",
			"namespaces": {
				"users": [{"exclusive": true, "regex": users_regex}],
				"aliases": [],
				"rooms": [],
			},
		}))
		.unwrap();

		RegistrationInfo::new(registration, server_name!("hs.test")).unwrap()
	}

	fn query(user_id: Option<&str>, with: Option<&str>) -> SharedRoomsQuery {
		SharedRoomsQuery {
			user_id: user_id.map(ToOwned::to_owned),
			with: with.map(ToOwned::to_owned),
			access_token: None,
		}
	}

	fn status(result: Result<(OwnedUserId, OwnedUserId)>) -> (u16, String) {
		let err = result.unwrap_err();
		(err.status_code().as_u16(), err.kind().errcode().to_string())
	}

	const V: &str = "@sms_15551230000:hs.test";
	const R: &str = "@alice:hs.test";

	#[test]
	fn auth_accepts_appservice_with_v_in_namespace() {
		let info = appservice("@sms_.*:hs.test");
		let server: OwnedServerName = SERVER.try_into().unwrap();

		let (user_id, with) =
			authorize(Some(&info), Some(&query(Some(V), Some(R))), &server).unwrap();
		assert_eq!(user_id, V);
		assert_eq!(with, R);

		// R need not be in the namespace: it is the real user.
		authorize(Some(&info), Some(&query(Some(V), Some("@bob:hs.test"))), &server).unwrap();
	}

	#[test]
	fn auth_refusals() {
		let info = appservice("@sms_.*:hs.test");
		let server: OwnedServerName = SERVER.try_into().unwrap();
		let forbidden = (403, "M_FORBIDDEN".to_owned());
		let invalid = (400, "M_INVALID_PARAM".to_owned());
		let good = query(Some(V), Some(R));

		// No appservice behind the token (no token, a user's token, unknown):
		// refused before the parameters are even looked at.
		assert_eq!(status(authorize(None, Some(&good), &server)), forbidden);
		assert_eq!(status(authorize(None, Some(&query(None, None)), &server)), forbidden);
		assert_eq!(status(authorize(None, None, &server)), forbidden);

		// V outside this appservice's namespace.
		let other = appservice("@telegram_.*:hs.test");
		assert_eq!(status(authorize(Some(&other), Some(&good), &server)), forbidden);
		assert_eq!(
			status(authorize(Some(&info), Some(&query(Some(R), Some(V))), &server)),
			forbidden
		);

		// Malformed / missing / remote parameters, with a valid appservice.
		for (user_id, with) in [
			(None, Some(R)),
			(Some(V), None),
			(Some("sms_1555"), Some(R)),
			(Some(V), Some("alice")),
			(Some(""), Some(R)),
			(Some("@sms_15551230000:other.test"), Some(R)),
			(Some(V), Some("@alice:other.test")),
		] {
			assert_eq!(
				status(authorize(Some(&info), Some(&query(user_id, with)), &server)),
				invalid,
				"user_id={user_id:?} with={with:?}"
			);
		}
		assert_eq!(status(authorize(Some(&info), None, &server)), invalid);
	}

	#[test]
	fn query_string_parsing() {
		let parsed: SharedRoomsQuery = serde_html_form::from_str(
			"user_id=%40sms_15551230000%3Ahs.test&with=%40alice%3Ahs.test&access_token=t",
		)
		.unwrap();
		assert_eq!(parsed.user_id.as_deref(), Some(V));
		assert_eq!(parsed.with.as_deref(), Some(R));
		assert_eq!(parsed.access_token.as_deref(), Some("t"));

		let empty: SharedRoomsQuery = serde_html_form::from_str("").unwrap();
		assert!(empty.user_id.is_none() && empty.with.is_none());

		// Repeated parameters are a parse error -> 400, not a silent pick.
		serde_html_form::from_str::<SharedRoomsQuery>("user_id=a&user_id=b").unwrap_err();
	}

	#[test]
	fn unknown_users_are_not_an_error() {
		// Well-formed, local, but nobody registered them: accepted; the answer
		// is then whatever their (empty) indexes give.
		let info = appservice("@sms_.*:hs.test");
		let server: OwnedServerName = SERVER.try_into().unwrap();
		authorize(
			Some(&info),
			Some(&query(Some("@sms_19999999999:hs.test"), Some("@nobody:hs.test"))),
			&server,
		)
		.unwrap();
	}
}
