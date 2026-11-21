//! Issues LiveKit access tokens for MatrixRTC calls, follows delegated leave
//! events and removes departed users (MSC4195).
//!
//! A token admits one call member to the LiveKit room of one call slot. Room
//! names and participant identities are hashes, so the SFU learns neither the
//! Matrix room, the slot nor the user. The room is created through LiveKit's
//! RoomService before a token is signed, so a failing SFU yields no token. A
//! member may also hand this server its delayed leave event, which is kept
//! alive while LiveKit reports the member connected and sent once it leaves.
//! When a user leaves a room or is banned from it, the participants this
//! server recorded issuing it tokens for there are kicked from LiveKit.

mod delegation;
mod get_token;
mod kick;
mod names;
mod room_service;
#[cfg(test)]
mod tests;
mod token;

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join;
use loole::{Receiver, Sender, unbounded};
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tuwunel_core::{
	Result, err, implement,
	jwt::EncodingKey,
	smallstr::SmallString,
	utils::{MutexMap, rand::string_array},
};
use tuwunel_database::Map;

use self::{
	delegation::{Generation, Participant},
	get_token::forbidden,
	names::{identity, room_name},
	room_service::Method,
};
use crate::{Args, SelfServices, service::make_name};

// The 15-byte keys `livekit-server generate-keys` prints stay inline.
type ApiKey = SmallString<[u8; 16]>;

type Departure = (OwnedRoomId, OwnedUserId);

/// Issues LiveKit tokens, follows delegations and kicks departed users on the
/// SFU configured at startup.
///
/// It keeps the SFU's address and the signing key from the startup
/// configuration, which a reload cannot change.
pub struct Service {
	services: SelfServices,
	db: Data,

	/// Serializes the writes to one participant's delegation.
	///
	/// The map holds an entry only while a delegation request or a poll is
	/// working on that participant.
	locks: MutexMap<Participant, ()>,

	/// Departures of users issued tokens in the room, for the worker to kick.
	///
	/// It is unbounded because a membership update must never wait on LiveKit,
	/// and dropping a departure when a bounded queue was full would miss a
	/// kick. An entry is a room and a user ID, queued once per departure of a
	/// token holder, so a backlog grows only while departures outpace kicks
	/// whose every RoomService call is bounded by its timeout. The channel
	/// closes at shutdown, and the departures still queued are dropped.
	departures: (Sender<Departure>, Receiver<Departure>),

	/// The SFU's connection details, or `None` when native LiveKit is off.
	conf: Option<Conf>,
}

struct Data {
	lkparticipant_delegation: Arc<Map>,
	roomuserid_lkparticipant: Arc<Map>,
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

	/// RoomService's GetParticipant method, which polling a delegation calls.
	get_participant: Method,

	/// RoomService's RemoveParticipant method, which a kick calls to remove a
	/// participant and revoke its tokens.
	remove_participant: Method,
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

		let db = Data {
			lkparticipant_delegation: args.db["lkparticipant_delegation"].clone(),
			roomuserid_lkparticipant: args.db["roomuserid_lkparticipant"].clone(),
		};

		let service = Self {
			services: args.services.clone(),
			db,
			locks: MutexMap::new(),
			departures: unbounded(),
			conf,
		};

		Ok(Arc::new(service))
	}

	#[tracing::instrument(level = "debug", skip_all)]
	async fn worker(self: Arc<Self>) -> Result {
		// A read-only database could record no outcome, so each poll would repeat it.
		if self.conf.is_none() || self.services.db.is_read_only() {
			return Ok(());
		}

		join(self.delegation_loop(), self.kick_loop()).await;

		Ok(())
	}

	async fn interrupt(&self) {
		let (departures, _) = &self.departures;

		departures.close();
	}

	fn name(&self) -> &str { make_name(module_path!()) }
}

/// Issues a token admitting a user's call member to the LiveKit room of a
/// call slot.
///
/// The room is created first, so a failing SFU yields no token. Once the token
/// is signed, the participant is recorded for the kick when the user leaves
/// the room, under a fresh generation, so a kick already under way never
/// deletes the record. The caller has checked that the user may join the
/// call; the user must still be joined once the record is written, so if the
/// user departs meanwhile, either the kick finds the record or this sees the
/// departure, refuses the token and deletes the record.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
async fn issue_token(
	&self,
	user_id: &UserId,
	room_id: &RoomId,
	slot_id: &str,
	member_id: &str,
) -> Result<String> {
	let conf = self.conf()?;
	let room = room_name(room_id, slot_id);

	self.create_room(conf, &room).await?;

	let identity = identity(user_id, member_id);
	let token = conf.join_token(&identity, &room)?;
	let generation: Generation = string_array();
	let key = (room_id, user_id, room.as_str(), identity.as_str(), generation.as_str());

	self.db.roomuserid_lkparticipant.put_raw(key, []);

	// Read after the write, so a departure's kick either finds the record or is seen here.
	let joined = self
		.services
		.state_cache
		.is_joined(user_id, room_id)
		.await;

	if !joined {
		self.db.roomuserid_lkparticipant.del(key);

		return Err(forbidden());
	}

	Ok(token)
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
fn is_own_url(&self, url: &str) -> bool {
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
			get_participant: Method::new(url, "GetParticipant")?,
			remove_participant: Method::new(url, "RemoveParticipant")?,
		})
	}
}
