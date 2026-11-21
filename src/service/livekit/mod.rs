//! Issues LiveKit access tokens for MatrixRTC calls (MSC4195).
//!
//! A token admits one call member to the LiveKit room of one call slot. Room
//! names and participant identities are hashes, so the SFU learns neither the
//! Matrix room, the slot nor the user. The room is created through LiveKit's
//! RoomService before a token is signed, so a failing SFU yields no token.

mod names;
mod room_service;
#[cfg(test)]
mod tests;
mod token;

use std::sync::Arc;

use async_trait::async_trait;
use ruma::{RoomId, UserId};
use tuwunel_core::{Result, err, implement, jwt::EncodingKey, smallstr::SmallString};

use self::{
	names::{identity, room_name},
	room_service::Method,
};
use crate::{Args, SelfServices, service::make_name};

// The 15-byte keys `livekit-server generate-keys` prints stay inline.
type ApiKey = SmallString<[u8; 16]>;

/// Issues LiveKit tokens against the SFU configured at startup.
///
/// It keeps the SFU's address and the signing key from the startup
/// configuration, which a reload cannot change.
pub struct Service {
	services: SelfServices,

	/// The SFU's connection details, or `None` when native LiveKit is off.
	conf: Option<Conf>,
}

/// Connection details of the configured SFU, fixed for the server's lifetime.
///
/// A reload that changes `[global.livekit]` is refused, so these never go
/// stale.
struct Conf {
	/// WebSocket URL clients connect to, exactly as configured.
	url: String,

	/// API key, the issuer of every token.
	api_key: ApiKey,

	/// Signing key made from the API secret.
	key: EncodingKey,

	/// RoomService's CreateRoom method.
	create_room: Method,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &Args<'_>) -> Result<Arc<Self>> {
		let conf = args
			.server
			.config
			.livekit
			.enabled()
			.map(Conf::new)
			.transpose()?;

		Ok(Arc::new(Self { services: args.services.clone(), conf }))
	}

	fn name(&self) -> &str { make_name(module_path!()) }
}

/// Issues a token admitting a user's call member to the LiveKit room of a
/// call slot.
///
/// The room is created first, so a failing SFU yields no token. The caller
/// has checked that the user may join the call.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn issue_token(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	slot_id: &str,
	member_id: &str,
) -> Result<String> {
	let conf = self.conf()?;
	let room = room_name(room_id, slot_id);

	self.create_room(conf, &room).await?;
	conf.join_token(&identity(user_id, member_id), &room)
}

/// Whether native LiveKit is configured.
///
/// The answer comes from the startup configuration; it makes no request to
/// the SFU.
#[implement(Service)]
#[inline]
#[must_use]
pub fn is_enabled(&self) -> bool { self.conf.is_some() }

/// Whether a request's `url` names this server's SFU.
///
/// The comparison is exact, with no normalization: a request for any other
/// URL is one this server cannot issue tokens for.
#[implement(Service)]
#[must_use]
pub fn is_own_url(&self, url: &str) -> bool {
	self.conf
		.as_ref()
		.is_some_and(|conf| conf.url == url)
}

#[implement(Service)]
fn conf(&self) -> Result<&Conf> {
	self.conf
		.as_ref()
		.ok_or_else(|| err!(Request(NotFound("LiveKit is not configured on this server."))))
}

impl Conf {
	fn new((url, api_key, api_secret): (&str, &str, &str)) -> Result<Self> {
		Ok(Self {
			url: url.to_owned(),
			api_key: api_key.into(),
			key: EncodingKey::from_secret(api_secret.as_bytes()),
			create_room: Method::new(url, "CreateRoom")?,
		})
	}
}
