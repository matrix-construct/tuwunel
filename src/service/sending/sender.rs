use std::{
	collections::{BTreeMap, HashMap, HashSet},
	fmt::Debug,
	sync::{
		Arc,
		atomic::{AtomicU64, AtomicUsize, Ordering},
	},
	time::{Duration, Instant},
};

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use futures::{
	FutureExt, StreamExt, TryFutureExt,
	future::{BoxFuture, join3, try_join3},
	pin_mut,
	stream::FuturesUnordered,
};
use ruma::{
	MilliSecondsSinceUnixEpoch, OwnedRoomId, OwnedServerName, OwnedUserId, RoomId, ServerName,
	UserId,
	api::{
		appservice::event::push_events::v1::EphemeralData,
		federation::transactions::{
			edu::{
				DeviceListUpdateContent, Edu, PresenceContent, PresenceUpdate, ReceiptContent,
				ReceiptData, ReceiptMap,
			},
			send_transaction_message,
		},
	},
	device_id,
	events::{
		AnySyncEphemeralRoomEvent, GlobalAccountDataEventType, push_rules::PushRulesEvent,
		receipt::ReceiptType,
	},
	presence::PresenceState,
	push,
	serde::Raw,
	uint,
};
use tuwunel_core::{
	Error, Event, Result, debug, err, error, extract_variant,
	result::LogErr,
	trace,
	utils::{
		BoolExt, ReadyExt, calculate_hash, continue_exponential_backoff_secs,
		future::TryExtExt,
		stream::{BroadbandExt, IterStream, WidebandExt},
	},
	warn,
};

use super::{Destination, EduBuf, EduVec, Msg, SendingEvent, Service, data::QueueItem};
use crate::rooms::timeline::RawPduId;

#[derive(Debug)]
enum TransactionStatus {
	Running,
	Failed(u32, Instant), // number of times failed, time of last failure
	Retrying(u32),        // number of times failed
}

type SendingError = (Destination, Error);
type SendingResult = Result<Destination, SendingError>;
type SendingFuture<'a> = BoxFuture<'a, SendingResult>;
type SendingFutures<'a> = FuturesUnordered<SendingFuture<'a>>;
type CurTransactionStatus = HashMap<Destination, TransactionStatus>;

const SELECT_PRESENCE_LIMIT: usize = 256;
const SELECT_RECEIPT_LIMIT: usize = 256;
const SELECT_EDU_LIMIT: usize = EDU_LIMIT - 2;
const DEQUEUE_LIMIT: usize = 48;

pub const PDU_LIMIT: usize = 50;
pub const EDU_LIMIT: usize = 100;

impl Service {
	#[tracing::instrument(skip(self), level = "debug")]
	pub(super) async fn sender(self: Arc<Self>, id: usize) -> Result {
		let mut statuses: CurTransactionStatus = CurTransactionStatus::new();
		let mut futures: SendingFutures<'_> = FuturesUnordered::new();

		self.startup_netburst(id, &mut futures, &mut statuses)
			.boxed()
			.await;

		self.work_loop(id, &mut futures, &mut statuses)
			.await;

		if !futures.is_empty() {
			self.finish_responses(&mut futures).boxed().await;
		}

		Ok(())
	}

	#[tracing::instrument(
		name = "work",
		level = "trace"
		skip_all,
		fields(
			futures = %futures.len(),
			statuses = %statuses.len(),
		),
	)]
	async fn work_loop<'a>(
		&'a self,
		id: usize,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
	) {
		let receiver = self
			.channels
			.get(id)
			.map(|(_, receiver)| receiver.clone())
			.expect("Missing channel for sender worker");

		while !receiver.is_closed() {
			tokio::select! {
				Some(response) = futures.next() => {
					self.handle_response(response, futures, statuses).await;
				},
				request = receiver.recv_async() => match request {
					Ok(request) => self.handle_request(request, futures, statuses).await,
					Err(_) => return,
				},
			}
		}
	}

	#[tracing::instrument(name = "response", level = "debug", skip_all)]
	async fn handle_response<'a>(
		&'a self,
		response: SendingResult,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
	) {
		match response {
			| Err((dest, e)) => Self::handle_response_err(dest, statuses, &e),
			| Ok(dest) =>
				self.handle_response_ok(&dest, futures, statuses)
					.await,
		}
	}

	fn handle_response_err(dest: Destination, statuses: &mut CurTransactionStatus, e: &Error) {
		debug!(dest = ?dest, "{e:?}");
		statuses.entry(dest).and_modify(|e| {
			*e = match e {
				| TransactionStatus::Running => TransactionStatus::Failed(1, Instant::now()),

				| &mut TransactionStatus::Retrying(ref n) =>
					TransactionStatus::Failed(n.saturating_add(1), Instant::now()),

				| TransactionStatus::Failed(..) => {
					panic!("Request that was not even running failed?!")
				},
			}
		});
	}

	#[expect(clippy::needless_pass_by_ref_mut)]
	async fn handle_response_ok<'a>(
		&'a self,
		dest: &Destination,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
	) {
		let _cork = self.db.db.cork();
		self.db.delete_all_active_requests_for(dest).await;

		// Find events that have been added since starting the last request
		let new_events = self
			.db
			.queued_requests(dest)
			.take(DEQUEUE_LIMIT)
			.collect::<Vec<_>>()
			.await;

		// Insert any pdus we found
		if !new_events.is_empty() {
			self.db.mark_as_active(new_events.iter());

			let new_events_vec = new_events
				.into_iter()
				.map(|(_, event)| event)
				.collect();

			futures.push(self.send_events(dest.clone(), new_events_vec));
		} else {
			statuses.remove(dest);
		}
	}

	#[expect(clippy::needless_pass_by_ref_mut)]
	#[tracing::instrument(name = "request", level = "debug", skip_all)]
	async fn handle_request<'a>(
		&'a self,
		msg: Msg,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
	) {
		let iv = vec![(msg.queue_id, msg.event)];
		if let Ok(Some(events)) = self.select_events(&msg.dest, iv, statuses).await {
			if !events.is_empty() {
				futures.push(self.send_events(msg.dest, events));
			} else {
				statuses.remove(&msg.dest);
			}
		}
	}

	#[tracing::instrument(
		name = "finish",
		level = "info",
		skip_all,
		fields(futures = %futures.len()),
	)]
	async fn finish_responses<'a>(&'a self, futures: &mut SendingFutures<'a>) {
		use tokio::{
			select,
			time::{Instant, sleep_until},
		};

		let timeout = self.server.config.sender_shutdown_timeout;
		let timeout = Duration::from_secs(timeout);
		let now = Instant::now();
		let deadline = now.checked_add(timeout).unwrap_or(now);
		loop {
			trace!("Waiting for {} requests to complete...", futures.len());
			select! {
				() = sleep_until(deadline) => return,
				response = futures.next() => match response {
					Some(Ok(dest)) => self.db.delete_all_active_requests_for(&dest).await,
					Some(_) => continue,
					None => return,
				},
			}
		}
	}

	#[tracing::instrument(
		name = "netburst",
		level = "debug",
		skip_all,
		fields(futures = %futures.len()),
	)]
	#[expect(clippy::needless_pass_by_ref_mut)]
	async fn startup_netburst<'a>(
		&'a self,
		id: usize,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
	) {
		let keep =
			usize::try_from(self.server.config.startup_netburst_keep).unwrap_or(usize::MAX);

		let mut txns = HashMap::<Destination, Vec<SendingEvent>>::new();
		let active = self.db.active_requests();

		pin_mut!(active);
		while let Some((key, event, dest)) = active.next().await {
			if self.shard_id(&dest) != id {
				continue;
			}

			let entry = txns.entry(dest.clone()).or_default();
			if self.server.config.startup_netburst_keep >= 0 && entry.len() >= keep {
				warn!("Dropping unsent event {dest:?} {:?}", String::from_utf8_lossy(&key));
				self.db.delete_active_request(&key);
			} else {
				entry.push(event);
			}
		}

		for (dest, events) in txns {
			if self.server.config.startup_netburst && !events.is_empty() {
				statuses.insert(dest.clone(), TransactionStatus::Running);
				futures.push(self.send_events(dest.clone(), events));
			}
		}
	}

	#[tracing::instrument(
		name = "select",,
		level = "debug",
		skip_all,
		fields(
			?dest,
			new_events = %new_events.len(),
		)
	)]
	async fn select_events(
		&self,
		dest: &Destination,
		new_events: Vec<QueueItem>, // Events we want to send: event and full key
		statuses: &mut CurTransactionStatus,
	) -> Result<Option<Vec<SendingEvent>>> {
		let (allow, retry) = self.select_events_current(dest, statuses)?;

		// Nothing can be done for this remote, bail out.
		if !allow {
			return Ok(None);
		}

		let _cork = self.db.db.cork();
		let mut events = Vec::new();

		// Must retry any previous transaction for this remote.
		if retry {
			self.db
				.active_requests_for(dest)
				.ready_for_each(|(_, e)| events.push(e))
				.await;

			return Ok(Some(events));
		}

		// Compose the next transaction
		let _cork = self.db.db.cork();
		if !new_events.is_empty() {
			self.db.mark_as_active(new_events.iter());
			for (_, e) in new_events {
				events.push(e);
			}
		}

		// Add EDU's into the transaction
		if let Destination::Federation(server_name) = dest
			&& let Ok((select_edus, last_count)) = self.select_edus(server_name).await
		{
			debug_assert!(select_edus.len() <= EDU_LIMIT, "exceeded edus limit");
			let select_edus = select_edus.into_iter().map(SendingEvent::Edu);

			events.extend(select_edus);
			self.db
				.set_latest_educount(server_name, last_count);
		}

		Ok(Some(events))
	}

	fn select_events_current(
		&self,
		dest: &Destination,
		statuses: &mut CurTransactionStatus,
	) -> Result<(bool, bool)> {
		let (mut allow, mut retry) = (true, false);
		statuses
			.entry(dest.clone()) // TODO: can we avoid cloning?
			.and_modify(|e| match e {
				TransactionStatus::Failed(tries, time) => {
					// Fail if a request has failed recently (exponential backoff)
					let min = self.server.config.sender_timeout;
					let max = self.server.config.sender_retry_backoff_limit;
					if continue_exponential_backoff_secs(min, max, time.elapsed(), *tries)
						&& !matches!(dest, Destination::Appservice(_))
					{
						allow = false;
					} else {
						retry = true;
						*e = TransactionStatus::Retrying(*tries);
					}
				},
				TransactionStatus::Running | TransactionStatus::Retrying(_) => {
					allow = false; // already running
				},
			})
			.or_insert(TransactionStatus::Running);

		Ok((allow, retry))
	}

	#[tracing::instrument(
		name = "edus",,
		level = "debug",
		skip_all,
	)]
	async fn select_edus(&self, server_name: &ServerName) -> Result<(EduVec, u64)> {
		// selection window
		let since = self.db.get_latest_educount(server_name).await;
		let since_upper = self.services.globals.current_count();
		let batch = (since, since_upper);
		debug_assert!(batch.0 <= batch.1, "since range must not be negative");

		let events_len = AtomicUsize::default();
		let max_edu_count = AtomicU64::new(since);

		let device_changes =
			self.select_edus_device_changes(server_name, batch, &max_edu_count, &events_len);

		let receipts = self
			.server
			.config
			.allow_outgoing_read_receipts
			.then_async(|| self.select_edus_receipts(server_name, batch, &max_edu_count));

		let presence = self
			.server
			.config
			.allow_outgoing_presence
			.then_async(|| self.select_edus_presence(server_name, batch, &max_edu_count));

		let (device_changes, receipts, presence) =
			join3(device_changes, receipts, presence).await;

		let mut events = device_changes;
		events.extend(presence.into_iter().flatten());
		events.extend(receipts.into_iter().flatten());

		Ok((events, max_edu_count.load(Ordering::Acquire)))
	}

	/// Look for device changes
	#[tracing::instrument(
		name = "device_changes",
		level = "trace",
		skip(self, server_name, max_edu_count)
	)]
	async fn select_edus_device_changes(
		&self,
		server_name: &ServerName,
		since: (u64, u64),
		max_edu_count: &AtomicU64,
		events_len: &AtomicUsize,
	) -> EduVec {
		let mut events = EduVec::new();
		let server_rooms = self
			.services
			.state_cache
			.server_rooms(server_name);

		pin_mut!(server_rooms);
		let mut device_list_changes = HashSet::<OwnedUserId>::new();
		while let Some(room_id) = server_rooms.next().await {
			let keys_changed = self
				.services
				.users
				.room_keys_changed(room_id, since.0, Some(since.1))
				.ready_filter(|(user_id, _)| self.services.globals.user_is_local(user_id));

			pin_mut!(keys_changed);
			while let Some((user_id, count)) = keys_changed.next().await {
				debug_assert!(count <= since.1, "exceeds upper-bound");

				max_edu_count.fetch_max(count, Ordering::Relaxed);
				if !device_list_changes.insert(user_id.into()) {
					continue;
				}

				// Empty prev id forces synapse to resync; because synapse resyncs,
				// we can just insert placeholder data
				let edu = Edu::DeviceListUpdate(DeviceListUpdateContent {
					user_id: user_id.into(),
					device_id: device_id!("placeholder").to_owned(),
					device_display_name: Some("Placeholder".to_owned()),
					stream_id: uint!(1),
					prev_id: Vec::new(),
					deleted: None,
					keys: None,
				});

				let mut buf = EduBuf::new();
				serde_json::to_writer(&mut buf, &edu)
					.expect("failed to serialize device list update to JSON");

				events.push(buf);
				if events_len.fetch_add(1, Ordering::Relaxed) >= SELECT_EDU_LIMIT - 1 {
					return events;
				}
			}
		}

		events
	}

	/// Look for read receipts in this room
	#[tracing::instrument(
		name = "receipts",
		level = "trace",
		skip(self, server_name, max_edu_count)
	)]
	async fn select_edus_receipts(
		&self,
		server_name: &ServerName,
		since: (u64, u64),
		max_edu_count: &AtomicU64,
	) -> Option<EduBuf> {
		let num = AtomicUsize::new(0);
		let receipts: BTreeMap<OwnedRoomId, ReceiptMap> = self
			.services
			.state_cache
			.server_rooms(server_name)
			.map(ToOwned::to_owned)
			.broad_filter_map(async |room_id| {
				let receipt_map = self
					.select_edus_receipts_room(&room_id, since, max_edu_count, &num)
					.await;

				receipt_map
					.read
					.is_empty()
					.eq(&false)
					.then_some((room_id, receipt_map))
			})
			.collect()
			.boxed()
			.await;

		if receipts.is_empty() {
			return None;
		}

		let receipt_content = Edu::Receipt(ReceiptContent { receipts });

		let mut buf = EduBuf::new();
		serde_json::to_writer(&mut buf, &receipt_content)
			.expect("Failed to serialize Receipt EDU to JSON vec");

		Some(buf)
	}

	/// Look for read receipts in this room
	#[tracing::instrument(
		name = "receipts",
		level = "trace",
		skip(self, since, max_edu_count)
	)]
	async fn select_edus_receipts_room(
		&self,
		room_id: &RoomId,
		since: (u64, u64),
		max_edu_count: &AtomicU64,
		num: &AtomicUsize,
	) -> ReceiptMap {
		let receipts =
			self.services
				.read_receipt
				.readreceipts_since(room_id, since.0, Some(since.1));

		pin_mut!(receipts);
		let mut read = BTreeMap::<OwnedUserId, ReceiptData>::new();
		while let Some((user_id, count, read_receipt)) = receipts.next().await {
			debug_assert!(count <= since.1, "exceeds upper-bound");

			max_edu_count.fetch_max(count, Ordering::Relaxed);
			if !self.services.globals.user_is_local(user_id) {
				continue;
			}

			let Ok(event) = serde_json::from_str(read_receipt.json().get()) else {
				error!(?user_id, ?count, ?read_receipt, "Invalid edu event in read_receipts.");
				continue;
			};

			let AnySyncEphemeralRoomEvent::Receipt(r) = event else {
				error!(?user_id, ?count, ?event, "Invalid event type in read_receipts");
				continue;
			};

			let (event_id, mut receipt) = r
				.content
				.0
				.into_iter()
				.next()
				.expect("we only use one event per read receipt");

			let receipt = receipt
				.remove(&ReceiptType::Read)
				.expect("our read receipts always set this")
				.remove(user_id)
				.expect("our read receipts always have the user here");

			let receipt_data = ReceiptData {
				data: receipt,
				event_ids: vec![event_id.clone()],
			};

			if read
				.insert(user_id.to_owned(), receipt_data)
				.is_none()
			{
				let num = num.fetch_add(1, Ordering::Relaxed);
				if num >= SELECT_RECEIPT_LIMIT {
					break;
				}
			}
		}

		ReceiptMap { read }
	}

	/// Look for presence
	#[tracing::instrument(
		name = "presence",
		level = "trace",
		skip(self, server_name, max_edu_count)
	)]
	async fn select_edus_presence(
		&self,
		server_name: &ServerName,
		since: (u64, u64),
		max_edu_count: &AtomicU64,
	) -> Option<EduBuf> {
		let presence_since = self
			.services
			.presence
			.presence_since(since.0, Some(since.1));

		pin_mut!(presence_since);
		let mut presence_updates = HashMap::<OwnedUserId, PresenceUpdate>::new();
		while let Some((user_id, count, presence_bytes)) = presence_since.next().await {
			debug_assert!(count <= since.1, "exceeded upper-bound");

			max_edu_count.fetch_max(count, Ordering::Relaxed);
			if !self.services.globals.user_is_local(user_id) {
				continue;
			}

			if !self
				.services
				.state_cache
				.server_sees_user(server_name, user_id)
				.await
			{
				continue;
			}

			let Ok(presence_event) = self
				.services
				.presence
				.from_json_bytes_to_event(presence_bytes, user_id)
				.await
				.log_err()
			else {
				continue;
			};

			let update = PresenceUpdate {
				user_id: user_id.into(),
				presence: presence_event.content.presence,
				currently_active: presence_event
					.content
					.currently_active
					.unwrap_or(false),
				status_msg: presence_event.content.status_msg,
				last_active_ago: presence_event
					.content
					.last_active_ago
					.unwrap_or_else(|| uint!(0)),
			};

			presence_updates.insert(user_id.into(), update);
			if presence_updates.len() >= SELECT_PRESENCE_LIMIT {
				break;
			}
		}

		if presence_updates.is_empty() {
			return None;
		}

		let presence_content = Edu::Presence(PresenceContent {
			push: presence_updates.into_values().collect(),
		});

		let mut buf = EduBuf::new();
		serde_json::to_writer(&mut buf, &presence_content)
			.expect("failed to serialize Presence EDU to JSON");

		Some(buf)
	}

	fn send_events(&self, dest: Destination, events: Vec<SendingEvent>) -> SendingFuture<'_> {
		debug_assert!(!events.is_empty(), "sending empty transaction");
		match dest {
			| Destination::Federation(server) => self
				.send_events_dest_federation(server, events)
				.boxed(),
			| Destination::Appservice(id) => self
				.send_events_dest_appservice(id, events)
				.boxed(),
			| Destination::Push(user_id, pushkey) => self
				.send_events_dest_push(user_id, pushkey, events)
				.boxed(),
		}
	}

	#[tracing::instrument(
		name = "appservice",
		level = "debug",
		skip(self, events),
		fields(
			events = %events.len(),
		),
	)]
	async fn send_events_dest_appservice(
		&self,
		id: String,
		events: Vec<SendingEvent>,
	) -> SendingResult {
		let Some(appservice) = self
			.services
			.appservice
			.get_registration(&id)
			.await
		else {
			return Err((
				Destination::Appservice(id.clone()),
				err!(Database(warn!(?id, "Missing appservice registration"))),
			));
		};

		let mut pdu_jsons = Vec::with_capacity(
			events
				.iter()
				.filter(|event| matches!(event, SendingEvent::Pdu(_)))
				.count(),
		);
		let mut edu_jsons: Vec<Raw<EphemeralData>> = Vec::with_capacity(
			events
				.iter()
				.filter(|event| matches!(event, SendingEvent::Edu(_)))
				.count(),
		);
		for event in &events {
			match event {
				| SendingEvent::Pdu(pdu_id) => {
					if let Ok(pdu) = self
						.services
						.timeline
						.get_pdu_from_id(pdu_id)
						.await
					{
						pdu_jsons.push(pdu.to_format());
					}
				},
				| SendingEvent::Edu(edu) => {
					if appservice.receive_ephemeral
						&& let Ok(edu) =
							serde_json::from_slice(edu).and_then(|edu| Raw::new(&edu))
					{
						edu_jsons.push(edu);
					}
				},
				| SendingEvent::Flush => {}, // flush only; no new content
			}
		}

		let txn_hash = calculate_hash(events.iter().filter_map(|e| match e {
			| SendingEvent::Edu(b) => Some(b.as_ref()),
			| SendingEvent::Pdu(b) => Some(b.as_ref()),
			| SendingEvent::Flush => None,
		}));

		let txn_id = &*URL_SAFE_NO_PAD.encode(txn_hash);

		//debug_assert!(pdu_jsons.len() + edu_jsons.len() > 0, "sending empty
		// transaction");
		match self
			.services
			.appservice
			.send_request(appservice, ruma::api::appservice::event::push_events::v1::Request {
				txn_id: txn_id.into(),
				events: pdu_jsons,
				ephemeral: edu_jsons,
				to_device: Vec::new(), // TODO
			})
			.await
		{
			| Ok(_) => Ok(Destination::Appservice(id)),
			| Err(e) => Err((Destination::Appservice(id), e)),
		}
	}

	#[tracing::instrument(
		name = "push",
		level = "info",
		skip(self, events),
		fields(
			events = %events.len(),
		),
	)]
	async fn send_events_dest_push(
		&self,
		user_id: OwnedUserId,
		pushkey: String,
		events: Vec<SendingEvent>,
	) -> SendingResult {
		let suppressed = self.pushing_suppressed(&user_id).map(Ok);

		let pusher = self
			.services
			.pusher
			.get_pusher(&user_id, &pushkey)
			.map_err(|_| {
				(
					Destination::Push(user_id.clone(), pushkey.clone()),
					err!(Database(error!(?user_id, ?pushkey, "Missing pusher"))),
				)
			});

		let rules_for_user = self
			.services
			.account_data
			.get_global::<PushRulesEvent>(&user_id, GlobalAccountDataEventType::PushRules)
			.map(|ev| {
				ev.map_or_else(
					|_| push::Ruleset::server_default(&user_id),
					|ev| ev.content.global,
				)
			})
			.map(Ok);

		let (pusher, rules_for_user, suppressed) =
			try_join3(pusher, rules_for_user, suppressed).await?;

		if suppressed {
			let queued = self
				.enqueue_suppressed_push_events(&user_id, &pushkey, &events)
				.await;
			debug!(
				?user_id,
				pushkey,
				queued,
				events = events.len(),
				"Push suppressed; queued events"
			);
			return Ok(Destination::Push(user_id, pushkey));
		}

		self.schedule_flush_suppressed_for_pushkey(
			user_id.clone(),
			pushkey.clone(),
			"non-suppressed push",
		);

		let _sent = events
			.iter()
			.stream()
			.ready_filter_map(|event| extract_variant!(event, SendingEvent::Pdu))
			.wide_filter_map(|pdu_id| {
				self.services
					.timeline
					.get_pdu_from_id(pdu_id)
					.ok()
			})
			.ready_filter(|pdu| !pdu.is_redacted())
			.wide_filter_map(async |pdu| {
				self.services
					.pusher
					.send_push_notice(&user_id, &pusher, &rules_for_user, &pdu)
					.await
					.map_err(|e| (Destination::Push(user_id.clone(), pushkey.clone()), e))
					.ok()
			})
			.count()
			.await;

		Ok(Destination::Push(user_id, pushkey))
	}

	pub fn schedule_flush_suppressed_for_pushkey(
		&self,
		user_id: OwnedUserId,
		pushkey: String,
		reason: &'static str,
	) {
		let sending = self.services.sending.clone();
		let runtime = self.server.runtime();
		runtime.spawn(async move {
			sending
				.flush_suppressed_for_pushkey(user_id, pushkey, reason)
				.await;
		});
	}

	pub fn schedule_flush_suppressed_for_user(&self, user_id: OwnedUserId, reason: &'static str) {
		let sending = self.services.sending.clone();
		let runtime = self.server.runtime();
		runtime.spawn(async move {
			sending
				.flush_suppressed_for_user(user_id, reason)
				.await;
		});
	}

	async fn enqueue_suppressed_push_events(
		&self,
		user_id: &UserId,
		pushkey: &str,
		events: &[SendingEvent],
	) -> usize {
		let mut queued = 0_usize;
		for event in events {
			let SendingEvent::Pdu(pdu_id) = event else {
				continue;
			};

			let Ok(pdu) = self
				.services
				.timeline
				.get_pdu_from_id(pdu_id)
				.await
			else {
				debug!(?user_id, ?pdu_id, "Suppressing push but PDU is missing");
				continue;
			};

			if pdu.is_redacted() {
				trace!(?user_id, ?pdu_id, "Suppressing push for redacted PDU");
				continue;
			}

			if self.services.pusher.queue_suppressed_push(
				user_id,
				pushkey,
				pdu.room_id(),
				*pdu_id,
			) {
				queued = queued.saturating_add(1);
			}
		}

		queued
	}

	async fn flush_suppressed_rooms(
		&self,
		user_id: &UserId,
		pushkey: &str,
		pusher: &ruma::api::client::push::Pusher,
		rules_for_user: &push::Ruleset,
		rooms: Vec<(OwnedRoomId, Vec<RawPduId>)>,
		reason: &'static str,
	) {
		if rooms.is_empty() {
			return;
		}

		let mut sent = 0_usize;
		debug!(?user_id, pushkey, rooms = rooms.len(), "Flushing suppressed pushes ({reason})");

		for (room_id, pdu_ids) in rooms {
			let unread = self
				.services
				.pusher
				.notification_count(user_id, &room_id)
				.await;
			if unread == 0 {
				trace!(?user_id, ?room_id, "Skipping suppressed push flush: no unread");
				continue;
			}

			for pdu_id in pdu_ids {
				let Ok(pdu) = self
					.services
					.timeline
					.get_pdu_from_id(&pdu_id)
					.await
				else {
					debug!(?user_id, ?pdu_id, "Suppressed PDU missing during flush");
					continue;
				};

				if pdu.is_redacted() {
					trace!(?user_id, ?pdu_id, "Suppressed PDU redacted during flush");
					continue;
				}

				if let Err(error) = self
					.services
					.pusher
					.send_push_notice(user_id, pusher, rules_for_user, &pdu)
					.await
				{
					let requeued = self
						.services
						.pusher
						.queue_suppressed_push(user_id, pushkey, &room_id, pdu_id);
					warn!(
						?user_id,
						?room_id,
						?error,
						requeued,
						"Failed to send suppressed push notification"
					);
				} else {
					sent = sent.saturating_add(1);
				}
			}
		}

		debug!(?user_id, pushkey, sent, "Flushed suppressed push notifications");
	}

	async fn flush_suppressed_for_pushkey(
		&self,
		user_id: OwnedUserId,
		pushkey: String,
		reason: &'static str,
	) {
		let suppressed = self
			.services
			.pusher
			.take_suppressed_for_pushkey(&user_id, &pushkey);
		if suppressed.is_empty() {
			return;
		}

		let pusher = match self
			.services
			.pusher
			.get_pusher(&user_id, &pushkey)
			.await
		{
			| Ok(pusher) => pusher,
			| Err(error) => {
				warn!(?user_id, pushkey, ?error, "Missing pusher for suppressed flush");
				return;
			},
		};

		let rules_for_user = match self
			.services
			.account_data
			.get_global::<PushRulesEvent>(&user_id, GlobalAccountDataEventType::PushRules)
			.await
		{
			| Ok(ev) => ev.content.global,
			| Err(_) => push::Ruleset::server_default(&user_id),
		};

		self.flush_suppressed_rooms(
			&user_id,
			&pushkey,
			&pusher,
			&rules_for_user,
			suppressed,
			reason,
		)
		.await;
	}

	pub async fn flush_suppressed_for_user(&self, user_id: OwnedUserId, reason: &'static str) {
		let suppressed = self
			.services
			.pusher
			.take_suppressed_for_user(&user_id);
		if suppressed.is_empty() {
			return;
		}

		let rules_for_user = match self
			.services
			.account_data
			.get_global::<PushRulesEvent>(&user_id, GlobalAccountDataEventType::PushRules)
			.await
		{
			| Ok(ev) => ev.content.global,
			| Err(_) => push::Ruleset::server_default(&user_id),
		};

		for (pushkey, rooms) in suppressed {
			let pusher = match self
				.services
				.pusher
				.get_pusher(&user_id, &pushkey)
				.await
			{
				| Ok(pusher) => pusher,
				| Err(error) => {
					warn!(?user_id, pushkey, ?error, "Missing pusher for suppressed flush");
					continue;
				},
			};

			self.flush_suppressed_rooms(
				&user_id,
				&pushkey,
				&pusher,
				&rules_for_user,
				rooms,
				reason,
			)
			.await;
		}
	}

	// optional suppression: heuristic combining presence age and recent sync
	// activity.
	async fn pushing_suppressed(&self, user_id: &UserId) -> bool {
		if !self.services.config.suppress_push_when_active {
			debug!(?user_id, "push not suppressed: suppress_push_when_active disabled");
			return false;
		}

		let Ok(presence) = self.services.presence.get_presence(user_id).await else {
			debug!(?user_id, "push not suppressed: presence unavailable");
			return false;
		};

		if presence.content.presence != PresenceState::Online {
			debug!(
				?user_id,
				presence = ?presence.content.presence,
				"push not suppressed: presence not online"
			);
			return false;
		}

		let presence_age_ms = presence
			.content
			.last_active_ago
			.map(u64::from)
			.unwrap_or(u64::MAX);

		if presence_age_ms >= 65_000 {
			debug!(?user_id, presence_age_ms, "push not suppressed: presence too old");
			return false;
		}

		let sync_gap_ms = self
			.services
			.presence
			.last_sync_gap_ms(user_id)
			.await;

		let considered_active = sync_gap_ms.is_some_and(|gap| gap < 32_000);

		match sync_gap_ms {
			| Some(gap) if gap < 32_000 => debug!(
				?user_id,
				presence_age_ms,
				sync_gap_ms = gap,
				"suppressing push: active heuristic"
			),
			| Some(gap) => debug!(
				?user_id,
				presence_age_ms,
				sync_gap_ms = gap,
				"push not suppressed: sync gap too large"
			),
			| None => debug!(?user_id, presence_age_ms, "push not suppressed: no recent sync"),
		}

		considered_active
	}

	async fn send_events_dest_federation(
		&self,
		server: OwnedServerName,
		events: Vec<SendingEvent>,
	) -> SendingResult {
		let pdus: Vec<_> = events
			.iter()
			.filter_map(|pdu| match pdu {
				| SendingEvent::Pdu(pdu) => Some(pdu),
				| _ => None,
			})
			.stream()
			.wide_filter_map(|pdu_id| {
				self.services
					.timeline
					.get_pdu_json_from_id(pdu_id)
					.ok()
			})
			.wide_then(|pdu| {
				self.services
					.federation
					.format_pdu_into(pdu, None)
			})
			.collect()
			.await;

		let edus: Vec<Raw<Edu>> = events
			.iter()
			.filter_map(|edu| match edu {
				| SendingEvent::Edu(edu) => Some(edu.as_ref()),
				| _ => None,
			})
			.map(serde_json::from_slice)
			.filter_map(Result::ok)
			.collect();

		if pdus.is_empty() && edus.is_empty() {
			return Ok(Destination::Federation(server));
		}

		let preimage = pdus
			.iter()
			.map(|raw| raw.get().as_bytes())
			.chain(edus.iter().map(|raw| raw.json().get().as_bytes()));

		let txn_hash = calculate_hash(preimage);
		let txn_id = &*URL_SAFE_NO_PAD.encode(txn_hash);
		let request = send_transaction_message::v1::Request {
			transaction_id: txn_id.into(),
			origin: self.server.name.clone(),
			origin_server_ts: MilliSecondsSinceUnixEpoch::now(),
			pdus,
			edus,
		};

		let result = self
			.services
			.federation
			.execute_on(&self.services.client.sender, &server, request)
			.await;

		for (event_id, result) in result.iter().flat_map(|resp| resp.pdus.iter()) {
			if let Err(e) = result {
				warn!(
					%txn_id, %server,
					"error sending PDU {event_id} to remote server: {e:?}"
				);
			}
		}

		match result {
			| Err(error) => Err((Destination::Federation(server), error)),
			| Ok(_) => Ok(Destination::Federation(server)),
		}
	}
}
