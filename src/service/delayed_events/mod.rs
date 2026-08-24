//! Persistent delayed-event scheduling for MSC4140.

use std::{
	collections::HashMap,
	net::IpAddr,
	sync::Arc,
	time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::TryStreamExt;
use http::StatusCode;
use ruma::{
	CanonicalJsonObject, MilliSecondsSinceUnixEpoch, OwnedDeviceId, OwnedRoomId,
	OwnedTransactionId, OwnedUserId, UInt,
	api::error::{ErrorKind, LimitExceededErrorData, RetryAfter},
	events::{AnyTimelineEventContent, TimelineEventType},
	serde::Raw,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tuwunel_core::{
	Err, Result, err,
	matrix::pdu::PduBuilder,
	utils::{rand::string_array, time::now_millis},
};
use tuwunel_database::{Deserialized, Json, Map};

const DELAY_ID_LENGTH: usize = 32;
const RATELIMITER_CAPACITY: usize = 4096;
const RATELIMITER_RATE: f64 = 1.0;
const RATELIMITER_BURST: f64 = 20.0;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct DelayedEvent {
	delay_id: String,
	user_id: OwnedUserId,
	device_id: Option<OwnedDeviceId>,
	room_id: OwnedRoomId,
	event_type: TimelineEventType,
	state_key: Option<String>,
	content: CanonicalJsonObject,
	txn_id: Option<OwnedTransactionId>,
	delay_ms: u64,
	send_at: u64,
	processing: bool,
}

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	delayid_event: Arc<Map>,
	lock: Mutex<()>,
	notify: Notify,
	ratelimiter: std::sync::Mutex<HashMap<IpAddr, (Instant, f64)>>,
}

pub struct ScheduleParams<'a> {
	pub user_id: &'a ruma::UserId,
	pub device_id: Option<&'a ruma::DeviceId>,
	pub room_id: OwnedRoomId,
	pub event_type: TimelineEventType,
	pub state_key: Option<String>,
	pub content: CanonicalJsonObject,
	pub txn_id: Option<OwnedTransactionId>,
	pub delay: Duration,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			delayid_event: args.db["delayid_event"].clone(),
			lock: Mutex::new(()),
			notify: Notify::new(),
			ratelimiter: std::sync::Mutex::new(HashMap::new()),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		self.recover_processing().await?;

		loop {
			self.process_due().await?;
			let wait = self.next_wait().await?;

			tokio::select! {
				() = self.services.server.until_shutdown() => return Ok(()),
				() = self.notify.notified() => {},
				() = tokio::time::sleep(wait) => {},
			}
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Schedule an event for later delivery and return its server-generated id.
	pub async fn schedule(&self, params: ScheduleParams<'_>) -> Result<String> {
		let ScheduleParams {
			user_id,
			device_id,
			room_id,
			event_type,
			state_key,
			content,
			txn_id,
			delay,
		} = params;
		let max_delay = self
			.services
			.config
			.max_event_delay_duration
			.saturating_mul(1000);
		let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);

		if max_delay == 0 || self.services.config.max_delayed_events_per_user == 0 {
			return Err!(Request(Forbidden("Delayed events are disabled.")));
		}
		if delay_ms == 0 {
			return Err!(Request(InvalidParam(
				"The delayed event timeout must be greater than zero."
			)));
		}
		if delay_ms > max_delay {
			return Err!(Request(Forbidden(
				"The delayed event timeout exceeds the configured maximum."
			)));
		}

		let _lock = self.lock.lock().await;
		if let Some(txn_id) = txn_id.as_ref()
			&& let Ok(response) = self
				.services
				.transaction_ids
				.existing_txnid(user_id, device_id, txn_id)
				.await
		{
			return std::str::from_utf8(&response)
				.ok()
				.filter(|delay_id| delay_id.len() == DELAY_ID_LENGTH)
				.map(ToOwned::to_owned)
				.ok_or_else(|| {
					err!(Request(InvalidParam(
						"Tried to use txn_id already used for an incompatible endpoint."
					)))
				});
		}

		let events = self.records().await?;
		let scheduled = events
			.iter()
			.filter(|(_, event)| event.user_id == user_id)
			.count();
		if scheduled >= self.services.config.max_delayed_events_per_user {
			let now = now_millis();
			let retry_after = events
				.iter()
				.filter(|(_, event)| event.user_id == user_id)
				.map(|(_, event)| event.send_at)
				.min()
				.map(|send_at| send_at.saturating_sub(now).div_ceil(1000).max(1))
				.map(Duration::from_secs)
				.map(RetryAfter::Delay);

			return Err(tuwunel_core::Error::Request(
				ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after }),
				"The maximum number of delayed events has been reached.".into(),
				StatusCode::TOO_MANY_REQUESTS,
			));
		}

		let delay_id = string_array::<DELAY_ID_LENGTH>().to_string();
		let event = DelayedEvent {
			delay_id: delay_id.clone(),
			user_id: user_id.to_owned(),
			device_id: device_id.map(ToOwned::to_owned),
			room_id,
			event_type,
			state_key,
			content,
			txn_id: txn_id.clone(),
			delay_ms,
			send_at: now_millis().saturating_add(delay_ms),
			processing: false,
		};

		self.delayid_event.put(&delay_id, Json(event));
		if let Some(txn_id) = txn_id.as_ref() {
			self.services.transaction_ids.add_txnid(
				user_id,
				device_id,
				txn_id,
				delay_id.as_bytes(),
			);
		}
		self.notify.notify_one();
		Ok(delay_id)
	}

	/// Update a scheduled event. Unauthenticated management requests are
	/// rate-limited by client IP; authenticated requests are tied to the owner.
	pub async fn update(
		&self,
		delay_id: &str,
		action: &str,
		owner: Option<&ruma::UserId>,
		client: IpAddr,
	) -> Result {
		if owner.is_none() {
			self.check_rate_limit(client)?;
		}

		let event = {
			let _lock = self.lock.lock().await;
			let mut event = self
				.delayid_event
				.get(delay_id)
				.await
				.deserialized::<Json<DelayedEvent>>()
				.map(|Json(event)| event)
				.map_err(|_| err!(Request(NotFound("Delayed event not found."))))?;
			if owner.is_some_and(|owner| event.user_id != owner) {
				return Err!(Request(NotFound("Delayed event not found.")));
			}

			if event.processing {
				return Err!(Request(NotFound("Delayed event is already being processed.")));
			}

			match action {
				| "cancel" => {
					self.delayid_event.remove(&delay_id);
					return Ok(());
				},
				| "restart" => {
					event.send_at = now_millis().saturating_add(event.delay_ms);
					self.delayid_event.put(delay_id, Json(event));
					self.notify.notify_one();
					return Ok(());
				},
				| "send" => {
					event.processing = true;
					self.delayid_event.put(delay_id, Json(&event));
				},
				| _ => return Err!(Request(InvalidParam("Unknown delayed event action."))),
			}

			event
		};

		match self.send_event(&event).await {
			| Ok(_) => {
				self.delayid_event.remove(&delay_id);
				Ok(())
			},
			| Err(error) => {
				let mut event = event;
				event.processing = false;
				self.delayid_event.put(delay_id, Json(event));
				self.notify.notify_one();
				Err(error)
			},
		}
	}

	/// Return all scheduled events owned by a user.
	pub async fn list(
		&self,
		user_id: &ruma::UserId,
	) -> Result<Vec<ruma::api::client::delayed_events::DelayedEventData>> {
		self.records()
			.await?
			.into_iter()
			.filter(|(_, event)| event.user_id == user_id)
			.map(|(_, event)| event_data(event))
			.collect()
	}

	/// Return one scheduled event owned by a user.
	pub async fn get(
		&self,
		delay_id: &str,
		user_id: &ruma::UserId,
	) -> Result<ruma::api::client::delayed_events::DelayedEventData> {
		let event = self
			.delayid_event
			.get(delay_id)
			.await
			.deserialized::<Json<DelayedEvent>>()
			.map(|Json(event)| event)
			.map_err(|_| err!(Request(NotFound("Delayed event not found."))))?;
		if event.user_id != user_id {
			return Err!(Request(NotFound("Delayed event not found.")));
		}

		event_data(event)
	}

	async fn recover_processing(&self) -> Result {
		let _lock = self.lock.lock().await;
		for (delay_id, mut event) in self.records().await? {
			if event.processing {
				event.processing = false;
				event.send_at = now_millis();
				self.delayid_event.put(&delay_id, Json(event));
			}
		}
		Ok(())
	}

	async fn process_due(&self) -> Result {
		let now = now_millis();
		let due = {
			let _lock = self.lock.lock().await;
			let mut due = Vec::new();
			for (delay_id, mut event) in self.records().await? {
				if event.send_at <= now && !event.processing {
					event.processing = true;
					self.delayid_event.put(&delay_id, Json(&event));
					due.push(event);
				}
			}
			due
		};

		for event in due {
			if let Err(error) = self.send_event(&event).await {
				tracing::warn!(delay_id = %event.delay_id, ?error, "Failed to send delayed event");
			}
			self.delayid_event.remove(&event.delay_id);
		}

		Ok(())
	}

	async fn send_event(&self, event: &DelayedEvent) -> Result<ruma::OwnedEventId> {
		let state_lock = self
			.services
			.state
			.mutex
			.lock(&event.room_id)
			.await;
		let mut unsigned = std::collections::BTreeMap::new();
		unsigned.insert("org.matrix.msc4140.delay_id".to_owned(), event.delay_id.clone().into());
		if let Some(txn_id) = &event.txn_id {
			unsigned.insert("transaction_id".to_owned(), txn_id.to_string().into());
		}

		let event_id = self
			.services
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: event.event_type.clone(),
					content: Raw::new(&event.content)?,
					state_key: event.state_key.clone().map(Into::into),
					unsigned: Some(unsigned),
					..Default::default()
				},
				&event.user_id,
				&event.room_id,
				&state_lock,
			)
			.await?;

		Ok(event_id)
	}

	async fn records(&self) -> Result<Vec<(String, DelayedEvent)>> {
		let mut records: Vec<(String, DelayedEvent)> = self
			.delayid_event
			.stream::<&str, Json<DelayedEvent>>()
			.map_ok(|(delay_id, Json(event))| (delay_id.to_owned(), event))
			.try_collect()
			.await?;

		records.sort_unstable_by(|(left_id, left), (right_id, right)| {
			left.send_at
				.cmp(&right.send_at)
				.then_with(|| left_id.cmp(right_id))
		});

		Ok(records)
	}

	async fn next_wait(&self) -> Result<Duration> {
		let now = now_millis();
		let next = self
			.records()
			.await?
			.into_iter()
			.filter(|(_, event)| !event.processing)
			.map(|(_, event)| event.send_at)
			.min();

		Ok(Duration::from_millis(
			next.map_or(60_000, |send_at| send_at.saturating_sub(now).max(1)),
		))
	}

	fn check_rate_limit(&self, client: IpAddr) -> Result {
		let now = Instant::now();
		let mut ratelimiter = self.ratelimiter.lock()?;
		if ratelimiter.len() >= RATELIMITER_CAPACITY && !ratelimiter.contains_key(&client) {
			ratelimiter.retain(|_, (last, tokens)| {
				now.duration_since(*last)
					.as_secs_f64()
					.mul_add(RATELIMITER_RATE, *tokens)
					< RATELIMITER_BURST
			});
			if ratelimiter.len() >= RATELIMITER_CAPACITY {
				return Err(tuwunel_core::Error::Request(
					ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after: None }),
					"Too many delayed event actions.".into(),
					StatusCode::TOO_MANY_REQUESTS,
				));
			}
		}

		let (last, tokens) = ratelimiter
			.entry(client)
			.or_insert((now, RATELIMITER_BURST));
		let available = now
			.duration_since(*last)
			.as_secs_f64()
			.mul_add(RATELIMITER_RATE, *tokens)
			.min(RATELIMITER_BURST);
		if available < 1.0 {
			return Err(tuwunel_core::Error::Request(
				ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after: None }),
				"Too many delayed event actions.".into(),
				StatusCode::TOO_MANY_REQUESTS,
			));
		}

		*last = now;
		*tokens = available - 1.0;
		Ok(())
	}
}

fn event_data(
	event: DelayedEvent,
) -> Result<ruma::api::client::delayed_events::DelayedEventData> {
	let content: Raw<AnyTimelineEventContent> =
		Raw::from_json_string(serde_json::to_string(&event.content)?)?;
	Ok(ruma::api::client::delayed_events::DelayedEventData::new(
		event.delay_id,
		event.room_id,
		event.event_type,
		event.state_key,
		content,
		Duration::from_millis(event.delay_ms),
		MilliSecondsSinceUnixEpoch(UInt::new_saturating(
			event.send_at.saturating_sub(event.delay_ms),
		)),
	))
}
