mod append;
mod notification;
mod request;
mod send;
mod suppressed;
#[cfg(test)]
mod tests;

use std::sync::Arc;

use futures::{Stream, StreamExt, TryFutureExt, future::join};
use ipaddress::IPAddress;
use ruma::{
	DeviceId, OwnedDeviceId, OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::client::push::{Pusher, PusherKind, set_pusher},
	events::{AnySyncTimelineEvent, TimelineEventType, room::power_levels::RoomPowerLevels},
	push::{Action, PushConditionPowerLevelsCtx, PushConditionRoomCtx, Ruleset},
	serde::Raw,
	uint,
};
use serde_json::{Value as JsonValue, value::to_raw_value};
use tuwunel_core::{
	Err, Result, err, implement,
	utils::{
		MutexMap,
		future::TryExtExt,
		stream::{BroadbandExt, ReadyExt, TryIgnore},
	},
};
use tuwunel_database::{Database, Deserialized, Ignore, Interfix, Json, Map};

pub use self::append::Notified;

const MINDROOM_STREAM_STATUS_KEY: &str = "io.mindroom.stream_status";
const MINDROOM_TERMINAL_STREAM_STATUSES: [&str; 4] =
	["completed", "cancelled", "interrupted", "error"];

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	notification_increment_mutex: MutexMap<(OwnedRoomId, OwnedUserId), ()>,
	highlight_increment_mutex: MutexMap<(OwnedRoomId, OwnedUserId), ()>,
	db: Data,
	suppressed: suppressed::SuppressedQueue,
}

enum MindroomPushEvaluation {
	Suppress,
	Normalize(Raw<AnySyncTimelineEvent>),
	Unchanged,
}

pub(super) fn mindroom_terminal_push_content(
	event_type: &TimelineEventType,
	content: &JsonValue,
) -> Option<JsonValue> {
	if !is_terminal_mindroom_edit(content) {
		return None;
	}

	match event_type {
		| TimelineEventType::RoomMessage => content.as_object()?.get("m.new_content").cloned(),
		| TimelineEventType::RoomEncrypted => {
			let mut encrypted_content = content.clone();
			encrypted_content
				.as_object_mut()?
				.remove("m.relates_to");
			Some(encrypted_content)
		},
		| _ => None,
	}
}

fn is_terminal_mindroom_edit(content: &JsonValue) -> bool {
	let Some((content, stream_status)) = mindroom_stream_content(content) else {
		return false;
	};
	if !MINDROOM_TERMINAL_STREAM_STATUSES.contains(&stream_status) {
		return false;
	}

	content
		.get("m.relates_to")
		.and_then(JsonValue::as_object)
		.and_then(|relation| relation.get("rel_type"))
		.and_then(JsonValue::as_str)
		== Some("m.replace")
}

fn mindroom_stream_content(
	content: &JsonValue,
) -> Option<(&serde_json::Map<String, JsonValue>, &str)> {
	// This is an event-level protocol signal, not an account privilege: senders
	// may opt their own streamed events into these notification semantics.
	let content = content.as_object()?;
	let stream_status = content
		.get(MINDROOM_STREAM_STATUS_KEY)
		.and_then(JsonValue::as_str)?;
	Some((content, stream_status))
}

pub(super) fn mindroom_push_suppressed(pdu: &Raw<AnySyncTimelineEvent>) -> bool {
	matches!(mindroom_push_evaluation(pdu), MindroomPushEvaluation::Suppress)
}

#[cfg(test)]
fn mindroom_terminal_push_event(
	pdu: &Raw<AnySyncTimelineEvent>,
) -> Option<Raw<AnySyncTimelineEvent>> {
	match mindroom_push_evaluation(pdu) {
		| MindroomPushEvaluation::Normalize(event) => Some(event),
		| MindroomPushEvaluation::Suppress | MindroomPushEvaluation::Unchanged => None,
	}
}

fn mindroom_push_evaluation(pdu: &Raw<AnySyncTimelineEvent>) -> MindroomPushEvaluation {
	let Ok(mut event) = serde_json::to_value(pdu) else {
		return MindroomPushEvaluation::Unchanged;
	};
	let Some(event_type) = event.get("type").and_then(JsonValue::as_str) else {
		return MindroomPushEvaluation::Unchanged;
	};
	if !matches!(event_type, "m.room.message" | "m.room.encrypted") {
		return MindroomPushEvaluation::Unchanged;
	}
	let Some(content) = event.get("content") else {
		return MindroomPushEvaluation::Unchanged;
	};
	let Some((_, stream_status)) = mindroom_stream_content(content) else {
		return MindroomPushEvaluation::Unchanged;
	};
	// Only known terminal states may notify. Future intermediate states fail
	// closed so a new producer state cannot accidentally reintroduce push spam.
	if !MINDROOM_TERMINAL_STREAM_STATUSES.contains(&stream_status) {
		return MindroomPushEvaluation::Suppress;
	}
	if !is_terminal_mindroom_edit(content) {
		return MindroomPushEvaluation::Unchanged;
	}

	let push_content =
		mindroom_terminal_push_content(&TimelineEventType::from(event_type), content);
	let Some(push_content) = push_content else {
		return MindroomPushEvaluation::Unchanged;
	};

	// Evaluate the completed replacement as the final message it represents.
	// This lets ordinary room, DM, mention, and mute rules decide the push result
	// instead of the generic edit-suppression rule swallowing the terminal update.
	let Some(event_content) = event.get_mut("content") else {
		return MindroomPushEvaluation::Unchanged;
	};
	*event_content = push_content;
	let Ok(raw_event) = to_raw_value(&event) else {
		return MindroomPushEvaluation::Unchanged;
	};
	MindroomPushEvaluation::Normalize(Raw::from_json(raw_event))
}

struct Data {
	db: Arc<Database>,
	senderkey_pusher: Arc<Map>,
	pushkey_deviceid: Arc<Map>,
	useridcount_notification: Arc<Map>,
	userroomid_highlightcount: Arc<Map>,
	userroomid_notificationcount: Arc<Map>,
	roomuserid_lastnotificationread: Arc<Map>,
}

impl crate::Service for Service {
	fn build(args: &crate::Args<'_>) -> Result<Arc<Self>> {
		Ok(Arc::new(Self {
			services: args.services.clone(),
			notification_increment_mutex: MutexMap::new(),
			highlight_increment_mutex: MutexMap::new(),
			db: Data {
				db: args.db.clone(),
				senderkey_pusher: args.db["senderkey_pusher"].clone(),
				pushkey_deviceid: args.db["pushkey_deviceid"].clone(),
				useridcount_notification: args.db["useridcount_notification"].clone(),
				userroomid_highlightcount: args.db["userroomid_highlightcount"].clone(),
				userroomid_notificationcount: args.db["userroomid_notificationcount"].clone(),
				roomuserid_lastnotificationread: args.db["roomuserid_lastnotificationread"]
					.clone(),
			},
			suppressed: suppressed::SuppressedQueue::default(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub async fn set_pusher(
	&self,
	sender: &UserId,
	sender_device: &DeviceId,
	pusher: &set_pusher::v3::PusherAction,
) -> Result {
	match pusher {
		| set_pusher::v3::PusherAction::Post(data) => {
			let pushkey = data.pusher.ids.pushkey.as_str();

			if pushkey.len() > 512 {
				return Err!(Request(InvalidParam(
					"Push key length cannot be greater than 512 bytes."
				)));
			}

			if data.pusher.ids.app_id.as_str().len() > 64 {
				return Err!(Request(InvalidParam(
					"App ID length cannot be greater than 64 bytes."
				)));
			}

			// add some validation to the pusher URL
			let pusher_kind = &data.pusher.kind;
			if let PusherKind::Http(http) = pusher_kind {
				let url = &http.url;
				let url = url::Url::parse(&http.url).map_err(|e| {
					err!(Request(InvalidParam(
						warn!(%url, "HTTP pusher URL is not a valid URL: {e}")
					)))
				})?;

				if ["http", "https"]
					.iter()
					.all(|&scheme| !scheme.eq_ignore_ascii_case(url.scheme()))
				{
					return Err!(Request(InvalidParam(
						warn!(%url, "HTTP pusher URL is not a valid HTTP/HTTPS URL")
					)));
				}

				if let Ok(ip) =
					IPAddress::parse(url.host_str().expect("URL previously validated"))
					&& !self.services.client.valid_cidr_range(&ip)
				{
					return Err!(Request(InvalidParam(
						warn!(%url, "HTTP pusher URL is a forbidden remote address")
					)));
				}
			}

			let pushkey = data.pusher.ids.pushkey.as_str();
			let key = (sender, pushkey);
			self.db.senderkey_pusher.put(key, Json(pusher));
			self.db
				.pushkey_deviceid
				.insert(pushkey, sender_device);
		},
		| set_pusher::v3::PusherAction::Delete(ids) => {
			self.delete_pusher(sender, ids.pushkey.as_str())
				.await;
		},
	}

	Ok(())
}

#[implement(Service)]
pub async fn delete_pusher(&self, sender: &UserId, pushkey: &str) {
	let key = (sender, pushkey);
	self.db.senderkey_pusher.del(key);
	self.db.pushkey_deviceid.remove(pushkey);
	self.clear_suppressed_pushkey(sender, pushkey);

	self.services
		.sending
		.cleanup_events(None, Some(sender), Some(pushkey))
		.await
		.ok();
}

#[implement(Service)]
pub async fn get_device_pushkeys(&self, sender: &UserId, device_id: &DeviceId) -> Vec<String> {
	self.get_pushkeys(sender)
		.map(ToOwned::to_owned)
		.broad_filter_map(async |pushkey| {
			self.get_pusher_device(&pushkey)
				.await
				.ok()
				.as_ref()
				.is_some_and(|pusher_device| pusher_device == device_id)
				.then_some(pushkey)
		})
		.collect()
		.await
}

#[implement(Service)]
pub async fn get_pusher_device(&self, pushkey: &str) -> Result<OwnedDeviceId> {
	self.db
		.pushkey_deviceid
		.get(pushkey)
		.await
		.deserialized()
}

#[implement(Service)]
pub async fn get_pusher(&self, sender: &UserId, pushkey: &str) -> Result<Pusher> {
	let senderkey = (sender, pushkey);
	self.db
		.senderkey_pusher
		.qry(&senderkey)
		.await
		.deserialized()
}

#[implement(Service)]
pub async fn get_pushers(&self, sender: &UserId) -> Vec<Pusher> {
	let prefix = (sender, Interfix);
	self.db
		.senderkey_pusher
		.stream_prefix(&prefix)
		.ignore_err()
		.map(|(_, pusher): (Ignore, Pusher)| pusher)
		.collect()
		.await
}

#[implement(Service)]
pub fn get_pushkeys<'a>(&'a self, sender: &'a UserId) -> impl Stream<Item = &str> + Send + 'a {
	let prefix = (sender, Interfix);
	self.db
		.senderkey_pusher
		.keys_prefix(&prefix)
		.ignore_err()
		.map(|(_, pushkey): (Ignore, &str)| pushkey)
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub fn get_notifications<'a>(
	&'a self,
	sender: &'a UserId,
	from: Option<u64>,
) -> impl Stream<Item = (u64, Notified)> + Send + 'a {
	let from = from
		.map(|from| from.saturating_sub(1))
		.unwrap_or(u64::MAX);

	self.db
		.useridcount_notification
		.rev_stream_from(&(sender, from))
		.ignore_err()
		.map(|item: ((&UserId, u64), _)| (item.0, item.1))
		.ready_take_while(move |((user_id, _count), _)| sender == *user_id)
		.map(|((_, count), notified)| (count, notified))
}

#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn get_actions<'a>(
	&self,
	user: &UserId,
	ruleset: &'a Ruleset,
	power_levels: Option<&RoomPowerLevels>,
	pdu: &Raw<AnySyncTimelineEvent>,
	room_id: &RoomId,
) -> &'a [Action] {
	let mindroom_push = mindroom_push_evaluation(pdu);
	let pdu = match &mindroom_push {
		| MindroomPushEvaluation::Suppress => return &[],
		| MindroomPushEvaluation::Normalize(event) => event,
		| MindroomPushEvaluation::Unchanged => pdu,
	};

	let user_display_name = self
		.services
		.profile
		.displayname(user)
		.unwrap_or_else(|_| user.localpart().to_owned());

	let room_joined_count = self
		.services
		.state_cache
		.room_joined_count(room_id)
		.map_ok(TryInto::try_into)
		.map_ok(|res| res.unwrap_or_else(|_| uint!(1)))
		.unwrap_or_default();

	let (room_joined_count, user_display_name) = join(room_joined_count, user_display_name).await;

	let power_levels = power_levels.map(|power_levels| PushConditionPowerLevelsCtx {
		users: power_levels.users.clone(),
		users_default: power_levels.users_default,
		notifications: power_levels.notifications.clone(),
		rules: power_levels.rules.clone(),
	});

	let ctx = PushConditionRoomCtx::new(
		room_id.to_owned(),
		room_joined_count,
		user.to_owned(),
		user_display_name,
	);
	let ctx = match power_levels {
		| Some(pl) => ctx.with_power_levels(pl),
		| None => ctx,
	};

	ruleset.get_actions(pdu, &ctx).await
}
