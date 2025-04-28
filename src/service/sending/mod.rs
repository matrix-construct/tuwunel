mod appservice;
mod data;
mod dest;
mod sender;

use std::{
	fmt::Debug,
	hash::{DefaultHasher, Hash, Hasher},
	iter::once,
	sync::Arc,
};

use async_trait::async_trait;
use futures::{FutureExt, Stream, StreamExt};
use ruma::{
	RoomId, ServerName, UserId,
	api::{OutgoingRequest, appservice::Registration},
};
use tokio::{task, task::JoinSet};
use tuwunel_core::{
	Result, Server, debug, debug_warn, err, error,
	smallvec::SmallVec,
	utils::{ReadyExt, TryReadyExt, available_parallelism, math::usize_from_u64_truncated},
	warn,
};

use self::data::Data;
pub use self::{
	dest::Destination,
	sender::{EDU_LIMIT, PDU_LIMIT},
};
use crate::{
	Dep, account_data, client, federation, globals, presence, pusher, rooms,
	rooms::timeline::RawPduId, users,
};

pub struct Service {
	pub db: Data,
	server: Arc<Server>,
	services: Services,
	channels: Vec<(loole::Sender<Msg>, loole::Receiver<Msg>)>,
}

struct Services {
	client: Dep<client::Service>,
	globals: Dep<globals::Service>,
	state: Dep<rooms::state::Service>,
	state_cache: Dep<rooms::state_cache::Service>,
	user: Dep<rooms::user::Service>,
	users: Dep<users::Service>,
	presence: Dep<presence::Service>,
	read_receipt: Dep<rooms::read_receipt::Service>,
	timeline: Dep<rooms::timeline::Service>,
	account_data: Dep<account_data::Service>,
	appservice: Dep<crate::appservice::Service>,
	pusher: Dep<pusher::Service>,
	federation: Dep<federation::Service>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Msg {
	dest: Destination,
	event: SendingEvent,
	queue_id: Vec<u8>,
}

#[allow(clippy::module_name_repetitions)]
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SendingEvent {
	Pdu(RawPduId), // pduid
	Edu(EduBuf),   // edu json
	Flush,         // none
}

pub type EduBuf = SmallVec<[u8; EDU_BUF_CAP]>;
pub type EduVec = SmallVec<[EduBuf; EDU_VEC_CAP]>;

const EDU_BUF_CAP: usize = 128;
const EDU_VEC_CAP: usize = 1;

#[async_trait]
impl crate::Service for Service {
	fn build(args: crate::Args<'_>) -> Result<Arc<Self>> {
		let num_senders = num_senders(&args);
		Ok(Arc::new(Self {
			db: Data::new(&args),
			server: args.server.clone(),
			services: Services {
				client: args.depend::<client::Service>("client"),
				globals: args.depend::<globals::Service>("globals"),
				state: args.depend::<rooms::state::Service>("rooms::state"),
				state_cache: args.depend::<rooms::state_cache::Service>("rooms::state_cache"),
				user: args.depend::<rooms::user::Service>("rooms::user"),
				users: args.depend::<users::Service>("users"),
				presence: args.depend::<presence::Service>("presence"),
				read_receipt: args.depend::<rooms::read_receipt::Service>("rooms::read_receipt"),
				timeline: args.depend::<rooms::timeline::Service>("rooms::timeline"),
				account_data: args.depend::<account_data::Service>("account_data"),
				appservice: args.depend::<crate::appservice::Service>("appservice"),
				pusher: args.depend::<pusher::Service>("pusher"),
				federation: args.depend::<federation::Service>("federation"),
			},
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

	fn interrupt(&self) {
		for (sender, _) in &self.channels {
			if !sender.is_closed() {
				sender.close();
			}
		}
	}

	fn name(&self) -> &str {
		crate::service::make_name(std::module_path!())
	}

	fn unconstrained(&self) -> bool {
		true
	}
}

impl Service {
	#[tracing::instrument(skip(self, pdu_id, user, pushkey), level = "debug")]
	pub fn send_pdu_push(&self, pdu_id: &RawPduId, user: &UserId, pushkey: String) -> Result {
		let dest = Destination::Push(user.to_owned(), pushkey);
		let event = SendingEvent::Pdu(*pdu_id);
		let _cork = self.db.db.cork();
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

	#[tracing::instrument(skip(self, room_id, serialized), level = "debug")]
	pub async fn send_edu_room(&self, room_id: &RoomId, serialized: EduBuf) -> Result {
		let servers = self
			.services
			.state_cache
			.room_servers(room_id)
			.ready_filter(|server_name| !self.services.globals.server_is_ours(server_name));

		self.send_edu_servers(servers, serialized).await
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
	pub async fn flush_room(&self, room_id: &RoomId) -> Result<()> {
		let servers = self
			.services
			.state_cache
			.room_servers(room_id)
			.ready_filter(|server_name| !self.services.globals.server_is_ours(server_name));

		self.flush_servers(servers).await
	}

	#[tracing::instrument(skip(self, servers), level = "debug")]
	pub async fn flush_servers<'a, S>(&self, servers: S) -> Result<()>
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

	/// Sends a request to a federation server
	#[inline]
	pub async fn send_federation_request<T>(
		&self,
		dest: &ServerName,
		request: T,
	) -> Result<T::IncomingResponse>
	where
		T: OutgoingRequest + Debug + Send,
	{
		self.services
			.federation
			.execute(dest, request)
			.await
	}

	/// Like send_federation_request() but with a very large timeout
	#[inline]
	pub async fn send_synapse_request<T>(
		&self,
		dest: &ServerName,
		request: T,
	) -> Result<T::IncomingResponse>
	where
		T: OutgoingRequest + Debug + Send,
	{
		self.services
			.federation
			.execute_synapse(dest, request)
			.await
	}

	/// Sends a request to an appservice
	///
	/// Only returns None if there is no url specified in the appservice
	/// registration file
	pub async fn send_appservice_request<T>(
		&self,
		registration: Registration,
		request: T,
	) -> Result<Option<T::IncomingResponse>>
	where
		T: OutgoingRequest + Debug + Send,
	{
		let client = &self.services.client.appservice;
		appservice::send_request(client, registration, request).await
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
