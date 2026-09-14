mod append;
mod badge;
mod notification;
mod request;
mod send;
mod suppressed;
#[cfg(test)]
mod tests;

use std::{
	collections::BTreeMap,
	sync::{Arc, LazyLock},
};

use futures::{Stream, StreamExt, TryFutureExt, future::join};
use ruma::{
	DeviceId, OwnedDeviceId, OwnedEventId, OwnedRoomId, OwnedUserId, RoomId, UserId,
	api::client::push::{Pusher, PusherKind, set_pusher::v3::PusherAction},
	events::{
		AnySyncTimelineEvent, GlobalAccountDataEventType, push_rules::PushRulesEvent,
		room::power_levels::RoomPowerLevels,
	},
	push::{Action, FlattenedJson, PushConditionPowerLevelsCtx, PushConditionRoomCtx, Ruleset},
	serde::Raw,
	uint,
};
use serde::Deserialize;
use tracing::Level;
use tuwunel_core::{
	Err, Result, err, implement,
	matrix::Event,
	utils::{
		MutexMap,
		future::TryExtExt,
		result::ErrLog,
		stream::{BroadbandExt, IterStream, ReadyExt, TryIgnore, WidebandExt},
	},
};
use tuwunel_database::{Database, Deserialized, Ignore, Interfix, Json, Map};
use url::Url;

use self::badge::SentBadges;
pub use self::{
	append::Notified,
	suppressed::{SuppressedPushes, SuppressedRooms},
};

/// The events an event relates to, keyed by relation type, for MSC3664.
type RelatedEvents = BTreeMap<String, FlattenedJson>;

/// The inputs for evaluating one user's push rules against one event.
pub struct Evaluate<'a, 'b> {
	/// The user whose rules are evaluated, and who is notified.
	pub user: &'b UserId,
	/// The user's ruleset, which outlives the borrowed actions returned.
	pub ruleset: &'a Ruleset,
	/// The room's power levels, without which some conditions never match.
	pub power_levels: Option<&'b RoomPowerLevels>,
	/// The event being evaluated, in the format the conditions flatten.
	pub pdu: &'b Raw<AnySyncTimelineEvent>,
	/// The room the event was sent to.
	pub room_id: &'b RoomId,
	/// The events it relates to, or `None` where they are not resolved at all.
	pub related_events: Option<&'b Arc<RelatedEvents>>,
}

/// MSC3664 matches a reply as if it carried a relation of this type.
const IN_REPLY_TO: &str = "m.in_reply_to";

/// Shared by every event that resolves no relations, which is every event
/// while MSC3664 evaluation is disabled.
static NO_RELATED_EVENTS: LazyLock<Arc<RelatedEvents>> = LazyLock::new(Arc::default);

#[derive(Deserialize)]
struct ExtractRelatesTo {
	#[serde(rename = "m.relates_to")]
	relates_to: RelatesTo,
}

#[derive(Deserialize)]
struct RelatesTo {
	rel_type: Option<String>,

	event_id: Option<OwnedEventId>,

	#[serde(rename = "m.in_reply_to")]
	in_reply_to: Option<InReplyTo>,
}

#[derive(Deserialize)]
struct InReplyTo {
	event_id: OwnedEventId,
}

pub struct Service {
	services: Arc<crate::services::OnceServices>,
	notification_increment_mutex: MutexMap<(OwnedRoomId, OwnedUserId), ()>,
	highlight_increment_mutex: MutexMap<(OwnedRoomId, OwnedUserId), ()>,
	db: Data,
	suppressed: suppressed::SuppressedQueue,
	sent_badges: SentBadges,
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
			sent_badges: SentBadges::default(),
		}))
	}

	fn name(&self) -> &str { crate::service::make_name(std::module_path!()) }
}

#[implement(Service)]
pub async fn set_pusher(
	&self,
	sender: &UserId,
	sender_device: &DeviceId,
	pusher: &PusherAction,
) -> Result {
	match pusher {
		| PusherAction::Delete(ids) =>
			self.set_pusher_delete(sender, ids.pushkey.as_str())
				.await,
		| PusherAction::Post(data) =>
			self.set_pusher_post(sender, sender_device, pusher, &data.pusher)?,
	}

	Ok(())
}

#[implement(Service)]
async fn set_pusher_delete(&self, sender: &UserId, pushkey: &str) {
	self.delete_pusher(sender, pushkey).await;
}

#[implement(Service)]
fn set_pusher_post(
	&self,
	sender: &UserId,
	sender_device: &DeviceId,
	action: &PusherAction,
	pusher: &Pusher,
) -> Result {
	let pushkey = pusher.ids.pushkey.as_str();

	if pushkey.len() > 512 {
		return Err!(Request(InvalidParam("Push key length cannot be greater than 512 bytes.")));
	}

	if pusher.ids.app_id.as_str().len() > 64 {
		return Err!(Request(InvalidParam("App ID length cannot be greater than 64 bytes.")));
	}

	if let PusherKind::Http(http) = &pusher.kind {
		let url = &http.url;
		let url = Url::parse(&http.url).map_err(|e| {
			err!(Request(InvalidParam(warn!(%url, "HTTP pusher URL is not a valid URL: {e}"))))
		})?;

		self.check_http_pusher_url(&url)?;
	}

	let key = (sender, pushkey);
	self.db.senderkey_pusher.put(key, Json(action));
	self.db
		.pushkey_deviceid
		.insert(pushkey, sender_device);

	self.forget_sent_badge(sender, pushkey);

	Ok(())
}

#[implement(Service)]
fn check_http_pusher_url(&self, url: &Url) -> Result {
	if ["http", "https"]
		.iter()
		.all(|&scheme| !scheme.eq_ignore_ascii_case(url.scheme()))
	{
		return Err!(Request(InvalidParam(
			warn!(%url, "HTTP pusher URL is not a valid HTTP/HTTPS URL")
		)));
	}

	if self.services.client.proxy.resolver_alias(url) {
		return Err!(Request(InvalidParam(
			warn!(%url, "HTTP pusher URL is a forbidden proxy endpoint")
		)));
	}

	if !self.services.client.valid_cidr_range_url(url) {
		return Err!(Request(InvalidParam(
			warn!(%url, "HTTP pusher URL is a forbidden remote address")
		)));
	}

	Ok(())
}

#[implement(Service)]
pub async fn delete_pusher(&self, sender: &UserId, pushkey: &str) {
	let key = (sender, pushkey);
	self.db.senderkey_pusher.del(key);
	self.db.pushkey_deviceid.remove(pushkey);
	self.clear_suppressed_pushkey(sender, pushkey);
	self.forget_sent_badge(sender, pushkey);

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

/// The push ruleset a user's notifications are evaluated against.
///
/// A user without stored rules gets the server default ruleset.
#[implement(Service)]
#[tracing::instrument(skip(self), level = "debug")]
pub async fn ruleset(&self, user_id: &UserId) -> Ruleset {
	self.services
		.account_data
		.get_global(user_id, GlobalAccountDataEventType::PushRules)
		.await
		.log_err(Level::TRACE)
		.map_or_else(|_| Ruleset::server_default(user_id), |ev: PushRulesEvent| ev.content.global)
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
	Evaluate {
		user,
		ruleset,
		power_levels,
		pdu,
		room_id,
		related_events,
	}: Evaluate<'a, '_>,
) -> &'a [Action] {
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

	let ctx = match related_events {
		| Some(related_events) => ctx.with_related_events(related_events.clone()),
		| None => ctx,
	};

	let ctx = match power_levels {
		| Some(pl) => ctx.with_power_levels(pl),
		| None => ctx,
	};

	ruleset.get_actions(pdu, &ctx).await
}

/// Resolve the events an event relates to, for the MSC3664 push condition.
///
/// `None` while `msc3664_related_event_match` is disabled, which leaves every
/// MSC3664 condition unable to match. The relations do not vary by recipient,
/// so the append path resolves them once for the whole room; the push-notice
/// path resolves them per notice, since it holds one event at a time.
#[implement(Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn related_events<E: Event>(&self, event: &E) -> Option<Arc<RelatedEvents>> {
	let config = &self.services.server.config;

	if !config.msc3664_related_event_match {
		return None;
	}

	let Ok(ExtractRelatesTo { relates_to }) = event.get_content() else {
		return Some(NO_RELATED_EVENTS.clone());
	};

	let reply = relates_to
		.in_reply_to
		.map(|reply| (IN_REPLY_TO.to_owned(), reply.event_id));

	let related = relates_to
		.rel_type
		.zip(relates_to.event_id)
		.into_iter()
		.chain(reply)
		.stream()
		.wide_filter_map(async |(rel_type, event_id)| {
			let related = self
				.services
				.timeline
				.get_pdu(&event_id)
				.await
				.ok()
				// A relation crossing rooms is not one the sender could have made, and
				// following it would expose an event the recipient may not be in a room for.
				.filter(|related| related.room_id() == event.room_id())?;

			let related: Raw<AnySyncTimelineEvent> = related.to_format();

			Some((rel_type, FlattenedJson::from_raw(&related)))
		})
		.collect()
		.await;

	Some(Arc::new(related))
}
