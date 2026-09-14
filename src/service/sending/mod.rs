mod data;
mod dest;
mod device;
mod sender;
#[cfg(test)]
mod tests;
mod worker;

use std::{
	io::Write,
	iter::{once, repeat_with},
	sync::{Arc, Mutex as StdMutex},
};

use async_trait::async_trait;
use futures::{Stream, StreamExt};
use loole::unbounded;
use ruma::{RoomId, ServerName, UserId};
use tokio::task::JoinSet;
use tuwunel_core::{
	Result, Server, debug_warn, implement,
	smallvec::SmallVec,
	utils::{IterStream, ReadyExt, TryReadyExt, result::LogErr},
};

use self::worker::num_senders;
pub use self::{
	data::Data,
	dest::Destination,
	sender::{EDU_LIMIT, PDU_LIMIT},
};
use crate::rooms::timeline::RawPduId;

/// Outbound delivery of PDUs and EDUs to federation peers, appservices, and
/// push gateways.
///
/// Requests are written as durable queue rows and dispatched to a pool of
/// sender workers sharded by destination.
pub struct Service {
	pub db: Data,
	server: Arc<Server>,
	services: Arc<crate::services::OnceServices>,
	channels: Vec<(loole::Sender<Msg>, loole::Receiver<Msg>)>,

	// Aborted and joined when the service stops.
	flushes: StdMutex<JoinSet<()>>,
}

/// One queued unit of delivery.
///
/// PDUs are referenced by ID; the EDU variants carry their serialized body.
#[expect(clippy::module_name_repetitions)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SendingEvent {
	/// A room event, fetched from the timeline by ID at dispatch.
	Pdu(RawPduId),

	/// A serialized EDU body.
	Edu(EduBuf),

	/// A serialized to-device message for an appservice (MSC4203).
	ToDevice(EduBuf),

	/// A serialized device-list change for an appservice (MSC3202).
	DeviceListChanged(EduBuf),

	/// Queue an account-wide counts-only push.
	///
	/// The sender recomputes the count when the row is delivered.
	BadgeRefresh,

	/// Wake the destination without queueing anything.
	Flush,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Msg {
	dest: Destination,
	event: SendingEvent,
	queue_id: Vec<u8>,
}

/// Inline buffer for one serialized EDU.
///
/// The budget keeps a typical EDU on the stack; a larger body spills to the
/// heap.
pub type EduBuf = SmallVec<[u8; EDU_BUF_CAP]>;

/// Inline collection of the EDU buffers composed for one transaction.
///
/// Most transactions carry at most one EDU.
pub type EduVec = SmallVec<[EduBuf; EDU_VEC_CAP]>;

const EDU_BUF_CAP: usize = 128 - 16;
const EDU_VEC_CAP: usize = 1;

// Leading bytes on queued sending values select tagged event variants. Legacy
// PDU and EDU rows cannot collide; the badge tag stands alone.
const TAG_TO_DEVICE: u8 = 0x01;
const TAG_DEVICE_LIST_CHANGED: u8 = 0x02;
const TAG_BADGE_REFRESH: u8 = 0x03;
const TAG_PREFIX_LEN: usize = 1 + size_of::<u64>();

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let channels = repeat_with(unbounded)
			.take(num_senders(args))
			.collect();

		Ok(Arc::new(Self {
			db: Data::new(args),
			server: args.server.clone(),
			services: args.services.clone(),
			channels,
			flushes: JoinSet::new().into(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result { self.run().await }

	async fn interrupt(&self) { self.close(); }

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }

	fn unconstrained(&self) -> bool { true }
}

/// Queue a PDU for delivery to one of a user's pushers.
///
/// The row is durable and the shard owning the destination is woken.
#[implement(Service)]
#[tracing::instrument(skip(self, pdu_id, user, pushkey), level = "debug")]
pub fn send_pdu_push(&self, pdu_id: &RawPduId, user: &UserId, pushkey: String) -> Result {
	let dest = Destination::Push(user.to_owned(), pushkey);
	let event = SendingEvent::Pdu(*pdu_id);
	let _cork = self.db.db.cork();

	self.queue_and_dispatch(dest, event)
}

#[implement(Service)]
fn queue_and_dispatch(&self, dest: Destination, event: SendingEvent) -> Result {
	let queue_id = self
		.db
		.queue_requests(once((&event, &dest)))
		.pop()
		.expect("request queue key");

	self.dispatch(Msg { dest, event, queue_id })
}

/// Queue a counts-only push refresh for every pusher owned by a user.
///
/// Rows are durable, coalesced, and recomputed at send time.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip(self))]
pub async fn refresh_push_badge(&self, user_id: &UserId) -> Result {
	self.services
		.pusher
		.get_pushkeys(user_id)
		.map(Ok)
		.ready_try_for_each(|pushkey| {
			let dest = Destination::Push(user_id.to_owned(), pushkey.to_owned());

			self.queue_and_dispatch(dest, SendingEvent::BadgeRefresh)
		})
		.await
}

/// Queue a PDU for delivery to an appservice.
///
/// The row is durable and the shard owning the destination is woken.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn send_pdu_appservice(&self, appservice_id: String, pdu_id: RawPduId) -> Result {
	let dest = Destination::Appservice(appservice_id);
	let event = SendingEvent::Pdu(pdu_id);
	let _cork = self.db.db.cork();

	self.queue_and_dispatch(dest, event)
}

/// Queue a PDU for delivery to every remote server in a room.
///
/// The fan-out is one durable row per server, dispatched under a single cork.
#[implement(Service)]
#[tracing::instrument(skip(self, room_id, pdu_id), level = "debug")]
pub async fn send_pdu_room(&self, room_id: &RoomId, pdu_id: &RawPduId) -> Result {
	let servers = self
		.services
		.state_cache
		.remote_room_servers(room_id);

	self.send_pdu_servers(servers, pdu_id).await
}

/// Queue a PDU for delivery to each of the given servers.
///
/// The fan-out is one durable row per server, dispatched under a single cork.
#[implement(Service)]
#[tracing::instrument(skip(self, servers, pdu_id), level = "debug")]
pub async fn send_pdu_servers<'a, S>(&self, servers: S, pdu_id: &RawPduId) -> Result
where
	S: Stream<Item = &'a ServerName> + Send + 'a,
{
	self.queue_and_dispatch_servers(servers, SendingEvent::Pdu(*pdu_id))
		.await
}

#[implement(Service)]
async fn queue_and_dispatch_servers<'a, S>(&self, servers: S, event: SendingEvent) -> Result
where
	S: Stream<Item = &'a ServerName> + Send + 'a,
{
	let requests: Vec<_> = servers
		.map(|server| (event.clone(), Destination::Federation(server.to_owned())))
		.collect()
		.await;

	let _cork = self.db.db.cork();
	let keys = self
		.db
		.queue_requests(requests.iter().map(|(event, dest)| (event, dest)));

	requests
		.into_iter()
		.zip(keys)
		.try_for_each(|((event, dest), queue_id)| self.dispatch(Msg { dest, event, queue_id }))
}

/// Queue an EDU for delivery to a server.
///
/// The row is durable and the shard owning the destination is woken.
#[implement(Service)]
#[tracing::instrument(skip(self, server, serialized), level = "debug")]
pub fn send_edu_server(&self, server: &ServerName, serialized: EduBuf) -> Result {
	let dest = Destination::Federation(server.to_owned());
	let event = SendingEvent::Edu(serialized);
	let _cork = self.db.db.cork();

	self.queue_and_dispatch(dest, event)
}

/// Queue an EDU for delivery to every remote server in a room.
///
/// The fan-out is one durable row per server, dispatched under a single cork.
#[implement(Service)]
#[tracing::instrument(skip(self, room_id, serialized), level = "debug")]
pub async fn send_edu_room(&self, room_id: &RoomId, serialized: EduBuf) -> Result {
	let servers = self
		.services
		.state_cache
		.remote_room_servers(room_id);

	self.send_edu_servers(servers, serialized).await
}

/// Queue an EDU for delivery to each of the given servers.
///
/// The fan-out is one durable row per server, dispatched under a single cork.
#[implement(Service)]
#[tracing::instrument(skip(self, servers, serialized), level = "debug")]
pub async fn send_edu_servers<'a, S>(&self, servers: S, serialized: EduBuf) -> Result
where
	S: Stream<Item = &'a ServerName> + Send + 'a,
{
	self.queue_and_dispatch_servers(servers, SendingEvent::Edu(serialized))
		.await
}

/// Queue an EDU for every appservice interested in a room.
///
/// An appservice is interested when it receives ephemeral events and the room
/// is in its namespace, it is present in the room, or one of the room's local
/// aliases matches. The serializer writes `EphemeralData`, not a federation
/// `Edu`, once per matching appservice.
#[implement(Service)]
// Stream::filter names one future type independent of the item borrow, which
// an async closure's future would capture.
#[expect(closure_returning_async_block)]
#[tracing::instrument(skip(self, serializer), level = "debug")]
pub async fn send_edu_room_appservices<'a, F>(&self, room_id: &RoomId, serializer: F) -> Result
where
	F: Fn(&mut dyn Write) -> Result + Send + 'a,
	&'a F: Send + Sync,
{
	self.services
		.appservice
		.read()
		.await
		.values()
		.stream()
		.filter(|&appservice| async move {
			if !appservice.registration.receive_ephemeral {
				return false;
			}

			if appservice.rooms.is_match(room_id.as_str()) {
				return true;
			}

			if self
				.services
				.state_cache
				.appservice_in_room(room_id, appservice)
				.await
			{
				return true;
			}

			self.services
				.alias
				.local_aliases_for_room(room_id)
				.ready_any(|room_alias| appservice.aliases.is_match(room_alias.as_str()))
				.await
		})
		.map(Ok)
		.ready_try_for_each(|appservice| {
			let mut buf = EduBuf::new(); // serializer out-param

			serializer(&mut buf)?;
			self.send_edu_appservice(appservice.registration.id.clone(), buf)
				.log_err()
				.ok();

			Ok(())
		})
		.await
}

/// Queue an EDU for delivery to a specific appservice.
///
/// The row is durable and the shard owning the destination is woken.
#[implement(Service)]
#[tracing::instrument(skip(self, serialized), level = "debug")]
pub fn send_edu_appservice(&self, appservice_id: String, serialized: EduBuf) -> Result {
	let dest = Destination::Appservice(appservice_id);
	let event = SendingEvent::Edu(serialized);
	let _cork = self.db.db.cork();

	self.queue_and_dispatch(dest, event)
}

/// Wake the sender for every remote server in a room.
///
/// A flush is not queued as a row; it only prompts the shard to compose a
/// transaction from whatever is pending.
#[implement(Service)]
#[tracing::instrument(skip(self, room_id), level = "debug")]
pub async fn flush_room(&self, room_id: &RoomId) -> Result {
	let servers = self
		.services
		.state_cache
		.remote_room_servers(room_id);

	self.flush_servers(servers).await
}

/// Wake the sender for each of the given servers.
///
/// A flush is not queued as a row; it only prompts the shard to compose a
/// transaction from whatever is pending.
#[implement(Service)]
#[tracing::instrument(skip(self, servers), level = "debug")]
pub async fn flush_servers<'a, S>(&self, servers: S) -> Result
where
	S: Stream<Item = &'a ServerName> + Send + 'a,
{
	servers
		.map(ToOwned::to_owned)
		.map(Destination::Federation)
		.map(Ok)
		.ready_try_for_each(|dest| self.dispatch_flush(dest))
		.await
}

#[implement(Service)]
fn dispatch_flush(&self, dest: Destination) -> Result {
	self.dispatch(Msg {
		dest,
		event: SendingEvent::Flush,
		queue_id: Vec::new(),
	})
}

/// Wake the sender for an appservice.
///
/// A flush is not queued as a row; it only prompts the shard to compose a
/// transaction from whatever is pending.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub fn flush_appservice(&self, appservice_id: String) -> Result {
	self.dispatch_flush(Destination::Appservice(appservice_id))
}

/// Wake the sender for a federation peer that has proven reachable.
///
/// Reachability comes from inbound activity or an operator reset. The flush
/// is dispatched only when the peer was in its failure bucket, and the return
/// value reports whether it was.
#[implement(Service)]
#[tracing::instrument(
	level = "debug",
	skip(self),
	fields(
		%server,
	),
)]
pub async fn notify_peer_alive(&self, server: &ServerName) -> bool {
	let sad = self
		.services
		.federation
		.note_peer_alive(server)
		.await;

	if sad {
		self.dispatch_flush(Destination::Federation(server.to_owned()))
			.log_err()
			.ok();
	}

	sad
}

/// Clean up queued sending event data.
///
/// Accepts either an appservice ID alone, after its registration is removed,
/// or a user ID with a push key, after the pusher is deleted; any other
/// combination is ignored with a warning.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn cleanup_events(
	&self,
	appservice_id: Option<&str>,
	user_id: Option<&UserId>,
	push_key: Option<&str>,
) -> Result {
	let dest = match (appservice_id, user_id, push_key) {
		| (None, Some(user_id), Some(push_key)) =>
			Destination::Push(user_id.to_owned(), push_key.to_owned()),
		| (Some(appservice_id), None, None) => Destination::Appservice(appservice_id.to_owned()),
		| _ => {
			debug_warn!("cleanup_events called with too many or too few arguments");
			return Ok(());
		},
	};

	self.db.delete_all_requests_for(&dest).await;

	Ok(())
}
