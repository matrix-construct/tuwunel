use super::*;

impl Service {
	pub(super) fn send_events(
		&self,
		dest: Destination,
		events: Vec<SendingEvent>,
	) -> SendingFuture<'_> {
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
	pub(super) async fn send_events_dest_appservice(
		&self,
		id: String,
		events: Vec<SendingEvent>,
	) -> SendingResult {
		let Some(info) = self
			.services
			.appservice
			.get_registration_info(&id)
			.await
		else {
			//TODO: appservice queue cleanup.
			return Err((
				Destination::Appservice(id.clone()),
				err!(Database(debug_warn!(?id, "Missing appservice registration"))),
			));
		};

		let msc3202 = info.registration.msc3202_transaction_extensions;

		let (pdu_count, edu_count, to_device_count, device_list_count) = events.iter().fold(
			(0_usize, 0_usize, 0_usize, 0_usize),
			|(pdus, edus, to_device, device_list), event| match event {
				| SendingEvent::Pdu(_) => (pdus.saturating_add(1), edus, to_device, device_list),
				| SendingEvent::Edu(_) => (pdus, edus.saturating_add(1), to_device, device_list),
				| SendingEvent::ToDevice(_) =>
					(pdus, edus, to_device.saturating_add(1), device_list),
				| SendingEvent::DeviceListChanged(_) =>
					(pdus, edus, to_device, device_list.saturating_add(1)),
				| SendingEvent::BadgeRefresh | SendingEvent::Flush =>
					(pdus, edus, to_device, device_list),
			},
		);

		let mut pdu_jsons = Vec::with_capacity(pdu_count);
		let mut edu_jsons: Vec<Raw<EphemeralData>> = Vec::with_capacity(edu_count);
		let mut to_device = Vec::with_capacity(to_device_count);
		let mut changed = Vec::with_capacity(device_list_count);

		// MSC3202 one-time-key scope: the appservice sender plus (below) the
		// namespace-matched PDU senders and to-device recipients of this txn.
		let mut otk_users = BTreeSet::new();
		let mut otk_recipients = BTreeSet::new();
		if msc3202 {
			otk_users.insert(info.sender.clone());
		}

		for event in &events {
			match event {
				| SendingEvent::Pdu(pdu_id) => {
					if let Ok(pdu) = self
						.services
						.timeline
						.get_pdu_from_id(pdu_id)
						.await
					{
						if msc3202 && info.is_user_match(pdu.sender()) {
							otk_users.insert(pdu.sender().to_owned());
						}

						pdu_jsons.push(pdu.to_format());
					}
				},
				| SendingEvent::Edu(edu) => {
					if info.registration.receive_ephemeral
						&& let Ok(edu) =
							serde_json::from_slice(edu).and_then(|edu| Raw::new(&edu))
					{
						edu_jsons.push(edu);
					}
				},
				| SendingEvent::ToDevice(buf) => {
					let Some(bytes) = buf.get(TAG_PREFIX_LEN..) else {
						debug_warn!("skipping malformed queued to-device event");
						continue;
					};

					if msc3202
						&& let Ok(recipient) = serde_json::from_slice::<ToDeviceRecipient>(bytes)
					{
						otk_recipients.insert((recipient.to_user_id, recipient.to_device_id));
					}

					if let Ok(raw) = serde_json::from_slice(bytes) {
						to_device.push(raw);
					} else {
						debug_warn!("skipping malformed queued to-device event");
					}
				},
				| SendingEvent::DeviceListChanged(buf) => {
					if msc3202
						&& let Some(bytes) = buf.get(TAG_PREFIX_LEN..)
						&& let Ok(user) = from_utf8(bytes)
						&& let Ok(user_id) = UserId::parse(user)
					{
						changed.push(user_id);
					}
				},
				| SendingEvent::BadgeRefresh | SendingEvent::Flush => {},
			}
		}

		let txn_hash = calculate_hash(events.iter().filter_map(|e| match e {
			| SendingEvent::Edu(b)
			| SendingEvent::ToDevice(b)
			| SendingEvent::DeviceListChanged(b) => Some(b.as_ref()),
			| SendingEvent::Pdu(b) => Some(b.as_ref()),
			| SendingEvent::BadgeRefresh | SendingEvent::Flush => None,
		}));

		let txn_id = &*URL_SAFE_NO_PAD.encode(txn_hash);

		let (device_lists, device_one_time_keys_count, device_unused_fallback_key_types) =
			if msc3202 {
				changed.sort_unstable();
				changed.dedup();

				let (counts, fallbacks) = self
					.msc3202_key_counts(otk_users, otk_recipients)
					.await;

				(DeviceLists { changed, left: Vec::new() }, counts, fallbacks)
			} else {
				(DeviceLists::new(), OtkCounts::new(), FallbackTypes::new())
			};

		if pdu_jsons.is_empty()
			&& edu_jsons.is_empty()
			&& to_device.is_empty()
			&& device_lists.is_empty()
			&& device_one_time_keys_count.is_empty()
			&& device_unused_fallback_key_types.is_empty()
		{
			return Ok(Destination::Appservice(id));
		}

		match self
			.services
			.appservice
			.send_request(info.registration, PushEventsRequest {
				txn_id: txn_id.into(),
				events: pdu_jsons,
				ephemeral: edu_jsons,
				to_device,
				device_lists,
				device_one_time_keys_count,
				device_unused_fallback_key_types,
			})
			.await
		{
			| Ok(_) => Ok(Destination::Appservice(id)),
			| Err(e) => Err((Destination::Appservice(id), e)),
		}
	}

	/// MSC3202 one-time-key counts and unused fallback key types over every
	/// device of `users` plus the specific `recipients`. Recomputed per build
	/// rather than snapshotted, so a retry ships fresh counts.
	pub(super) async fn msc3202_key_counts(
		&self,
		users: BTreeSet<OwnedUserId>,
		recipients: BTreeSet<(OwnedUserId, OwnedDeviceId)>,
	) -> (OtkCounts, FallbackTypes) {
		let mut devices: Devices = users
			.into_iter()
			.stream()
			.broad_then(async |user_id: OwnedUserId| {
				self.services
					.users
					.all_device_ids(&user_id)
					.map(|device_id| (user_id.clone(), device_id.to_owned()))
					.collect()
					.await
			})
			.flat_map(|pairs: Vec<(OwnedUserId, OwnedDeviceId)>| pairs.into_iter().stream())
			.chain(recipients.into_iter().stream())
			.collect()
			.await;

		devices.sort_unstable();
		devices.dedup();

		devices
			.into_iter()
			.stream()
			.broad_then(async |(user_id, device_id): (OwnedUserId, OwnedDeviceId)| {
				let counts = self
					.services
					.users
					.count_one_time_keys(&user_id, &device_id);

				let fallbacks = self
					.services
					.users
					.unused_fallback_key_algorithms(&user_id, &device_id)
					.collect();

				let (counts, fallbacks) = join(counts, fallbacks).await;

				(user_id, device_id, counts, fallbacks)
			})
			.ready_fold(
				(OtkCounts::new(), FallbackTypes::new()),
				|(mut counts, mut fallbacks), (user_id, device_id, otk, fallback)| {
					counts
						.entry(user_id.clone())
						.or_default()
						.insert(device_id.clone(), otk);

					fallbacks
						.entry(user_id)
						.or_default()
						.insert(device_id, fallback);

					(counts, fallbacks)
				},
			)
			.await
	}
}
