//! Configures push-gateway delivery and notification policy.
//!
//! Gateway transport settings control requests. Rule evaluation and
//! active-user suppression control notification selection, and the email
//! delay decides when notification emails go out.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

/// Push notifications
///
/// Controls delivery to the push gateways that clients register for their
/// users, and which events reach them. Push works with nothing set here: by
/// default the server evaluates each user's push rules and forwards only
/// events the rules mark as notifying. `email_delay` decides when notification
/// emails go out to users who turn them on.
///
/// Tips:
/// - Gateway requests obey `network.http.ip_range_denylist`, so with its
///   default a gateway on a loopback or private address is refused.
/// - `suppress_when_active` is experimental and logs a warning at startup.
///   It judges activity from presence, so it needs `presence.allow_local`.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.push",
	group = "true"
)]
pub struct Push {
	/// Sets the notification-gateway pusher idle connection-pool timeout.
	///
	/// default: 15
	/// legacy: pusher_idle_timeout
	#[serde(default = "default_idle_timeout")]
	pub idle_timeout: u64,

	/// Selects the suffix stripped from a gateway URL before constructing a
	/// push request.
	///
	/// reloadable: yes
	/// default: "/_matrix/push/v1/notify"
	/// legacy: notification_push_path
	#[serde(default = "default_notification_path")]
	pub notification_path: String,

	/// Sends every message to HTTP pushers without filtering by push rules or
	/// actions.
	///
	/// This compatibility and special-purpose setting should not normally be
	/// needed, but can bypass bugs or outdated server-side rule support, let
	/// clients evaluate rules themselves, or support hosting and custom-pusher
	/// requirements. It does not alter the notification records shown in the
	/// notifications pane. Email pushers still mail only what the push rules
	/// notify for.
	///
	/// reloadable: yes
	/// legacy: push_everything
	#[serde(default)]
	pub everything: bool,

	/// Evaluates the `im.nheko.msc3664.related_event_match` push-rule
	/// condition.
	///
	/// The condition matches properties of the event an incoming event relates
	/// to, enabling reply or reaction notifications for a user's own messages
	/// that no other condition can express. Enabling it costs one extra event
	/// lookup for each event carrying a relation. The default
	/// `.im.nheko.msc3664.reply` rule uses it; disabling evaluation leaves
	/// that rule present but unable to match replies, although clients
	/// implementing
	/// [MSC3664](https://github.com/matrix-org/matrix-spec-proposals/pull/3664)
	/// may still evaluate it locally.
	///
	/// reloadable: yes
	/// legacy: msc3664_related_event_match
	#[serde(default)]
	pub msc3664_related_event_match: bool,

	/// Experimentally suppresses HTTP push notifications for active users.
	///
	/// When enabled, users with Online presence and recent activity,
	/// determined from presence and sync activity, receive no HTTP push
	/// notifications, reducing duplicate alerts while active on another
	/// client. The default is disabled to preserve legacy behavior. Email
	/// pushers ignore this setting; they do not mail messages the user reads
	/// before `email_delay` ends.
	///
	/// reloadable: yes
	/// legacy: suppress_push_when_active
	#[serde(default)]
	pub suppress_when_active: bool,

	/// Seconds an unread notification waits before an email pusher mails it.
	///
	/// Email notifications need `[global.smtp]`; a user receives them when
	/// their client registers an email pusher for an address verified on their
	/// account. A notification still unread after this delay is mailed
	/// together with any others waiting. A room in that mail is then held back
	/// longer before it can prompt each further mail, up to a day, until the
	/// user reads it. With a delay of 0, each notification is mailed as it
	/// arrives and no room is held back.
	///
	/// reloadable: yes
	/// default: 600
	#[serde(default = "default_email_delay")]
	pub email_delay: u64,
}

impl Default for Push {
	fn default() -> Self {
		Self {
			idle_timeout: default_idle_timeout(),
			notification_path: default_notification_path(),
			everything: Default::default(),
			msc3664_related_event_match: Default::default(),
			suppress_when_active: Default::default(),
			email_delay: default_email_delay(),
		}
	}
}

fn default_idle_timeout() -> u64 { 15 }

fn default_notification_path() -> String { "/_matrix/push/v1/notify".to_owned() }

fn default_email_delay() -> u64 { 600 }
