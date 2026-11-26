//! Issues LiveKit access tokens for MatrixRTC calls, follows delegated leave
//! events, removes departed users and receives LiveKit's webhooks (MSC4195).
//!
//! A token admits one call member to the LiveKit room of one call slot. Room
//! names and participant identities are hashes, so the SFU learns neither the
//! Matrix room, the slot nor the user. The room is created through LiveKit's
//! RoomService before a token is signed, so a failing SFU yields no token. A
//! member may also hand this server its delayed leave event, which is kept
//! alive while LiveKit reports the member connected and sent once it leaves.
//! When a user leaves a room or is banned from it, the participants this
//! server recorded issuing it tokens for there are kicked from LiveKit, and a
//! webhook reporting one of them joining again has it removed once more.

mod delegation;
mod get_token;
mod kick;
mod names;
mod room_service;
#[cfg(test)]
mod tests;
mod token;
mod webhook;

use std::sync::Arc;

use async_trait::async_trait;
use futures::future::join;
use loole::{Receiver, Sender, unbounded};
use ruma::{OwnedRoomId, OwnedUserId, RoomId, UserId};
use tuwunel_core::{
	Result, err, implement,
	jwt::{DecodingKey, EncodingKey, errors::Error as JwtError},
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

// A token's row in the kick's index: room, user, LiveKit room, identity and generation.
type Issued<'a> = (&'a RoomId, &'a UserId, &'a str, &'a str, &'a str);

/// Issues LiveKit tokens, follows delegations, kicks departed users and
/// receives webhooks for the SFU configured at startup.
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

	/// Makes each poll of a participant act on LiveKit's answer before the next
	/// poll of it asks.
	///
	/// A webhook and the worker may poll the same participant at once. A poll
	/// holds this across its RoomService call, so a later poll asks only once
	/// the earlier one has acted; a delegation request never takes it, so it
	/// never waits on LiveKit. The map holds an entry only while a poll is
	/// working on that participant.
	polls: MutexMap<Participant, ()>,

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
	lkparticipant_roomuserid: Arc<Map>,
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

	/// Verifying key made from the API secret, for webhook tokens.
	webhook_key: DecodingKey,

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
			lkparticipant_roomuserid: args.db["lkparticipant_roomuserid"].clone(),
			roomuserid_lkparticipant: args.db["roomuserid_lkparticipant"].clone(),
		};

		let service = Self {
			services: args.services.clone(),
			db,
			locks: MutexMap::new(),
			polls: MutexMap::new(),
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
/// is signed, the participant is recorded twice. One record maps it to the
/// room and user, for a webhook to check the user's membership when it joins.
/// The other lists it under the room and user, for the kick when the user
/// leaves, under a fresh generation so that a kick already under way never
/// deletes it. The caller has checked that the user may join the call, and
/// issuance checks the membership again once both records are written. If
/// the user departed meanwhile, either the kick finds its record or the
/// recheck sees the departure, refuses the token and deletes the kick's
/// record.
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

	self.record(key);

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

/// Records an issued token's participant for both its readers, in one batch.
///
/// The webhook's record maps the participant to its room and user, which its
/// names hash, so every token for it writes the same record. The kick's
/// record lists the participant under the room and user, one per token.
#[implement(Service)]
fn record(&self, key @ (room_id, user_id, room, identity, _): Issued<'_>) {
	let mut txn = self.services.db.txn();

	txn.put(&self.db.lkparticipant_roomuserid, (room, identity), (room_id, user_id));
	txn.put_raw(&self.db.roomuserid_lkparticipant, key, []);
	txn.execute();
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
		let secret = api_secret.as_bytes();
		let invalid = |e: JwtError| {
			err!(Config("livekit.api_secret", "LiveKit api_secret is not a usable HMAC key: {e}"))
		};

		Ok(Self {
			url: url.to_owned(),
			api_key: api_key.into(),
			key: EncodingKey::from_secret(secret).map_err(invalid)?,
			webhook_key: DecodingKey::from_secret(secret).map_err(invalid)?,
			create_room: Method::new(url, "CreateRoom")?,
			get_participant: Method::new(url, "GetParticipant")?,
			remove_participant: Method::new(url, "RemoveParticipant")?,
		})
	}
}
