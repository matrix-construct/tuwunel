mod data;
mod dest;
mod sender;
#[cfg(test)]
mod tests;

use std::{
	fmt::Debug,
	hash::{DefaultHasher, Hash, Hasher},
	io::Write,
	iter::once,
	pin::pin,
	sync::Arc,
};

use async_trait::async_trait;
use futures::{FutureExt, Stream, StreamExt};
use ruma::{DeviceId, RoomId, ServerName, UserId};
use serde::Serialize;
use tokio::{task, task::JoinSet};
use tuwunel_core::{
	Result, Server, debug, debug_warn, err, error,
	smallvec::SmallVec,
	utils::{
		IterStream, ReadyExt, TryReadyExt, available_parallelism, future::BoolExt,
		math::usize_from_u64_truncated, result::LogErr,
	},
	warn,
};

use self::data::Data;
pub use self::{
	dest::Destination,
	sender::{EDU_LIMIT, PDU_LIMIT},
};
use crate::rooms::timeline::RawPduId;

pub struct Service {
	pub db: Data,
	server: Arc<Server>,
	services: Arc<crate::services::OnceServices>,
	channels: Vec<(loole::Sender<Msg>, loole::Receiver<Msg>)>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Msg {
	dest: Destination,
	event: SendingEvent,
	queue_id: Vec<u8>,
}

#[expect(clippy::module_name_repetitions)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SendingEvent {
	Pdu(RawPduId),             // pduid
	Edu(EduBuf),               // edu json
	ToDevice(EduBuf),          // msc4203 to-device
	DeviceListChanged(EduBuf), // msc3202 device list
	Flush,                     // none
}

pub type EduBuf = SmallVec<[u8; EDU_BUF_CAP]>;
pub type EduVec = SmallVec<[EduBuf; EDU_VEC_CAP]>;

const EDU_BUF_CAP: usize = 128 - 16;
const EDU_VEC_CAP: usize = 1;

/// Leading byte on a queued appservice value selecting a tagged
/// `SendingEvent` variant. Legacy rows self-identify without a tag: `Pdu`
/// values are empty and `Edu` values are `{`-leading json, neither of which
/// can collide with these bytes. The tag and the following count are baked
/// into the owned buffer at construction, so the codec writes it verbatim.
const TAG_TO_DEVICE: u8 = 0x01;
const TAG_DEVICE_LIST_CHANGED: u8 = 0x02;
const TAG_PREFIX_LEN: usize = 1 + size_of::<u64>();

impl SendingEvent {
	/// Bytes written verbatim as the queue row value. `Pdu` keeps its id in
	/// the row key and `Flush` is never persisted, so both are valueless; the
	/// tagged variants own their whole `[tag][count][body]` buffer.
	pub(super) fn value_bytes(&self) -> &[u8] {
		match self {
			| Self::Edu(bytes) | Self::ToDevice(bytes) | Self::DeviceListChanged(bytes) => bytes,
			| Self::Pdu(_) | Self::Flush => &[],
		}
	}
}

/// Wire shape of one `de.sorunome.msc2409.to_device` entry (MSC4203): the
/// stored to-device event flattened with the recipient's identifiers. The
/// ruma `AnyAppserviceToDeviceEvent` deliberately has no `Serialize`, so the
/// send side writes this local struct.
#[derive(Serialize)]
struct AsToDeviceEvent<'a> {
	#[serde(rename = "type")]
	kind: &'a str,
	sender: &'a UserId,
	content: &'a serde_json::Value,
	to_user_id: &'a UserId,
	to_device_id: &'a DeviceId,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		let num_senders = num_senders(args);
		Ok(Arc::new(Self {
			db: Data::new(args),
			server: args.server.clone(),
			services: args.services.clone(),
			channels: (0..num_senders)
				.map(|_| loole::unbounded())
				.collect(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		let mut senders =
			self.channels
				.iter()
				.enumerate()
				.fold(JoinSet::new(), |mut joinset, (id, _)| {
					let self_ = self.clone();
					let worker = self_.sender(id);
					let worker = if self.unconstrained() {
						task::unconstrained(worker).boxed()
					} else {
						worker.boxed()
					};

					let runtime = self.server.runtime();
					let _abort = joinset.spawn_on(worker, runtime);
					joinset
				});

		while let Some(ret) = senders.join_next_with_id().await {
			match ret {
				| Ok((id, _)) => {
					debug!(?id, "sender worker finished");
				},
				| Err(error) => {
					error!(id = ?error.id(), ?error, "sender worker finished");
				},
			}
		}

		Ok(())
	}

	async fn interrupt(&self) {
		for (sender, _) in &self.channels {
			if !sender.is_closed() {
				sender.close();
			}
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }

	fn unconstrained(&self) -> bool { true }
}

impl Service {
	#[tracing::instrument(skip(self, pdu_id, user, pushkey), level = "debug")]
	pub fn send_pdu_push(&self, pdu_id: &RawPduId, user: &UserId, pushkey: String) -> Result {
		let dest = Destination::Push(user.to_owned(), pushkey);
		let event = SendingEvent::Pdu(*pdu_id);
		let _cork = self.db.db.cork();

		self.queue_and_dispatch(dest, event)
	}

	/// Queue one event for delivery to `dest` and wake a sender.
	fn queue_and_dispatch(&self, dest: Destination, event: SendingEvent) -> Result {
		let keys = self.db.queue_requests(once((&event, &dest)));

		self.dispatch(Msg {
			dest,
			event,
			queue_id: keys
				.into_iter()
				.next()
				.expect("request queue key"),
		})
	}

	#[tracing::instrument(skip(self), level = "debug")]
	pub fn send_pdu_appservice(&self, appservice_id: String, pdu_id: RawPduId) -> Result {
		let dest = Destination::Appservice(appservice_id);
		let event = SendingEvent::Pdu(pdu_id);
		let _cork = self.db.db.cork();

		self.queue_and_dispatch(dest, event)
	}

	#[tracing::instrument(skip(self, room_id, pdu_id), level = "debug")]
	pub async fn send_pdu_room(&self, room_id: &RoomId, pdu_id: &RawPduId) -> Result {
		let servers = self
			.services
			.state_cache
			.room_servers(room_id)
			.ready_filter(|server_name| !self.services.globals.server_is_ours(server_name));

		self.send_pdu_servers(servers, pdu_id).await
	}

	#[tracing::instrument(skip(self, servers, pdu_id), level = "debug")]
	pub async fn send_pdu_servers<'a, S>(&self, servers: S, pdu_id: &RawPduId) -> Result
	where
		S: Stream<Item = &'a ServerName> + Send + 'a,
	{
		let requests = servers
			.map(|server| {
				(Destination::Federation(server.into()), SendingEvent::Pdu(pdu_id.to_owned()))
			})
			.collect::<Vec<_>>()
			.await;

		let _cork = self.db.db.cork();
		let keys = self
			.db
			.queue_requests(requests.iter().map(|(o, e)| (e, o)));

		for ((dest, event), queue_id) in requests.into_iter().zip(keys) {
			self.dispatch(Msg { dest, event, queue_id })?;
		}

		Ok(())
	}

	#[tracing::instrument(skip(self, server, serialized), level = "debug")]
	pub fn send_edu_server(&self, server: &ServerName, serialized: EduBuf) -> Result {
		let dest = Destination::Federation(server.to_owned());
		let event = SendingEvent::Edu(serialized);
		let _cork = self.db.db.cork();

		self.queue_and_dispatch(dest, event)
	}

	#[tracing::instrument(skip(self, room_id, serialized), level = "debug")]
	pub async fn send_edu_room(&self, room_id: &RoomId, serialized: EduBuf) -> Result {
		let servers = self
			.services
			.state_cache
			.room_servers(room_id)
			.ready_filter(|server_name| !self.services.globals.server_is_ours(server_name));

		self.send_edu_servers(servers, serialized).await
	}

	/// Queue an EDU for delivery to a specific appservice.
	#[tracing::instrument(skip(self, serialized), level = "debug")]
	pub fn send_edu_appservice(&self, appservice_id: String, serialized: EduBuf) -> Result {
		let dest = Destination::Appservice(appservice_id);
		let event = SendingEvent::Edu(serialized);
		let _cork = self.db.db.cork();

		self.queue_and_dispatch(dest, event)
	}

	/// Sends an EDU to all appservices interested in a room.
	/// The `serialized` data must be in `EphemeralData` format, not federation
	/// `Edu`.
	#[tracing::instrument(skip(self, serializer), level = "debug")]
	pub async fn send_edu_room_appservices<'a, F>(
		&self,
		room_id: &RoomId,
		serializer: F,
	) -> Result
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
			.filter(|&appservice| async {
				if !appservice.registration.receive_ephemeral {
					return false;
				}

				if appservice.rooms.is_match(room_id.as_str()) {
					return true;
				}

				let appservice_in_room = self
					.services
					.state_cache
					.appservice_in_room(room_id, appservice);

				let matching_aliases = self
					.services
					.alias
					.local_aliases_for_room(room_id)
					.ready_any(|room_alias| appservice.aliases.is_match(room_alias.as_str()));

				pin!(appservice_in_room)
					.or(pin!(matching_aliases))
					.await
			})
			.map(Ok)
			.ready_try_for_each(|appservice| {
				let mut buf = EduBuf::new();

				serializer(&mut buf)?;
				self.send_edu_appservice(appservice.registration.id.clone(), buf)
					.log_err()
					.ok();

				Ok(())
			})
			.await
	}

	/// Queue stored to-device events for delivery to interested appservices
	/// (MSC4203). `deliveries` are the concrete recipient devices already
	/// written to the inbox (post-`AllDevices` expansion) paired with their
	/// inbox counts, which uniquify the transaction hash.
	#[tracing::instrument(
		skip(self, deliveries, content),
		level = "debug",
		fields(
			%target_user,
		),
	)]
	pub async fn send_to_device_appservices<'a, I>(
		&self,
		sender: &UserId,
		target_user: &UserId,
		deliveries: I,
		event_type: &str,
		content: &serde_json::Value,
	) -> Result
	where
		I: Iterator<Item = (&'a DeviceId, u64)> + Clone + Send,
	{
		let registrations = self.services.appservice.read().await;
		let _cork = self.db.db.cork();

		let mut payloads: Option<EduVec> = None;
		for info in registrations.values() {
			if !info.is_user_match(target_user) {
				continue;
			}

			let payloads = payloads.get_or_insert_with(|| {
				to_device_payloads(sender, target_user, deliveries.clone(), event_type, content)
			});

			for buf in &*payloads {
				let dest = Destination::Appservice(info.registration.id.clone());
				let event = SendingEvent::ToDevice(buf.clone());

				self.queue_and_dispatch(dest, event)?;
			}
		}

		Ok(())
	}

	#[tracing::instrument(skip(self, servers, serialized), level = "debug")]
	pub async fn send_edu_servers<'a, S>(&self, servers: S, serialized: EduBuf) -> Result
	where
		S: Stream<Item = &'a ServerName> + Send + 'a,
	{
		let requests = servers
			.map(|server| {
				(
					Destination::Federation(server.to_owned()),
					SendingEvent::Edu(serialized.clone()),
				)
			})
			.collect::<Vec<_>>()
			.await;

		let _cork = self.db.db.cork();
		let keys = self
			.db
			.queue_requests(requests.iter().map(|(o, e)| (e, o)));

		for ((dest, event), queue_id) in requests.into_iter().zip(keys) {
			self.dispatch(Msg { dest, event, queue_id })?;
		}

		Ok(())
	}

	#[tracing::instrument(skip(self, room_id), level = "debug")]
	pub async fn flush_room(&self, room_id: &RoomId) -> Result {
		let servers = self
			.services
			.state_cache
			.room_servers(room_id)
			.ready_filter(|server_name| !self.services.globals.server_is_ours(server_name));

		self.flush_servers(servers).await
	}

	#[tracing::instrument(skip(self, servers), level = "debug")]
	pub async fn flush_servers<'a, S>(&self, servers: S) -> Result
	where
		S: Stream<Item = &'a ServerName> + Send + 'a,
	{
		servers
			.map(ToOwned::to_owned)
			.map(Destination::Federation)
			.map(Ok)
			.ready_try_for_each(|dest| {
				self.dispatch(Msg {
					dest,
					event: SendingEvent::Flush,
					queue_id: Vec::<u8>::new(),
				})
			})
			.await
	}

	/// Flushes the sender for a federation peer that has proven reachable via
	/// inbound activity, but only when it was actually in its failure bucket.
	#[tracing::instrument(
		level = "debug",
		skip(self),
		fields(
			%server,
		),
	)]
	pub async fn notify_peer_alive(&self, server: &ServerName) {
		if self
			.services
			.federation
			.note_peer_alive(server)
			.await
		{
			self.dispatch(Msg {
				dest: Destination::Federation(server.to_owned()),
				event: SendingEvent::Flush,
				queue_id: Vec::<u8>::new(),
			})
			.log_err()
			.ok();
		}
	}

	/// Clean up queued sending event data
	///
	/// Used after we remove an appservice registration or a user deletes a push
	/// key
	#[tracing::instrument(skip(self), level = "debug")]
	pub async fn cleanup_events(
		&self,
		appservice_id: Option<&str>,
		user_id: Option<&UserId>,
		push_key: Option<&str>,
	) -> Result {
		match (appservice_id, user_id, push_key) {
			| (None, Some(user_id), Some(push_key)) => {
				self.db
					.delete_all_requests_for(&Destination::Push(
						user_id.to_owned(),
						push_key.to_owned(),
					))
					.await;

				Ok(())
			},
			| (Some(appservice_id), None, None) => {
				self.db
					.delete_all_requests_for(&Destination::Appservice(appservice_id.to_owned()))
					.await;

				Ok(())
			},
			| _ => {
				debug_warn!("cleanup_events called with too many or too few arguments");
				Ok(())
			},
		}
	}

	fn dispatch(&self, msg: Msg) -> Result {
		let shard = self.shard_id(&msg.dest);
		let sender = &self
			.channels
			.get(shard)
			.expect("missing sender worker channels")
			.0;

		debug_assert!(!sender.is_full(), "channel full");
		debug_assert!(!sender.is_closed(), "channel closed");
		sender.send(msg).map_err(|e| err!("{e}"))
	}

	pub(super) fn shard_id(&self, dest: &Destination) -> usize {
		if self.channels.len() <= 1 {
			return 0;
		}

		let mut hash = DefaultHasher::default();
		dest.hash(&mut hash);

		let hash: u64 = hash.finish();
		let hash = usize_from_u64_truncated(hash);

		let chans = self.channels.len().max(1);
		hash.overflowing_rem(chans).0
	}
}

fn to_device_payloads<'a, I>(
	sender: &UserId,
	target_user: &UserId,
	deliveries: I,
	event_type: &str,
	content: &serde_json::Value,
) -> EduVec
where
	I: Iterator<Item = (&'a DeviceId, u64)>,
{
	deliveries
		.map(|(to_device_id, count)| {
			let mut buf = EduBuf::new();
			buf.push(TAG_TO_DEVICE);
			buf.extend_from_slice(&count.to_be_bytes());

			let event = AsToDeviceEvent {
				kind: event_type,
				sender,
				content,
				to_user_id: target_user,
				to_device_id,
			};

			serde_json::to_writer(&mut buf, &event)
				.expect("to-device appservice event serializes");

			buf
		})
		.collect()
}

fn num_senders(args: &crate::Args<'_>) -> usize {
	const MIN_SENDERS: usize = 1;
	// Limit the number of senders to the number of workers threads or number of
	// cores, conservatively.
	let max_senders = args
		.server
		.metrics
		.num_workers()
		.min(available_parallelism());

	// If the user doesn't override the default 0, this is intended to then default
	// to 1 for now as multiple senders is experimental.
	args.server
		.config
		.sender_workers
		.clamp(MIN_SENDERS, max_senders)
}
