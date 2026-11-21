//! Signs LiveKit access tokens.
//!
//! A join token admits one participant to one room with the grants MSC4195
//! lists. An API token authorizes one of this server's own RoomService calls
//! and is sent only to LiveKit.

use std::{ops::Not, sync::LazyLock, time::Duration};

use serde::Serialize;
use tuwunel_core::{
	Result, err, implement,
	jwt::{Algorithm, Header, encode},
	utils::time::now_secs,
};

use super::Conf;

#[derive(Serialize)]
struct Claims<'a> {
	iss: &'a str,

	#[serde(skip_serializing_if = "Option::is_none")]
	sub: Option<&'a str>,

	nbf: u64,
	exp: u64,
	video: Grant<'a>,
}

/// LiveKit's video grant.
///
/// The grants MSC4195 lists are always written, an explicit `false`
/// included, so every LiveKit version reads the token the same way.
/// `roomAdmin` appears on API tokens only.
#[derive(Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Grant<'a> {
	room: &'a str,
	room_create: bool,

	#[serde(skip_serializing_if = "Not::not")]
	room_admin: bool,

	room_join: bool,
	can_publish: bool,
	can_subscribe: bool,
	can_update_own_metadata: bool,
	hidden: bool,
}

/// Lifetime of a join token.
///
/// It is the reference implementation's value and MSC4195's example. LiveKit
/// renews a connected participant's token itself, so this bounds how long a
/// token admits a participant who is not connected.
const JOIN_TOKEN_TTL: Duration = Duration::from_hours(1);

/// Lifetime of a RoomService token.
///
/// Each token is sent immediately and used for one call. LiveKit allows one
/// minute of clock skew.
const API_TOKEN_TTL: Duration = Duration::from_mins(5);

// HS256 is the one algorithm LiveKit verifies.
static HEADER: LazyLock<Header> = LazyLock::new(|| Header::new(Algorithm::HS256));

impl<'a> Grant<'a> {
	pub(super) fn create_room(room: &'a str) -> Self {
		Self {
			room,
			room_create: true,
			..Default::default()
		}
	}
}

/// Signs a token admitting one participant to one room.
///
/// The participant may publish and subscribe, which
/// [MSC4195](https://github.com/matrix-org/matrix-spec-proposals/pull/4195)
/// requires for local users and asks for remote ones, and may neither create
/// rooms nor hide or change its own metadata.
#[implement(Conf)]
pub(super) fn join_token(&self, identity: &str, room: &str) -> Result<String> {
	let grant = Grant {
		room,
		room_join: true,
		can_publish: true,
		can_subscribe: true,
		..Default::default()
	};

	self.sign(Some(identity), grant, JOIN_TOKEN_TTL)
}

/// Signs a token for one RoomService call.
///
/// The caller supplies the grant its method needs; the token names no
/// participant.
#[implement(Conf)]
pub(super) fn api_token(&self, grant: Grant<'_>) -> Result<String> {
	self.sign(None, grant, API_TOKEN_TTL)
}

#[implement(Conf)]
fn sign(&self, sub: Option<&str>, video: Grant<'_>, ttl: Duration) -> Result<String> {
	// MSC4195 asks for `nbf` now: token revocation compares against it.
	let nbf = now_secs();
	let claims = Claims {
		iss: &self.api_key,
		sub,
		nbf,
		exp: nbf.saturating_add(ttl.as_secs()),
		video,
	};

	encode(&HEADER, &claims, &self.key)
		.map_err(|error| err!(error!(%error, "Failed to sign a LiveKit token.")))
}
