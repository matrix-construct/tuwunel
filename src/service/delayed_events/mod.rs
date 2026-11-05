//! MSC4140 delayed call memberships, held in memory only.
//!
//! A MatrixRTC client schedules its own call leave a few seconds ahead and
//! keeps restarting it, so the server sends the leave once the client stops,
//! for example on a crash. Only call memberships can be delayed: the state
//! endpoint validates other state types before sending them, which a delayed
//! event would skip. A server restart drops every scheduled leave; the
//! client's next restart request then answers `M_NOT_FOUND`, and the client
//! schedules a new one.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex},
	time::Duration,
};

use async_trait::async_trait;
use ruma::{
	CanonicalJsonObject, OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::client::delayed_events::update_delayed_event::UpdateAction, serde::Raw,
};
use tokio::{sync::Notify, time::Instant};
use tuwunel_core::{Err, Result, utils::rand, warn};
use tuwunel_matrix::PduBuilder;

/// The only event type that can be delayed.
pub const CALL_MEMBER: &str = "org.matrix.msc3401.call.member";

/// Longest delay a client may ask for.
const MAX_DELAY: Duration = Duration::from_hours(24);

/// Most events one user may have scheduled at once.
const MAX_PER_USER: usize = 100;

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	scheduled: Mutex<HashMap<String, Scheduled>>,
	notify: Notify,
}

struct Scheduled {
	user_id: OwnedUserId,
	room_id: OwnedRoomId,
	state_key: String,
	content: CanonicalJsonObject,
	delay: Duration,
	send_at: Instant,
}

#[async_trait]
impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			scheduled: Mutex::default(),
			notify: Notify::new(),
		}))
	}

	async fn worker(self: Arc<Self>) -> Result {
		loop {
			let next = self.send_due().await?;
			let wait = async {
				match next {
					| Some(send_at) => tokio::time::sleep_until(send_at).await,
					| None => std::future::pending().await,
				}
			};

			tokio::select! {
				() = self.services.server.until_shutdown() => return Ok(()),
				() = self.notify.notified() => {},
				() = wait => {},
			}
		}
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

impl Service {
	/// Schedules a call membership of a joined, unsuspended user and returns
	/// its delay ID.
	pub async fn schedule(
		&self,
		user_id: &UserId,
		room_id: &RoomId,
		state_key: &str,
		content: CanonicalJsonObject,
		delay: Duration,
	) -> Result<String> {
		if delay > MAX_DELAY {
			return Err!(Request(UnstableDelayTooLarge("Delays are limited to 24 hours.")));
		}
		if self.services.users.is_suspended(user_id).await {
			return Err!(Request(UserSuspended("Account is suspended.")));
		}
		if !self
			.services
			.state_cache
			.is_joined(user_id, room_id)
			.await
		{
			return Err!(Request(Forbidden("You are not joined to this room.")));
		}

		let mut scheduled = self.scheduled.lock()?;
		let user_scheduled = scheduled
			.values()
			.filter(|event| event.user_id == user_id)
			.count();
		if user_scheduled >= MAX_PER_USER {
			return Err!(Request(Forbidden("Too many delayed events are scheduled.")));
		}

		let delay_id = rand::string(32);
		scheduled.insert(delay_id.clone(), Scheduled {
			user_id: user_id.to_owned(),
			room_id: room_id.to_owned(),
			state_key: state_key.to_owned(),
			content,
			delay,
			send_at: deadline(delay),
		});
		drop(scheduled);
		self.notify.notify_one();

		Ok(delay_id)
	}

	/// Restarts, cancels or at once sends one of the user's scheduled events.
	pub async fn update(
		&self,
		user_id: &UserId,
		delay_id: &str,
		action: &UpdateAction,
	) -> Result {
		let event = {
			let mut scheduled = self.scheduled.lock()?;
			if scheduled
				.get(delay_id)
				.is_none_or(|event| event.user_id != user_id)
			{
				return Err!(Request(NotFound("Delayed event not found.")));
			}

			match action {
				// A later deadline needs no wake-up: the worker finds it not yet due.
				| UpdateAction::Restart => {
					let event = scheduled.get_mut(delay_id).expect("found above");
					event.send_at = deadline(event.delay);
					return Ok(());
				},
				| UpdateAction::Cancel => {
					scheduled.remove(delay_id);
					return Ok(());
				},
				| UpdateAction::Send => scheduled.remove(delay_id).expect("found above"),
				| _ => return Err!(Request(InvalidParam("Unknown delayed event action."))),
			}
		};

		self.send(event).await
	}

	/// Sends every due event and returns when the next one is due.
	async fn send_due(&self) -> Result<Option<Instant>> {
		let now = Instant::now();
		let (due, next): (Vec<Scheduled>, _) = {
			let mut scheduled = self.scheduled.lock()?;
			let due = scheduled
				.extract_if(|_, event| event.send_at <= now)
				.map(|(_, event)| event)
				.collect();
			let next = scheduled
				.values()
				.map(|event| event.send_at)
				.min();
			(due, next)
		};

		for event in due {
			let (user_id, room_id) = (event.user_id.clone(), event.room_id.clone());
			if let Err(error) = self.send(event).await {
				warn!(%user_id, %room_id, ?error, "Failed to send a delayed call membership");
			}
		}

		Ok(next)
	}

	async fn send(&self, event: Scheduled) -> Result {
		let state_lock = self
			.services
			.state
			.mutex
			.lock(&event.room_id)
			.await;

		self.services
			.timeline
			.build_and_append_pdu(
				PduBuilder {
					event_type: CALL_MEMBER.into(),
					content: Raw::new(&event.content)?,
					state_key: Some(event.state_key.into()),
					..Default::default()
				},
				&event.user_id,
				&event.room_id,
				&state_lock,
			)
			.await?;

		Ok(())
	}
}

/// When an event scheduled now with `delay` is due; delays are at most
/// [`MAX_DELAY`].
fn deadline(delay: Duration) -> Instant {
	Instant::now()
		.checked_add(delay)
		.expect("a delay of at most 24 hours fits an Instant")
}
