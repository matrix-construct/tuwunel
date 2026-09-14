use tracing::instrument;

use super::*;

impl Service {
	pub(super) async fn handle_force_retry<'a>(
		&'a self,
		dest: Destination,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
	) {
		let Ok(Some(events)) = self
			.select_events(&dest, Vec::new(), statuses)
			.await
		else {
			return;
		};

		self.schedule_events(dest, events, futures, statuses);
	}

	pub(super) fn handle_response_err(
		dest: &Destination,
		statuses: &mut CurTransactionStatus,
		e: &Error,
	) -> RetryAction {
		debug!(?dest, "{e:?}");
		// Push backs off locally; federation defers to peer_status, appservice retries.
		let push = matches!(dest, Destination::Push(..));

		let Some(status) = statuses.get_mut(dest) else {
			return RetryAction::None;
		};

		let (tries, retry_action) = match status {
			| TransactionStatus::Pending | TransactionStatus::Running => (1, RetryAction::None),
			| TransactionStatus::RunningForceRetry => (1, RetryAction::Force),
			| TransactionStatus::Failed(n, _) | TransactionStatus::Retrying(n) =>
				(n.saturating_add(1), RetryAction::None),
		};

		*status = if push {
			TransactionStatus::Failed(tries, Instant::now())
		} else {
			TransactionStatus::Retrying(tries)
		};

		retry_action
	}

	#[expect(clippy::needless_pass_by_ref_mut)]
	pub(super) async fn handle_response_ok<'a>(
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

		if !new_events.is_empty() {
			self.db.mark_as_active(new_events.iter());
		}

		let mut events: Vec<SendingEvent> = new_events
			.into_iter()
			.map(|(_, event)| event)
			.collect();

		// Top up with EDUs that accrued while the transaction was in flight.
		if let Destination::Federation(server_name) = dest {
			let budget_used = events
				.iter()
				.filter(|event| matches!(event, SendingEvent::Edu(_)))
				.count();

			if let Ok(select_edus) = self.select_edus(server_name, budget_used).await {
				events.extend(select_edus.into_iter().map(SendingEvent::Edu));
			}
		}

		if events.is_empty() {
			statuses.remove(dest);
		} else {
			if let Some(status) = statuses.get_mut(dest) {
				*status = TransactionStatus::Running;
			}

			futures.push(self.send_events(dest.clone(), events));
		}
	}

	#[expect(
		clippy::needless_pass_by_ref_mut,
		reason = "mutable reference avoids requiring SendingFutures to be Sync"
	)]
	pub(super) fn schedule_events<'a>(
		&'a self,
		dest: Destination,
		events: Vec<SendingEvent>,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
	) {
		if events.is_empty() {
			statuses.remove(&dest);
		} else {
			futures.push(self.send_events(dest, events));
		}
	}

	#[instrument(name = "request", level = "debug", skip_all)]
	pub(super) async fn handle_request<'a>(
		&'a self,
		msg: Msg,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
	) {
		let synthetic_badge =
			msg.queue_id.is_empty() && matches!(&msg.event, SendingEvent::BadgeRefresh);

		let new_events = match (synthetic_badge, statuses.contains_key(&msg.dest)) {
			| (false, _) => vec![(msg.queue_id, msg.event)],
			| (true, true) => Vec::new(),
			| (true, false) =>
				self.db
					.queued_requests(&msg.dest)
					.take(DEQUEUE_LIMIT)
					.collect()
					.await,
		};

		if let Ok(Some(events)) = self
			.select_events(&msg.dest, new_events, statuses)
			.await
		{
			self.schedule_events(msg.dest, events, futures, statuses);
		}
	}

	pub(super) async fn drain_due_wakes<'a>(
		&'a self,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
		wakes: &mut WakeQueue,
	) {
		use tokio::time::Instant;

		let now = Instant::now();
		while wakes
			.peek()
			.is_some_and(|Reverse((due, _))| *due <= now)
		{
			let Reverse((_, dest)) = wakes.pop().expect("peeked entry");
			self.handle_wake(dest, futures, statuses, wakes)
				.await;
		}
	}

	pub(super) async fn handle_wake<'a>(
		&'a self,
		dest: Destination,
		futures: &mut SendingFutures<'a>,
		statuses: &mut CurTransactionStatus,
		wakes: &mut WakeQueue,
	) {
		let status = statuses.get(&dest);

		if matches!(
			status,
			Some(TransactionStatus::Running | TransactionStatus::RunningForceRetry)
		) {
			return;
		}

		if matches!(
			(&dest, status),
			(Destination::Push(..), Some(TransactionStatus::Retrying(_)))
		) {
			trace!(?dest, "Dropping push wake while retry is in flight");
			return;
		}

		if let (Destination::Push(..), Some(remaining)) =
			(&dest, self.push_backoff_remaining(status))
		{
			if wakes
				.iter()
				.any(|Reverse((_, armed_dest))| armed_dest == &dest)
			{
				trace!(?dest, "Dropping stale push wake");
			} else {
				trace!(?dest, ?remaining, "Re-arming early push wake");
				arm_wake_in(wakes, dest, remaining);
			}

			return;
		}

		match dest {
			| Destination::Federation(server) => {
				let should_attempt = self
					.services
					.federation
					.should_attempt(&server)
					.await;

				let dest = Destination::Federation(server);

				match should_attempt {
					| ShouldAttempt::No { earliest_retry } =>
						arm_wake(wakes, dest, earliest_retry),
					| _ => {
						let msg = Msg {
							dest,
							event: SendingEvent::Flush,
							queue_id: Vec::new(),
						};

						self.handle_request(msg, futures, statuses).await;
					},
				}
			},
			| dest @ Destination::Push(..) => {
				self.handle_force_retry(dest, futures, statuses)
					.await;
			},
			| Destination::Appservice(_) => {},
		}
	}

	#[instrument(
		name = "finish",
		level = "info",
		skip_all,
		fields(futures = %futures.len()),
	)]
	pub(super) async fn finish_responses<'a>(&'a self, futures: &mut SendingFutures<'a>) {
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
					Some(_) => {},
					None => return,
				},
			}
		}
	}

	#[instrument(
		name = "netburst",
		level = "debug",
		skip_all,
		fields(futures = %futures.len()),
	)]
	pub(super) async fn startup_netburst<'a>(
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

		txns.into_iter()
			.filter(|(_, events)| !events.is_empty())
			.for_each(|(dest, events)| {
				let status = match self.server.config.startup_netburst {
					| true => TransactionStatus::Running,
					| false => TransactionStatus::Pending,
				};

				statuses.insert(dest.clone(), status);
				if self.server.config.startup_netburst {
					futures.push(self.send_events(dest, events));
				}
			});

		// Active transaction generations must own their queued successors before
		// queued-only badge destinations are woken.
		if !self.server.config.startup_netburst || keep == 0 {
			return;
		}

		let destinations = self
			.db
			.queued_badge_refresh_destinations()
			.ready_filter(|dest| self.shard_id(dest) == id)
			.collect::<HashSet<_>>()
			.await;

		for dest in destinations {
			let msg = Msg {
				dest,
				event: SendingEvent::BadgeRefresh,
				queue_id: Vec::new(),
			};

			self.handle_request(msg, futures, statuses).await;
		}
	}
}
