use ipaddress::IPAddress;
use ruma::{
	UserId,
	api::{
		client::push::{Pusher, PusherKind},
		push_gateway::send_event_notification::v1::{
			Device, Notification, NotificationCounts, NotificationPriority, Request,
		},
	},
	events::TimelineEventType,
	push::{Action, PushFormat, Ruleset, Tweak},
	uint,
};
use tuwunel_core::{Err, Result, err, implement, matrix::Event, warn};

use super::badge::badge_count_disabled;

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
		| PusherKind::Http(_) => {
			let mutex_key = (user_id.to_owned(), pusher.ids.pushkey.clone());
			let _lock = self.badge_send_mutex.lock(&mutex_key).await;
			let Ok(pusher) = self
				.get_pusher(user_id, &pusher.ids.pushkey)
				.await
			else {
				return Ok(());
			};
			let PusherKind::Http(http) = &pusher.kind else {
				return Ok(());
			};
			let unread = self.badge_count(user_id).await;
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

			if let Ok(ip) = IPAddress::parse(url.host_str().expect("URL previously validated"))
				&& !self.services.client.valid_cidr_range(&ip)
			{
				return Err!(Request(InvalidParam(
					warn!(%url, "HTTP pusher URL is a forbidden remote address")
				)));
			}

			// TODO (timo): can pusher/devices have conflicting formats
			let event_id_only = http.format == Some(PushFormat::EventIdOnly);

			let mut device = Device::new(pusher.ids.app_id.clone(), pusher.ids.pushkey.clone());
			device.data.data.clone_from(&http.data);
			device.data.format.clone_from(&http.format);

			// Tweaks are only added if the format is NOT event_id_only
			if !event_id_only {
				device.tweaks.clone_from(&tweaks);
			}

			let d = vec![device];
			let mut notify = Notification::new(d);

			notify.event_id = Some(event.event_id().to_owned());
			notify.room_id = Some(event.room_id().to_owned());
			let sends_badge = !badge_count_disabled(http);
			if sends_badge {
				notify.counts = NotificationCounts::new(unread, uint!(0));
			} else {
				// counts will not be serialised if it's the default (0, 0)
				// skip_serializing_if = "NotificationCounts::is_default"
				notify.counts = NotificationCounts::default();
			}

			if !event_id_only {
				if *event.kind() == TimelineEventType::RoomEncrypted
					|| tweaks.iter().any(|t| {
						matches!(
							t,
							Tweak::Highlight(ruma::push::HighlightTweakValue::Yes)
								| Tweak::Sound(_)
						)
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

				notify.sender_display_name = self
					.services
					.profile
					.displayname(event.sender())
					.await
					.ok();

				notify.room_name = self
					.services
					.state_accessor
					.get_name(event.room_id())
					.await
					.ok();

				notify.room_alias = self
					.services
					.state_accessor
					.get_canonical_alias(event.room_id())
					.await
					.ok();
			}

			let response = self
				.send_request(&http.url, Request::new(notify))
				.await?;

			if response.rejected.contains(&pusher.ids.pushkey) {
				let pushkey = &pusher.ids.pushkey;

				warn!(%url, %pushkey, "Push gateway rejected the pushkey; removing pusher");
				self.delete_pusher_unlocked(user_id, pushkey)
					.await;
			} else if sends_badge {
				self.store_last_badge(user_id, &pusher.ids.pushkey, unread.into());
			}

			Ok(())
		},
		// TODO: Handle email
		//PusherKind::Email(_) => Ok(()),
		| _ => Ok(()),
	}
}
