use super::*;

impl Service {
	#[tracing::instrument(
		name = "select",
		level = "debug",
		skip_all,
		fields(
			?dest,
			new_events = %new_events.len(),
		),
	)]
	pub(super) async fn select_events(
		&self,
		dest: &Destination,
		new_events: Vec<QueueItem>, // Events we want to send: event and full key
		statuses: &mut CurTransactionStatus,
	) -> Result<Option<Vec<SendingEvent>>> {
		let retry_action = if matches!(dest, Destination::Appservice(_))
			&& new_events
				.iter()
				.any(|(_, event)| matches!(event, SendingEvent::Flush))
		{
			RetryAction::Force
		} else {
			RetryAction::None
		};

		let (allow, retry) = self
			.select_events_current(dest, statuses, retry_action)
			.await?;

		// Nothing can be done for this remote, bail out.
		if !allow {
			return Ok(None);
		}

		let mut events = Vec::new();

		// Must retry any previous transaction for this remote.
		if retry {
			self.db
				.active_requests_for(dest)
				.ready_for_each(|(_, e)| events.push(e))
				.await;

			if !events.is_empty() {
				return Ok(Some(events));
			}
		}

		// Compose the next transaction
		let _cork = self.db.db.cork();
		self.db
			.retain_queued(new_events)
			.ready_for_each(|item| {
				self.db.mark_as_active(once(&item));
				if !matches!(&item.1, SendingEvent::Flush) {
					events.push(item.1);
				}
			})
			.await;

		// Add EDU's into the transaction
		if let Destination::Federation(server_name) = dest {
			let budget_used = events
				.iter()
				.filter(|event| matches!(event, SendingEvent::Edu(_)))
				.count();

			if let Ok(select_edus) = self.select_edus(server_name, budget_used).await {
				events.extend(select_edus.into_iter().map(SendingEvent::Edu));
			}
		}

		Ok(Some(events))
	}

	pub(super) async fn select_events_current(
		&self,
		dest: &Destination,
		statuses: &mut CurTransactionStatus,
		retry_action: RetryAction,
	) -> Result<(bool, bool)> {
		// peer_status gates federation only; appservice and push fall through.
		if let Destination::Federation(server) = dest {
			let should_attempt = self
				.services
				.federation
				.should_attempt(server)
				.await;

			if matches!(should_attempt, ShouldAttempt::No { .. }) {
				return Ok((false, false));
			}
		}

		let (mut allow, mut retry) = (true, false);
		statuses
			.entry(dest.clone())
			.and_modify(|e| match e {
				| TransactionStatus::Running | TransactionStatus::RunningForceRetry => {
					allow = false; // already running
					if matches!(retry_action, RetryAction::Force) {
						*e = TransactionStatus::RunningForceRetry;
					}
				},
				| TransactionStatus::Failed(tries, time) => {
					// Push backoff: hold off until the exponential window elapses.
					let min = self.server.config.sender_timeout;
					let max = self.server.config.sender_retry_backoff_limit;
					let remaining =
						exponential_backoff_remaining_secs(min, max, time.elapsed(), *tries);

					trace!(
						?dest,
						tries = *tries,
						?remaining,
						"Push destination remains in backoff",
					);

					if remaining.is_some() {
						allow = false;
					} else {
						retry = true;
						*e = TransactionStatus::Retrying(*tries);
					}
				},
				| TransactionStatus::Retrying(_) if matches!(dest, Destination::Push(..)) => {
					allow = false; // push retry already in flight
				},
				| TransactionStatus::Pending | TransactionStatus::Retrying(_) => {
					// Promote to Running so a concurrent select does not double-send.
					retry = true;
					*e = TransactionStatus::Running;
				},
			})
			.or_insert(TransactionStatus::Running);

		Ok((allow, retry))
	}

	#[tracing::instrument(name = "edus", level = "debug", skip_all)]
	pub(super) async fn select_edus(
		&self,
		server_name: &ServerName,
		budget_used: usize,
	) -> Result<EduVec> {
		// selection window
		let since = self.db.get_latest_educount(server_name).await;
		let since_upper = self.services.globals.current_count();

		// Nothing new since the last window: skip the scan and the watermark.
		if since == since_upper {
			return Ok(EduVec::new());
		}

		let batch = (since, since_upper);
		debug_assert!(batch.0 <= batch.1, "since range must not be negative");

		let events_len = AtomicUsize::new(budget_used);
		let max_edu_count = AtomicU64::new(since);
		let device_changes =
			self.select_edus_device_changes(server_name, batch, &max_edu_count, &events_len);

		let receipts = self
			.server
			.config
			.allow_outgoing_read_receipts
			.then_async(|| {
				self.select_edus_receipts(server_name, batch, &max_edu_count, &events_len)
			});

		let presence = self
			.server
			.config
			.allow_outgoing_presence
			.then_async(|| {
				self.select_edus_presence(server_name, batch, &max_edu_count, &events_len)
			});

		let (device_changes, receipts, presence) =
			join3(device_changes, receipts, presence).await;

		let receipts = receipts.unwrap_or_default();
		let mut events = device_changes.shipped;

		events.extend(receipts.shipped);

		// Presence rides last and is excluded from the durable prefix because
		// its content is compose-time-relative and regenerates fresh.
		let durable_len = events.len();

		events.extend(presence.flatten());
		debug_assert!(
			budget_used.saturating_add(events.len()) <= EDU_LIMIT,
			"exceeded edus limit"
		);

		// EDUs past the budget become queued rows drained by later transactions.
		let overflow: Vec<SendingEvent> = device_changes
			.overflow
			.into_iter()
			.chain(receipts.overflow)
			.map(SendingEvent::Edu)
			.collect();

		if !overflow.is_empty() {
			let dest = Destination::Federation(server_name.to_owned());
			self.db
				.queue_requests(overflow.iter().map(|event| (event, &dest)));
		}

		// Persist the durable prefix so a failed or restarted transaction
		// replays it; the ACK deletes these active rows.
		if durable_len > 0 {
			self.db
				.persist_active_edus(server_name, &events[..durable_len]);
		}

		let last_count = max_edu_count.load(Ordering::Acquire);
		if last_count > since {
			self.db
				.set_latest_educount(server_name, last_count);
		}

		Ok(events)
	}

	/// Look for device changes
	#[tracing::instrument(
		name = "device_changes",
		level = "trace",
		skip(self, server_name, max_edu_count, events_len)
	)]
	pub(super) async fn select_edus_device_changes(
		&self,
		server_name: &ServerName,
		since: (u64, u64),
		max_edu_count: &AtomicU64,
		events_len: &AtomicUsize,
	) -> Selected {
		let mut selected = Selected::default();
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

				// Past the budget these rows overflow to the queue; replay is
				// benign because the placeholder content is user-id-only.
				if !selected.overflow.is_empty()
					|| events_len.fetch_add(1, Ordering::Relaxed) >= EDU_LIMIT
				{
					selected.overflow.push(buf);
				} else {
					selected.shipped.push(buf);
				}
			}
		}

		selected
	}

	/// Look for read receipts in this room
	///
	/// MSC3771 lets a user emit multiple receipts in the same EDU window, one
	/// per thread context. The federation EDU shape allows only one
	/// `ReceiptData` per `(room, user)` slot, so a user with N parallel
	/// thread receipts ships across N parallel `Edu::Receipt` buffers within
	/// the same transaction. Each buffer is shape-compliant; receivers
	/// process them as independent receipt EDUs and our storage keeps each
	/// thread distinct.
	#[tracing::instrument(
		name = "receipts",
		level = "trace",
		skip(self, server_name, max_edu_count, events_len)
	)]
	pub(super) async fn select_edus_receipts(
		&self,
		server_name: &ServerName,
		since: (u64, u64),
		max_edu_count: &AtomicU64,
		events_len: &AtomicUsize,
	) -> Selected {
		let num = AtomicUsize::new(0);
		let by_room: RoomReceipts = self
			.services
			.state_cache
			.server_rooms(server_name)
			.map(ToOwned::to_owned)
			.broad_filter_map(async |room_id| {
				let ranked = self
					.select_edus_receipts_room(&room_id, since, max_edu_count, &num)
					.await;

				ranked
					.is_empty()
					.is_false()
					.then_some((room_id, ranked))
			})
			.collect()
			.boxed()
			.await;

		let max_rank = by_room
			.iter()
			.map(|(_, maps)| maps.len())
			.max()
			.unwrap_or(0);

		let pivot_rank = |rank: usize| -> Option<BTreeMap<OwnedRoomId, ReceiptMap>> {
			let receipts: BTreeMap<_, _> = by_room
				.iter()
				.filter_map(|(room_id, maps)| {
					maps.get(rank)
						.cloned()
						.map(|map| (room_id.clone(), map))
				})
				.collect();

			receipts.is_empty().is_false().then_some(receipts)
		};

		let serialize_edu = |receipts: BTreeMap<OwnedRoomId, ReceiptMap>| -> EduBuf {
			let mut buf = EduBuf::new();
			serde_json::to_writer(&mut buf, &Edu::Receipt(ReceiptContent { receipts }))
				.expect("Failed to serialize Receipt EDU to JSON vec");

			buf
		};

		// Ranks reserve from the shared budget in order; those past the cap
		// overflow to the queue instead of truncating the tail.
		let mut selected = Selected::default();
		for receipts in (0..max_rank).filter_map(pivot_rank) {
			if !selected.overflow.is_empty()
				|| events_len.fetch_add(1, Ordering::Relaxed) >= EDU_LIMIT
			{
				selected.overflow.push(serialize_edu(receipts));
			} else {
				selected.shipped.push(serialize_edu(receipts));
			}
		}

		selected
	}

	/// Look for read receipts in this room.
	///
	/// Returns a per-rank vector of [`ReceiptMap`]s. Each user's receipts in
	/// the window (one per thread context, count-ordered) are placed into
	/// successive ranks, so rank 0 carries each user's earliest receipt,
	/// rank 1 the next, and so on. The receipt-limit budget bounds distinct
	/// users only; subsequent thread receipts for an already-counted user do
	/// not consume additional budget.
	#[tracing::instrument(
		name = "receipts",
		level = "trace",
		skip(self, since, max_edu_count)
	)]
	pub(super) async fn select_edus_receipts_room(
		&self,
		room_id: &RoomId,
		since: (u64, u64),
		max_edu_count: &AtomicU64,
		num: &AtomicUsize,
	) -> RankedReceipts {
		let receipts =
			self.services
				.read_receipt
				.readreceipts_since(room_id, since.0, Some(since.1));

		pin_mut!(receipts);
		let mut by_user = BTreeMap::<OwnedUserId, UserReceipts>::new();
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

			let receipt_data = ReceiptData { data: receipt, event_ids: vec![event_id] };

			match by_user.entry(user_id.to_owned()) {
				| Entry::Vacant(slot) => {
					slot.insert(SmallVec::from_buf([receipt_data]));
					let num = num.fetch_add(1, Ordering::Relaxed);
					if num >= SELECT_RECEIPT_LIMIT {
						break;
					}
				},
				| Entry::Occupied(mut slot) => {
					slot.get_mut().push(receipt_data);
				},
			}
		}

		// Pivot per-user count-ordered receipts into rank-major
		// `RankedReceipts`. Rank 0 carries each user's earliest receipt in
		// the window, rank 1 the next, and so on.
		by_user
			.into_iter()
			.fold(RankedReceipts::new(), |mut acc, (user_id, receipts)| {
				for (rank, receipt_data) in receipts.into_iter().enumerate() {
					if rank >= acc.len() {
						acc.push(ReceiptMap { read: BTreeMap::new() });
					}

					acc[rank]
						.read
						.insert(user_id.clone(), receipt_data);
				}

				acc
			})
	}

	/// Look for presence
	#[tracing::instrument(
		name = "presence",
		level = "trace",
		skip(self, server_name, max_edu_count, events_len)
	)]
	pub(super) async fn select_edus_presence(
		&self,
		server_name: &ServerName,
		since: (u64, u64),
		max_edu_count: &AtomicU64,
		events_len: &AtomicUsize,
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

		// A budget trip drops presence, which self-heals on the next transition.
		if events_len.fetch_add(1, Ordering::Relaxed) >= EDU_LIMIT {
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
}
