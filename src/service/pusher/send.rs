use futures::future::join4;
use ipaddress::IPAddress;
use ruma::{
	UInt, UserId,
	api::{
		client::push::{Pusher, PusherKind},
		push_gateway::send_event_notification::v1::{
			Device, Notification, NotificationCounts, NotificationPriority, Request,
		},
	},
	events::TimelineEventType,
	push::{Action, HighlightTweakValue, HttpPusherData, PushFormat, Ruleset, Tweak},
};
use serde_json::Value;
use tuwunel_core::{Err, Result, err, implement, matrix::Event, utils::BoolExt, warn};
use url::Url;

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn send_push_notice<E>(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	ruleset: &Ruleset,
	event: &E,
) -> Result
where
	E: Event,
{
	let mut notify = None;
	let mut tweaks = Vec::new();

	let power_levels = self
		.services
		.state_accessor
		.get_power_levels(event.room_id())
		.await
		.ok();

	let serialized = event.to_format();
	let actions = self
		.get_actions(user_id, ruleset, power_levels.as_ref(), &serialized, event.room_id())
		.await;

	for action in actions {
		let n = match action {
			| Action::Notify => true,
			| Action::SetTweak(tweak) => {
				tweaks.push(tweak.clone());
				continue;
			},
			| _ => false,
		};

		if notify.is_some() {
			return Err!(Request(BadJson(
				r#"Malformed pushrule contains more than one of these actions: ["dont_notify", "notify", "coalesce"]"#
			)));
		}

		notify = Some(n);
	}

	if notify == Some(true) || self.services.config.push_everything {
		self.send_notice(user_id, pusher, tweaks, event)
			.await?;
	}

	Ok(())
}

/// Send an account-wide counts-only notification to a push gateway.
///
/// Enabled HTTP pushers emit the request, including an explicit zero. The
/// delivery is skipped only when the gateway is known to hold the current
/// total already; an unknown gateway is always sent to.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
pub async fn send_badge_notice(&self, user_id: &UserId, pusher: &Pusher) -> Result {
	let PusherKind::Http(http) = &pusher.kind else {
		return Ok(());
	};

	if badge_count_disabled(http) {
		return Ok(());
	}

	let unread = UInt::new(self.global_notification_count(user_id).await).unwrap_or(UInt::MAX);

	if self.sent_badge(user_id, &pusher.ids.pushkey) == Some(unread) {
		return Ok(());
	}

	let device = self.prepare_http_pusher(pusher, http)?;
	let mut notify = Notification::new(vec![device]);
	notify.counts = NotificationCounts::new_explicit(Some(unread), None);

	self.send_http_notice(user_id, pusher, http, notify, Some(unread))
		.await
}

#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn send_notice<Pdu: Event>(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	tweaks: Vec<Tweak>,
	event: &Pdu,
) -> Result {
	// TODO: email
	match &pusher.kind {
		| PusherKind::Http(http) =>
			self.send_http_event_notice(user_id, pusher, http, tweaks, event)
				.await,
		// TODO: Handle email
		//PusherKind::Email(_) => Ok(()),
		| _ => Ok(()),
	}
}

#[implement(super::Service)]
async fn send_http_event_notice<Pdu: Event>(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	http: &HttpPusherData,
	tweaks: Vec<Tweak>,
	event: &Pdu,
) -> Result {
	let mut device = self.prepare_http_pusher(pusher, http)?;

	// TODO (timo): can pusher/devices have conflicting formats
	let event_id_only = http.format == Some(PushFormat::EventIdOnly);

	if !event_id_only {
		device.tweaks.clone_from(&tweaks);
	}

	let mut notify = Notification::new(vec![device]);

	notify.event_id = Some(event.event_id().to_owned());
	notify.room_id = Some(event.room_id().to_owned());

	let unread = badge_count_disabled(http)
		.is_false()
		.then_async(async || {
			UInt::new(self.global_notification_count(user_id).await).unwrap_or(UInt::MAX)
		});

	let unread = if !event_id_only {
		if *event.kind() == TimelineEventType::RoomEncrypted
			|| tweaks.iter().any(|t| {
				matches!(t, Tweak::Highlight(HighlightTweakValue::Yes) | Tweak::Sound(_))
			}) {
			notify.prio = NotificationPriority::High;
		} else {
			notify.prio = NotificationPriority::Low;
		}
		notify.sender = Some(event.sender().to_owned());
		notify.event_type = Some(event.kind().to_owned());
		notify.content = serde_json::value::to_raw_value(event.content()).ok();

		if *event.kind() == TimelineEventType::RoomMember {
			notify.user_is_target = event.state_key() == Some(event.sender().as_str());
		}

		let (display_name, room_name, room_alias, unread) = join4(
			self.services.profile.displayname(event.sender()),
			self.services
				.state_accessor
				.get_name(event.room_id()),
			self.services
				.state_accessor
				.get_canonical_alias(event.room_id()),
			unread,
		)
		.await;

		notify.sender_display_name = display_name.ok();
		notify.room_name = room_name.ok();
		notify.room_alias = room_alias.ok();

		unread
	} else {
		unread.await
	};

	if let Some(unread) = unread {
		notify.counts = NotificationCounts::new_explicit(Some(unread), None);
	}

	self.send_http_notice(user_id, pusher, http, notify, unread)
		.await
}

#[implement(super::Service)]
fn prepare_http_pusher(&self, pusher: &Pusher, http: &HttpPusherData) -> Result<Device> {
	let address = &http.url;
	let url = Url::parse(address).map_err(|e| {
		err!(Request(InvalidParam(
			warn!(url = %address, error = %e, "HTTP pusher URL is not a valid URL")
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

	let host = url.host_str().expect("URL previously validated");
	if let Ok(ip) = IPAddress::parse(host)
		&& !self.services.client.valid_cidr_range(&ip)
	{
		return Err!(Request(InvalidParam(
			warn!(%url, "HTTP pusher URL is a forbidden remote address")
		)));
	}

	let mut device = Device::new(pusher.ids.app_id.clone(), pusher.ids.pushkey.clone());
	device.data.data.clone_from(&http.data);
	device.data.format.clone_from(&http.format);

	Ok(device)
}

/// Deliver one notification to the pusher's gateway and honor its verdict.
///
/// A pushkey the gateway names in `rejected` has its pusher removed. `unread`
/// names the counts value on the wire; it is recorded as delivered only after
/// the gateway accepts, so a failed or rejected send leaves the next refresh
/// unconditional.
#[implement(super::Service)]
#[tracing::instrument(level = "debug", skip_all)]
async fn send_http_notice(
	&self,
	user_id: &UserId,
	pusher: &Pusher,
	http: &HttpPusherData,
	notify: Notification,
	unread: Option<UInt>,
) -> Result {
	let response = self
		.send_request(&http.url, Request::new(notify))
		.await?;

	let pushkey = &pusher.ids.pushkey;

	if response.rejected.contains(pushkey) {
		warn!(url = %http.url, %pushkey, "Push gateway rejected the pushkey; removing pusher");
		self.delete_pusher(user_id, pushkey).await;

		return Ok(());
	}

	if let Some(unread) = unread {
		self.record_sent_badge(user_id, pushkey, unread);
	}

	Ok(())
}

fn badge_count_disabled(http: &HttpPusherData) -> bool {
	["org.matrix.msc4076.disable_badge_count", "disable_badge_count"]
		.iter()
		.any(|key| http.data.get(*key).and_then(Value::as_bool) == Some(true))
}
