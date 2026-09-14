use tracing::instrument;

use super::*;

impl Service {
	#[instrument(
		name = "push",
		level = "info",
		skip(self, events),
		fields(
			events = %events.len(),
		),
	)]
	pub(super) async fn send_events_dest_push(
		&self,
		user_id: OwnedUserId,
		pushkey: String,
		events: Vec<SendingEvent>,
	) -> SendingResult {
		let has_pdu = events
			.iter()
			.any(|event| matches!(event, SendingEvent::Pdu(_)));

		let destination = || Destination::Push(user_id.clone(), pushkey.clone());
		let suppressed = self.pushing_suppressed(&user_id).map(Ok);
		let pusher = self
			.services
			.pusher
			.get_pusher(&user_id, &pushkey)
			.map(|result| match result {
				| Ok(pusher) => Ok(Some(pusher)),
				| Err(error) if error.is_not_found() => {
					error!(%user_id, %pushkey, "Pusher disappeared before delivery");

					Ok(None)
				},
				| Err(error) => Err((destination(), error)),
			});

		let rules_for_user = has_pdu
			.then_async(async || {
				self.services
					.account_data
					.get_global::<PushRulesEvent>(&user_id, GlobalAccountDataEventType::PushRules)
					.await
					.map_or_else(|_| Ruleset::server_default(&user_id), |ev| ev.content.global)
			})
			.map(Ok);

		let (pusher, rules_for_user, suppressed) =
			try_join3(pusher, rules_for_user, suppressed).await?;

		let Some(pusher) = pusher else {
			return Ok(Destination::Push(user_id, pushkey));
		};

		// Reconciliation, not an alert: a suppressed drop strands a stale badge.
		if events.contains(&SendingEvent::BadgeRefresh) {
			let result = self
				.services
				.pusher
				.send_badge_notice(&user_id, &pusher)
				.await;

			match result {
				| Ok(()) => (),
				| Err(error) if is_permanent_push_error(&error) => warn!(
					%user_id,
					%pushkey,
					chain = %error_chain(&error),
					"Dropping a badge push with a permanent local error",
				),
				| Err(error) => return Err((destination(), error)),
			}
		}

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

		let failures = match rules_for_user {
			| None => PushFailures::default(),
			| Some(rules_for_user) =>
				events
					.iter()
					.stream()
					.ready_filter_map(|event| extract_variant!(event, SendingEvent::Pdu))
					.wide_filter_map(async |pdu_id| {
						self.services
							.timeline
							.get_pdu_from_id(pdu_id)
							.map_ok(|pdu| (*pdu_id, pdu))
							.await
							.ok()
					})
					.ready_filter(|(_, pdu)| !pdu.is_redacted())
					.wide_then(async |(pdu_id, pdu)| {
						let result = self
							.services
							.pusher
							.send_push_notice(&user_id, &pusher, &rules_for_user, &pdu)
							.await;

						(pdu_id, result)
					})
					.ready_fold(
						PushFailures::default(),
						|failures, (pdu_id, result)| match result {
							| Ok(()) => failures,
							| Err(error) if is_permanent_push_error(&error) => {
								warn!(
									%user_id,
									%pushkey,
									?pdu_id,
									chain = %error_chain(&error),
									"Dropping a push with a permanent local error",
								);

								failures
							},
							| Err(error) => failures.retain(pdu_id, error),
						},
					)
					.await,
		};

		let PushFailures { ids, error: Some(error) } = failures else {
			return Ok(Destination::Push(user_id, pushkey));
		};

		let dest = Destination::Push(user_id, pushkey);

		events
			.iter()
			.filter_map(|event| extract_variant!(event, SendingEvent::Pdu))
			.filter(|pdu_id| !ids.contains(*pdu_id))
			.for_each(|pdu_id| {
				self.db
					.delete_active_request(&dest.event_key(pdu_id));
			});

		Err((dest, error))
	}
}

#[inline]
fn is_permanent_push_error(error: &Error) -> bool {
	matches!(error, Error::Request(ErrorKind::InvalidParam, ..))
}

impl Service {
	/// Schedule a flush of the pushes suppressed for one pushkey.
	///
	/// The flush runs as a task this service owns, so the caller never waits on
	/// the push gateway.
	pub fn schedule_flush_suppressed_for_pushkey(
		&self,
		user_id: OwnedUserId,
		pushkey: String,
		reason: &'static str,
	) {
		let sending = self.services.sending.clone();

		self.spawn_flush(async move {
			sending
				.flush_suppressed_for_pushkey(user_id, pushkey, reason)
				.await;
		});
	}

	/// Schedule a flush of the pushes suppressed for every pushkey a user owns.
	///
	/// The flush runs as a task this service owns, so the caller never waits on
	/// the push gateway.
	pub fn schedule_flush_suppressed_for_user(&self, user_id: OwnedUserId, reason: &'static str) {
		let sending = self.services.sending.clone();

		self.spawn_flush(async move {
			sending
				.flush_suppressed_for_user(user_id, reason)
				.await;
		});
	}

	pub(super) fn spawn_flush<F>(&self, flush: F)
	where
		F: Future<Output = ()> + Send + 'static,
	{
		// A flush scheduled during shutdown is dropped, not spawned.
		if !self.server.is_running() {
			return;
		}

		let mut flushes = self.flushes.lock().expect("locked");

		reap_flushes(&mut flushes);
		let _abort = flushes.spawn_on(flush, self.server.runtime());
	}

	pub(super) async fn enqueue_suppressed_push_events(
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

	pub(super) async fn flush_suppressed_rooms(
		&self,
		user_id: &UserId,
		pushkey: &str,
		pusher: &Pusher,
		rules_for_user: &Ruleset,
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

	pub(super) async fn flush_suppressed_for_pushkey(
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
			| Err(_) => Ruleset::server_default(&user_id),
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
			| Err(_) => Ruleset::server_default(&user_id),
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
	pub(super) async fn pushing_suppressed(&self, user_id: &UserId) -> bool {
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
}
