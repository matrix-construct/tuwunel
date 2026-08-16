//! Persistent delayed-event scheduling for MSC4140.

use std::{
	collections::{BTreeMap, HashMap},
	net::IpAddr,
	sync::Arc,
	time::{Duration, Instant},
};

use async_trait::async_trait;
use futures::TryStreamExt;
use http::StatusCode;
use ruma::{
	CanonicalJsonObject, CanonicalJsonValue, EventId, MilliSecondsSinceUnixEpoch, OwnedDeviceId,
	OwnedEventId, OwnedRoomId, OwnedTransactionId, OwnedUserId, UInt,
	api::{
		client::{
			delayed_events::{
				get_delayed_event::v1::{DelayedEventData, Finalized},
				update_delayed_event::UpdateAction,
			},
			discovery::get_capabilities::v3::DelayedEventsCapability,
		},
		error::{ErrorKind, LimitExceededErrorData, RetryAfter, StandardErrorBody},
	},
	events::{AnyTimelineEventContent, StateKey, TimelineEventType},
	serde::Raw,
};
use serde::{Deserialize, Serialize};
use tokio::sync::{Mutex, Notify};
use tuwunel_core::{
	Err, Error, Result, err,
	matrix::pdu::PduBuilder,
	utils::{rand::string_array, time::now_millis},
	warn,
};
use tuwunel_database::{Deserialized, Json, Map};

const DELAY_ID_LENGTH: usize = 32;
const FINALISED_RETENTION_MS: u64 = 7 * 24 * 60 * 60 * 1000;
const FINALISED_PER_USER: usize = 1000;
const IDLE_WAIT: Duration = Duration::from_mins(1);
const TXNID_SCOPE: &str = "org.matrix.msc4140.delayed_event";
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
	delay_ms: u64,
	send_at: u64,
	processing: bool,
	#[serde(default)]
	timestamp: Option<MilliSecondsSinceUnixEpoch>,
	#[serde(default)]
	finalised: Option<Finalised>,
}

/// How and when a delayed event stopped being scheduled.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct Finalised {
	ts: u64,
	outcome: Outcome,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
enum Outcome {
	Sent(OwnedEventId),
	Error(StandardErrorBody),
	Cancelled,
}

/// A management action on a scheduled delayed event.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
	Cancel,
	Restart,
	Send,
}

/// What a management action does to a delayed event in its current state.
#[derive(Debug, Eq, PartialEq)]
enum Resolution {
	Proceed,
	AlreadyDone,
	Conflict,
	NotFound,
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
	pub timestamp: Option<MilliSecondsSinceUnixEpoch>,
	pub stable: bool,
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
			self.prune().await?;
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

impl TryFrom<&UpdateAction> for Action {
	type Error = Error;

	fn try_from(action: &UpdateAction) -> Result<Self> {
		match action {
			| UpdateAction::Cancel => Ok(Self::Cancel),
			| UpdateAction::Restart => Ok(Self::Restart),
			| UpdateAction::Send => Ok(Self::Send),
			| _ => Err!(Request(InvalidParam("Unknown delayed event action."))),
		}
	}
}

impl Service {
	/// The configured limits, or none when delayed events are disabled.
	#[must_use]
	pub fn capability(&self) -> Option<DelayedEventsCapability> {
		let config = &self.services.config;
		let max_delay_ms = config
			.max_event_delay_duration
			.saturating_mul(1000);
		let max_scheduled = u64::try_from(config.max_delayed_events_per_user).unwrap_or(u64::MAX);

		(max_delay_ms > 0 && max_scheduled > 0).then(|| {
			DelayedEventsCapability::new(
				Some(UInt::new_saturating(max_delay_ms)),
				Some(UInt::new_saturating(max_scheduled)),
			)
		})
	}

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
			timestamp,
			stable,
		} = params;
		let Some(limits) = self.capability() else {
			return Err!(Request(Forbidden("Delayed events are disabled.")));
		};
		let max_delay = limits.max_delay_ms.map_or(u64::MAX, u64::from);
		let max_scheduled = limits.max_scheduled.map_or(u64::MAX, u64::from);
		let delay_ms = u64::try_from(delay.as_millis()).unwrap_or(u64::MAX);

		if delay_ms == 0 {
			return Err!(Request(InvalidParam(
				"The delayed event timeout must be greater than zero."
			)));
		}
		if delay_ms > max_delay {
			let message = "The delayed event timeout exceeds the configured maximum.";
			return Err(if stable {
				err!(Request(DelayTooLarge("{message}")))
			} else {
				err!(Request(UnstableDelayTooLarge("{message}")))
			});
		}

		self.check_sendable(user_id, &room_id, &event_type, state_key.as_deref())
			.await?;

		let txn_scope = format!("{TXNID_SCOPE}:{event_type}");
		let _lock = self.lock.lock().await;
		if let Some(txn_id) = txn_id.as_ref()
			&& let Ok(response) = self
				.services
				.transaction_ids
				.existing_room_txnid(user_id, device_id, txn_id, &room_id, &txn_scope)
				.await
		{
			return std::str::from_utf8(&response)
				.map(ToOwned::to_owned)
				.map_err(|_| err!(Database("Invalid delayed event transaction response.")));
		}

		let events = self.records().await?;
		let scheduled = || {
			events
				.iter()
				.map(|(_, event)| event)
				.filter(|event| event.user_id == user_id && event.finalised.is_none())
		};
		if u64::try_from(scheduled().count()).unwrap_or(u64::MAX) >= max_scheduled {
			let now = now_millis();
			let retry_after = scheduled()
				.map(|event| event.send_at)
				.min()
				.map(|send_at| send_at.saturating_sub(now).div_ceil(1000).max(1))
				.map(Duration::from_secs)
				.map(RetryAfter::Delay);

			return Err(Error::Request(
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
			delay_ms,
			send_at: now_millis().saturating_add(delay_ms),
			processing: false,
			timestamp,
			finalised: None,
		};

		self.delayid_event
			.raw_put(&delay_id, Json(&event));
		if let Some(txn_id) = txn_id.as_ref() {
			self.services.transaction_ids.add_room_txnid(
				user_id,
				device_id,
				txn_id,
				&event.room_id,
				&txn_scope,
				delay_id.as_bytes(),
			);
		}
		self.notify.notify_one();
		Ok(delay_id)
	}

	/// Update a delayed event. With an owner the request is the owner's own
	/// and finalised events answer as MSC4140 specifies; without one it is a
	/// delegated request, rate-limited by client IP, for which finalised events
	/// no longer exist.
	pub async fn update(
		&self,
		delay_id: &str,
		action: Action,
		owner: Option<&ruma::UserId>,
		client: IpAddr,
	) -> Result {
		if owner.is_none() {
			self.check_rate_limit(client)?;
		}

		let event = {
			let _lock = self.lock.lock().await;
			let mut event = self.record(delay_id).await?;
			if owner.is_some_and(|owner| event.user_id != owner) {
				return Err!(Request(NotFound("Delayed event not found.")));
			}

			let outcome = event.finalised.as_ref().map(|f| &f.outcome);
			match resolve(outcome, action, owner.is_none()) {
				| Resolution::Proceed => {},
				| Resolution::AlreadyDone => return Ok(()),
				| Resolution::NotFound => {
					return Err!(Request(NotFound("Delayed event not found.")));
				},
				| Resolution::Conflict => {
					return Err!(Conflict("Delayed event was already finalised differently."));
				},
			}

			if event.processing {
				return Err!(Request(NotFound("Delayed event is already being processed.")));
			}

			match action {
				| Action::Cancel => {
					self.finalise(event, Outcome::Cancelled);
					return Ok(());
				},
				| Action::Restart => {
					event.send_at = now_millis().saturating_add(event.delay_ms);
					self.delayid_event.raw_put(delay_id, Json(event));
					self.notify.notify_one();
					return Ok(());
				},
				| Action::Send => {
					event.processing = true;
					self.delayid_event.raw_put(delay_id, Json(&event));
				},
			}

			event
		};

		match self.send_event(&event).await {
			| Ok(event_id) => {
				self.finalise(event, Outcome::Sent(event_id));
				Ok(())
			},
			| Err(error) => {
				let mut event = event;
				event.processing = false;
				self.delayid_event.raw_put(delay_id, Json(event));
				self.notify.notify_one();
				Err(error)
			},
		}
	}

	/// Return one delayed event owned by a user, scheduled or finalised.
	pub async fn get(&self, delay_id: &str, user_id: &ruma::UserId) -> Result<DelayedEventData> {
		let event = self.record(delay_id).await?;
		if event.user_id != user_id {
			return Err!(Request(NotFound("Delayed event not found.")));
		}

		event_data(event)
	}

	/// Cancel and forget every delayed event of a deactivated user.
	pub async fn remove_user(&self, user_id: &ruma::UserId) -> Result {
		let _lock = self.lock.lock().await;
		for (delay_id, _) in self
			.records()
			.await?
			.into_iter()
			.filter(|(_, event)| event.user_id == user_id)
		{
			self.delayid_event.remove(&delay_id);
		}

		Ok(())
	}

	async fn check_sendable(
		&self,
		user_id: &ruma::UserId,
		room_id: &ruma::RoomId,
		event_type: &TimelineEventType,
		state_key: Option<&str>,
	) -> Result {
		let config = &self.services.config;

		if *event_type == TimelineEventType::RoomEncrypted && !config.allow_encryption {
			return Err!(Request(Forbidden("Encryption has been disabled")));
		}

		if *event_type == TimelineEventType::RoomRedaction
			&& config.disable_local_redactions
			&& !self.services.admin.user_is_admin(user_id).await
		{
			return Err!(Request(Forbidden("Redactions are disabled on this server.")));
		}

		if self.services.users.is_suspended(user_id).await {
			return Err!(Request(UserSuspended("Cannot schedule events while suspended.")));
		}

		let own_membership =
			*event_type == TimelineEventType::RoomMember && state_key == Some(user_id.as_str());
		if !own_membership
			&& !self
				.services
				.state_cache
				.is_joined(user_id, room_id)
				.await
		{
			return Err!(Request(Forbidden("You are not joined to this room.")));
		}

		Ok(())
	}

	async fn record(&self, delay_id: &str) -> Result<DelayedEvent> {
		self.delayid_event
			.get(delay_id)
			.await
			.deserialized::<Json<DelayedEvent>>()
			.map(|Json(event)| event)
			.map_err(|_| err!(Request(NotFound("Delayed event not found."))))
	}

	fn finalise(&self, mut event: DelayedEvent, outcome: Outcome) {
		event.processing = false;
		event.finalised = Some(Finalised { ts: now_millis(), outcome });
		self.delayid_event
			.raw_put(&event.delay_id, Json(&event));
	}

	async fn recover_processing(&self) -> Result {
		let _lock = self.lock.lock().await;
		for (delay_id, mut event) in self.records().await? {
			if event.processing {
				event.processing = false;
				event.send_at = now_millis();
				self.delayid_event.raw_put(&delay_id, Json(event));
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
				if is_due(&event, now) {
					event.processing = true;
					self.delayid_event
						.raw_put(&delay_id, Json(&event));
					due.push(event);
				}
			}
			due
		};

		for event in due {
			let outcome = match self.send_event(&event).await {
				| Ok(event_id) => Outcome::Sent(event_id),
				| Err(error) => {
					warn!(delay_id = %event.delay_id, ?error, "Failed to send delayed event");
					Outcome::Error(StandardErrorBody::new(
						error.kind(),
						error.sanitized_message(),
					))
				},
			};
			self.finalise(event, outcome);
		}

		Ok(())
	}

	async fn prune(&self) -> Result {
		let _lock = self.lock.lock().await;
		for delay_id in expired(&self.records().await?, now_millis()) {
			self.delayid_event.remove(&delay_id);
		}

		Ok(())
	}

	async fn send_event(&self, event: &DelayedEvent) -> Result<OwnedEventId> {
		let state_lock = self
			.services
			.state
			.mutex
			.lock(&event.room_id)
			.await;
		let unsigned = BTreeMap::from([(
			"org.matrix.msc4140.delay_id".to_owned(),
			event.delay_id.clone().into(),
		)]);

		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: event.event_type.clone(),
					content: Raw::new(&event.content)?,
					state_key: event.state_key.clone().map(Into::into),
					unsigned: Some(unsigned),
					redacts: redacts(event),
					timestamp: event.timestamp,
					..Default::default()
				},
				&event.user_id,
				&event.room_id,
				&state_lock,
			)
			.await
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
			.iter()
			.filter_map(|(_, event)| match &event.finalised {
				| Some(finalised) => Some(
					finalised
						.ts
						.saturating_add(FINALISED_RETENTION_MS),
				),
				| None => (!event.processing).then_some(event.send_at),
			})
			.min();

		Ok(next.map_or(IDLE_WAIT, |at| Duration::from_millis(at.saturating_sub(now).max(1))))
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
				return Err(rate_limited());
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
			return Err(rate_limited());
		}

		*last = now;
		*tokens = available - 1.0;
		Ok(())
	}
}

fn rate_limited() -> Error {
	Error::Request(
		ErrorKind::LimitExceeded(LimitExceededErrorData { retry_after: None }),
		"Too many delayed event actions.".into(),
		StatusCode::TOO_MANY_REQUESTS,
	)
}

fn is_due(event: &DelayedEvent, now: u64) -> bool {
	event.finalised.is_none() && !event.processing && event.send_at <= now
}

/// Decide how an action applies to a delayed event's final outcome, if any.
///
/// A retried action that matches the outcome succeeds again and one that
/// contradicts it conflicts. Delegated requests predate finalised records and
/// see finalised events as gone.
fn resolve(outcome: Option<&Outcome>, action: Action, delegated: bool) -> Resolution {
	match (outcome, action) {
		| (None, _) => Resolution::Proceed,
		| (Some(_), _) if delegated => Resolution::NotFound,
		| (Some(Outcome::Sent(_)), Action::Send)
		| (Some(Outcome::Cancelled | Outcome::Error(_)), Action::Cancel) => Resolution::AlreadyDone,
		| (Some(_), _) => Resolution::Conflict,
	}
}

/// Finalised delayed events to forget: those older than the retention period,
/// then each user's oldest beyond the per-user cap.
fn expired(records: &[(String, DelayedEvent)], now: u64) -> Vec<String> {
	let mut kept: HashMap<&ruma::UserId, Vec<(u64, &str)>> = HashMap::new();
	let mut expired = Vec::new();

	for (delay_id, event) in records {
		let Some(finalised) = &event.finalised else {
			continue;
		};

		if finalised
			.ts
			.saturating_add(FINALISED_RETENTION_MS)
			<= now
		{
			expired.push(delay_id.clone());
		} else {
			kept.entry(&*event.user_id)
				.or_default()
				.push((finalised.ts, delay_id));
		}
	}

	for mut finalised in kept.into_values() {
		if let Some(excess) = finalised.len().checked_sub(FINALISED_PER_USER) {
			finalised.sort_unstable();
			expired.extend(
				finalised
					.into_iter()
					.take(excess)
					.map(|(_, delay_id)| delay_id.to_owned()),
			);
		}
	}

	expired
}

/// The redacted event of a delayed redaction, which pre-v11 auth rules read
/// from the top level rather than the content.
fn redacts(event: &DelayedEvent) -> Option<OwnedEventId> {
	if event.event_type != TimelineEventType::RoomRedaction {
		return None;
	}

	match event.content.get("redacts") {
		| Some(CanonicalJsonValue::String(redacts)) => EventId::parse(redacts).ok(),
		| _ => None,
	}
}

fn event_data(event: DelayedEvent) -> Result<DelayedEventData> {
	let content: Raw<AnyTimelineEventContent> =
		Raw::from_json_string(serde_json::to_string(&event.content)?)?;
	let mut data = DelayedEventData::new(
		event.delay_id,
		event.room_id,
		event.event_type,
		content,
		Duration::from_millis(event.delay_ms),
		millis(event.send_at.saturating_sub(event.delay_ms)),
	);

	data.state_key = event.state_key.map(StateKey::from);
	data.finalized = event.finalised.map(|Finalised { ts, outcome }| {
		let finalized_ts = millis(ts);
		match outcome {
			| Outcome::Sent(event_id) => Finalized::Sent { finalized_ts, event_id },
			| Outcome::Error(error) => Finalized::Error { finalized_ts, error },
			| Outcome::Cancelled => Finalized::Canceled { finalized_ts },
		}
	});

	Ok(data)
}

fn millis(ms: u64) -> MilliSecondsSinceUnixEpoch {
	MilliSecondsSinceUnixEpoch(UInt::new_saturating(ms))
}
