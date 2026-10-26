//! Configures push-gateway delivery and notification policy.
//!
//! Gateway transport settings control requests. Rule evaluation and
//! active-user suppression control notification selection.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

/// Configures push-gateway delivery and notification policy.
///
/// Gateway transport settings control requests. Rule evaluation and
/// active-user suppression control notification selection.
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

	/// Sends every message to pushers without filtering by push rules or
	/// actions.
	///
	/// This compatibility and special-purpose setting should not normally be
	/// needed, but can bypass bugs or outdated server-side rule support, let
	/// clients evaluate rules themselves, or support hosting and custom-pusher
	/// requirements. It does not alter the notification records shown in the
	/// notifications pane.
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

	/// Experimentally suppresses push notifications for active users.
	///
	/// When enabled, users with Online presence and recent activity,
	/// determined from presence and sync activity, receive no push
	/// notifications, reducing duplicate alerts while active on another
	/// client. The default is disabled to preserve legacy behavior.
	///
	/// reloadable: yes
	/// legacy: suppress_push_when_active
	#[serde(default)]
	pub suppress_when_active: bool,
}

impl Default for Push {
	fn default() -> Self {
		Self {
			idle_timeout: default_idle_timeout(),
			notification_path: default_notification_path(),
			everything: Default::default(),
			msc3664_related_event_match: Default::default(),
			suppress_when_active: Default::default(),
		}
	}
}

fn default_idle_timeout() -> u64 { 15 }

fn default_notification_path() -> String { "/_matrix/push/v1/notify".to_owned() }
