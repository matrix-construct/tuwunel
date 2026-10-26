//! Configures room creation, access, and lifecycle.
//!
//! Defaults govern newly created rooms and supported versions. Dedicated
//! settings cover joins, invites, relations, redaction, policy servers, and
//! extremities.

use regex::RegexSet;
use ruma::RoomVersionId;
use serde::Deserialize;
use serde_json::Value;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures room creation, access, and lifecycle.
///
/// Defaults govern newly created rooms and supported versions. Dedicated
/// settings cover joins, invites, relations, redaction, policy servers, and
/// extremities.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rooms",
	group = "true",
	ignore = "extremities invites joins policy_server redaction relations"
)]
pub struct Rooms {
	/// Sets the default `m.federate` property for newly created rooms when the
	/// client does not request one.
	///
	/// When both this setting and `federation.enable` are false, we always
	/// override the client-requested `m.federate` value to false. Rooms
	/// permanently retain the setting chosen at creation, so changing this
	/// value affects only new rooms.
	///
	/// reloadable: yes
	/// legacy: federate_created_rooms
	#[serde(default = "true_fn")]
	pub federate_by_default: bool,

	/// Calls `/forget` on the user's behalf whenever they leave a room.
	///
	/// This implements
	/// [MSC4267](https://github.com/matrix-org/matrix-spec-proposals/pull/4267),
	/// automatically forgetting rooms on leave.
	///
	/// reloadable: yes
	/// legacy: forget_forced_upon_leave
	#[serde(default)]
	pub forget_forced_upon_leave: bool,

	/// Allows standard users to create rooms.
	///
	/// Appservices and admins can always create rooms.
	///
	/// reloadable: yes
	/// legacy: allow_room_creation
	#[serde(default = "true_fn")]
	pub allow_creation: bool,

	/// Allows standard users to create room aliases.
	///
	/// Appservices and admins can always create room aliases.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: allow_room_alias_creation
	#[serde(default = "true_fn")]
	pub allow_alias_creation: bool,

	/// Allows joining or creating room versions we do not officially support.
	///
	/// Unstable versions may have flawed specifications or a non-conforming
	/// implementation, so correct operation is not guaranteed even when
	/// incorrect behavior is tolerable or unnoticed. We officially support
	/// versions 6 and later, with slightly experimental support for versions 3
	/// through 5 that works in practice. Set to false to disable these
	/// unsupported versions.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: allow_unstable_room_versions
	#[serde(default = "true_fn")]
	pub allow_unstable_versions: bool,

	/// Enables experimental room versions for developer use.
	///
	/// Unlike unstable versions, these may be under development, prototype
	/// specification changes, or present serious risks to server operation or
	/// database integrity. Set to true only for developer use.
	///
	/// reloadable: yes
	/// legacy: allow_experimental_room_versions
	#[serde(default)]
	pub allow_experimental_versions: bool,

	/// Selects the room version used by default when creating rooms.
	///
	/// The default follows the specification but may follow developer
	/// recommendations.
	/// Override it only with an understanding that doing so precludes updates
	/// to newer versions.
	///
	/// reloadable: yes
	/// legacy: default_room_version
	#[serde(default = "default_default_version")]
	pub default_version: RoomVersionId,

	/// Sets default power-level overrides for newly created rooms.
	///
	/// This uses the client `/createRoom` parameter's
	/// `power_level_content_override` shape and is merged before per-request
	/// overrides, so clients can still override it per room. Only new rooms
	/// are affected. Top-level keys replace wholesale rather than deep-merge,
	/// matching the client parameter: `users` or `events` replaces that entire
	/// computed default submap.
	///
	/// reloadable: yes
	/// default: unset
	/// config-example: { users_default = 50 }
	/// legacy: default_power_level_content_override
	#[serde(default)]
	pub default_power_level_content_override: Option<Value>,

	/// Deactivates users who try to join banned rooms or forbidden room
	/// aliases or server names.
	///
	/// This includes room aliases or IDs with forbidden server names and makes
	/// the user leave all rooms; federation activity such as remote invites is
	/// ignored. It can remove the need for admin intervention when banned
	/// lists contain only toxic rooms or servers that no good-faith user would
	/// try to join. The default is false because rooms may be banned for
	/// non-moderation reasons and this performs full account deactivation.
	///
	/// reloadable: yes
	/// legacy: auto_deactivate_banned_room_attempts
	#[serde(default)]
	pub auto_deactivate_banned_room_attempts: bool,

	/// Forbids room aliases and room IDs matching listed regular expressions.
	///
	/// Patterns can use regular expressions or simple words for containment
	/// matches, as in the example. Checks run on room alias creation and
	/// custom room ID creation, with startup warnings for existing database
	/// aliases with forbidden aliases or IDs.
	///
	/// reloadable: yes
	/// example: ["19dollarfortnitecards", "b[4a]droom", "badphrase"]
	///
	/// default: []
	/// legacy: forbidden_alias_names
	#[serde(default, with = "serde_regex")]
	pub forbidden_alias_names: RegexSet,

	/// Deletes rooms after the last local user leaves.
	///
	/// This experimental feature is disabled by default to avoid surprises,
	/// but deployments conserving space may enable it; it may default to true
	/// in a future release. Some paths that remove the last local user do not
	/// yet invoke deletion, so a room can remain afterward.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: delete_rooms_after_leave
	#[serde(default)]
	pub delete_after_leave: bool,

	/// Settings for rooms extremities.
	///
	/// display: section
	#[serde(default)]
	pub extremities: Extremities,

	/// Settings for rooms invites.
	///
	/// display: section
	#[serde(default)]
	pub invites: Invites,

	/// Settings for rooms joins.
	///
	/// display: section
	#[serde(default)]
	pub joins: Joins,

	/// Settings for rooms policy server.
	///
	/// display: section
	#[serde(default)]
	pub policy_server: PolicyServer,

	/// Settings for rooms redaction.
	///
	/// display: section
	#[serde(default)]
	pub redaction: Redaction,

	/// Settings for rooms relations.
	///
	/// display: section
	#[serde(default)]
	pub relations: Relations,
}

/// Configures forward-extremity pruning for incoming federation events.
///
/// A soft cap and emergency bound control the tracked frontier. Batch
/// limits pace convergence across events.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rooms.extremities",
	group = "true"
)]
pub struct Extremities {
	/// Sets the soft cap on forward extremities tracked per room.
	///
	/// When applying an incoming federation event would exceed this cap, we
	/// prune the least useful leaves from the tracked frontier until it meets
	/// the cap. Pruned events remain stored and referenceable by other
	/// servers; we only stop citing them as frontier tips. Events created by
	/// this server are never pruned, and 0 disables automatic pruning.
	///
	/// reloadable: yes
	/// default: 60
	/// legacy: forward_extremities_max
	#[serde(default = "default_max")]
	pub max: usize,

	/// Sets the emergency bound on a room's forward extremities.
	///
	/// A larger frontier is cut down to this bound in one step, ignoring the
	/// per-event pruning batch limit. Values at or below
	/// `rooms.extremities.max` remove pacing entirely and prune straight to
	/// the cap.
	///
	/// reloadable: yes
	/// default: 256
	/// legacy: forward_extremities_emergency_max
	#[serde(default = "default_emergency_max")]
	pub emergency_max: usize,

	/// Limits forward extremities pruned by one incoming event between the
	/// soft cap and emergency bound.
	///
	/// This spreads convergence across events to bound each event's work. A
	/// value of 0 stops paced pruning and leaves only the emergency bound.
	///
	/// reloadable: yes
	/// default: 32
	/// legacy: forward_extremities_prune_batch
	#[serde(default = "default_prune_batch")]
	pub prune_batch: usize,
}

/// Configures room invitation policy.
///
/// Restrictions control non-admin invitations. Automatic acceptance can be
/// limited to direct messages or local senders.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rooms.invites",
	group = "true"
)]
pub struct Invites {
	/// Blocks non-admin local users from sending local or remote invites and
	/// non-admin users from receiving remote invites.
	///
	/// Admins can always send and receive all room invites.
	///
	/// reloadable: yes
	/// legacy: block_non_admin_invites
	#[serde(default)]
	pub block_non_admin: bool,

	/// Automatically joins local users to any room they are invited to.
	///
	/// The join runs in the background once the invite is recorded, and is
	/// retried for a short while when a federated invite has not yet settled
	/// on the inviting server. Users who are deactivated, suspended, or locked
	/// are never joined.
	///
	/// reloadable: yes
	/// legacy: auto_accept_invites
	#[serde(default)]
	pub auto_accept: bool,

	/// Restricts automatic invite acceptance to direct-message invites.
	///
	/// This restricts `rooms.invites.auto_accept`. The invite must carry
	/// `is_direct` in its membership content, which clients set when starting
	/// a direct chat. Invites to any other room are left for the user to
	/// answer.
	///
	/// reloadable: yes
	/// legacy: auto_accept_invites_direct_only
	#[serde(default)]
	pub auto_accept_direct_only: bool,

	/// Restricts automatic invite acceptance to invites sent by local users.
	///
	/// This restricts `rooms.invites.auto_accept`. Invites arriving from
	/// another server are left for the user to answer.
	///
	/// reloadable: yes
	/// legacy: auto_accept_invites_local_only
	#[serde(default)]
	pub auto_accept_local_only: bool,
}

/// Configures room-join server selection and retries.
///
/// Deprioritized servers are tried last. Attempt limits bound requests
/// before reporting a join failure.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rooms.joins",
	group = "true"
)]
pub struct Joins {
	/// Lists server names to try last when joining rooms.
	///
	/// Client-requested joins through these servers try them last, which can
	/// prevent failed joins caused by a particular server's timeouts.
	///
	/// reloadable: yes
	/// default: ["matrix\.org"]
	/// config-example: ["matrix\\.org"]
	/// legacy: deprioritize_joins_through_servers
	#[serde(
		default = "default_deprioritize_servers",
		with = "serde_regex"
	)]
	pub deprioritize_servers: RegexSet,

	/// Limits `make_join` requests within each join attempt.
	///
	/// Each request tries a different server, with each server tried only once
	/// per attempt. Servers can be retried when the entire join request is
	/// retried.
	///
	/// reloadable: yes
	/// default: 48
	/// legacy: max_make_join_attempts_per_join_attempt
	#[serde(default = "default_max_make_join_per_attempt")]
	pub max_make_join_per_attempt: usize,

	/// Limits join attempts per client join request.
	///
	/// Each attempt contains one or more `make_join` requests bounded by
	/// `rooms.joins.max_make_join_per_attempt` and one `send_join` request.
	/// Additional attempts let more servers act as the join server before
	/// returning the last error to the client.
	/// The default exceeds one while limiting the risk of exceeding the
	/// client's request timeout, although exceeding it may sometimes be
	/// unavoidable.
	///
	/// reloadable: yes
	/// default: 3
	/// legacy: max_join_attempts_per_join_request
	#[serde(default = "default_max_attempts_per_request")]
	pub max_attempts_per_request: usize,
}

/// Configures room policy-server requests.
///
/// The enablement setting controls outgoing event-signature requests. The
/// timeout also bounds inbound signature fetching.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rooms.policy_server",
	group = "true"
)]
pub struct PolicyServer {
	/// Requests policy-server signatures for outgoing events under
	/// [MSC4284](https://github.com/matrix-org/matrix-spec-proposals/pull/4284).
	///
	/// For a room with a valid `m.room.policy` state event, we request a
	/// signature from its policy server's federation `/sign` endpoint before
	/// federating each event. Refusal aborts the local request; network and
	/// timeout failures fail open with a warning log so a transient outage
	/// does not silently take the room offline.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: enable_policy_servers
	#[serde(default)]
	pub enable: bool,

	/// Sets the policy-server request timeout in seconds under
	/// [MSC4284](https://github.com/matrix-org/matrix-spec-proposals/pull/4284).
	///
	/// This applies to outbound `/sign` calls and inbound signature fetches.
	///
	/// reloadable: yes
	/// default: 5
	/// legacy: policy_server_request_timeout
	#[serde(default = "default_request_timeout")]
	pub request_timeout: u64,
}

/// Configures redaction, retained originals, and erasure.
///
/// Retention and access policies govern original events. Thread-scan limits
/// bound summary correction after redaction.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rooms.redaction",
	group = "true"
)]
pub struct Redaction {
	/// Limits relation rows read by each thread scan during redaction.
	///
	/// Replacing a redacted latest thread reply, and correcting each of a
	/// member's unread counters after a redacted reply, each read at most this
	/// many rows; missing, malformed, unrelated, and redacted events consume
	/// the budget before event filtering. When it runs out, the thread summary
	/// is omitted and the thread's unread count may stay one too high until a
	/// thread or unthreaded receipt clears it.
	///
	/// reloadable: yes
	/// default: 1024
	/// legacy: thread_latest_reply_search_limit
	#[serde(default = "default_thread_latest_reply_search_limit")]
	pub thread_latest_reply_search_limit: usize,

	/// Saves original events before applying redactions.
	///
	/// Retrieve them with `admin debug get-retained-pdu` or
	/// [MSC2815](https://github.com/matrix-org/matrix-spec-proposals/pull/2815).
	///
	/// reloadable: yes
	/// default: true
	/// legacy: save_unredacted_events
	#[serde(default = "true_fn")]
	pub save_unredacted_events: bool,

	/// Sets the redaction retention period in seconds.
	///
	/// By default the unredacted events are stored for 60 days.
	///
	/// reloadable: yes
	/// default: 5184000
	/// legacy: redaction_retention_seconds
	#[serde(default = "default_retention_seconds")]
	pub retention_seconds: u64,

	/// Allows users with `redact` power level to request unredacted events
	/// under
	/// [MSC2815](https://github.com/matrix-org/matrix-spec-proposals/pull/2815).
	///
	/// Server admins can request unredacted events regardless of this setting.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: allow_room_admins_to_request_unredacted_events
	#[serde(default = "true_fn")]
	pub allow_room_admins_to_request_unredacted_events: bool,

	/// Prevents local users from sending redactions.
	///
	/// This check does not apply to server admins.
	///
	/// reloadable: yes
	/// legacy: disable_local_redactions
	#[serde(default)]
	pub disable_local: bool,

	/// Serves erased senders' events as pruned copies over federation under
	/// [MSC4025](https://github.com/matrix-org/matrix-spec-proposals/pull/4025).
	///
	/// A requesting server retains the unredacted view only when one of its
	/// users was joined in the room state at the event. Join handshakes are
	/// not gated.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: enforce_erasure_over_federation
	#[serde(default = "true_fn")]
	pub enforce_erasure_over_federation: bool,
}

/// Configures optional relation bundles in client responses.
///
/// Edit and reference bundles add summaries to served events. Each bundle
/// has an independent enablement setting.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.rooms.relations",
	group = "true"
)]
pub struct Relations {
	/// Bundles the latest message edit under
	/// [MSC3925](https://github.com/matrix-org/matrix-spec-proposals/pull/3925).
	///
	/// Client read endpoints fold the most recent `m.replace` relation into
	/// `unsigned.m.relations` as the full replacement event. This is opt-in
	/// and disabled by default because it adds a typed index seek per served
	/// event and a server-authoritative summary most clients reconstruct
	/// locally.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: bundle_edit_relations
	#[serde(default)]
	pub bundle_edits: bool,

	/// Bundles reference relations under
	/// [MSC2675](https://github.com/matrix-org/matrix-spec-proposals/pull/2675)
	/// and
	/// [MSC3267](https://github.com/matrix-org/matrix-spec-proposals/pull/3267).
	///
	/// Client read endpoints fold `m.reference` relations into
	/// `unsigned.m.relations` as `{ chunk: [{ event_id }, ...] }`. This is
	/// disabled by default and adds read-time work when enabled; clients can
	/// resolve references for polls, beacons, and verification directly.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: bundle_reference_relations
	#[serde(default)]
	pub bundle_references: bool,
}

impl Default for Rooms {
	fn default() -> Self {
		Self {
			federate_by_default: true_fn(),
			forget_forced_upon_leave: Default::default(),
			allow_creation: true_fn(),
			allow_alias_creation: true_fn(),
			allow_unstable_versions: true_fn(),
			allow_experimental_versions: Default::default(),
			default_version: default_default_version(),
			default_power_level_content_override: Default::default(),
			auto_deactivate_banned_room_attempts: Default::default(),
			forbidden_alias_names: Default::default(),
			delete_after_leave: Default::default(),
			extremities: Extremities::default(),
			invites: Invites::default(),
			joins: Joins::default(),
			policy_server: PolicyServer::default(),
			redaction: Redaction::default(),
			relations: Relations::default(),
		}
	}
}

impl Default for Extremities {
	fn default() -> Self {
		Self {
			max: default_max(),
			emergency_max: default_emergency_max(),
			prune_batch: default_prune_batch(),
		}
	}
}

impl Default for Joins {
	fn default() -> Self {
		Self {
			deprioritize_servers: default_deprioritize_servers(),
			max_make_join_per_attempt: default_max_make_join_per_attempt(),
			max_attempts_per_request: default_max_attempts_per_request(),
		}
	}
}

impl Default for PolicyServer {
	fn default() -> Self {
		Self {
			enable: Default::default(),
			request_timeout: default_request_timeout(),
		}
	}
}

impl Default for Redaction {
	fn default() -> Self {
		Self {
			thread_latest_reply_search_limit: default_thread_latest_reply_search_limit(),
			save_unredacted_events: true_fn(),
			retention_seconds: default_retention_seconds(),
			allow_room_admins_to_request_unredacted_events: true_fn(),
			disable_local: Default::default(),
			enforce_erasure_over_federation: true_fn(),
		}
	}
}

/// Returns the default Matrix room version.
///
/// Room version 12 is selected when `rooms.default_version` is omitted. The
/// value is returned without consulting runtime configuration.
#[must_use]
#[inline]
pub fn default_default_version() -> RoomVersionId { RoomVersionId::V12 }

fn default_max() -> usize { 60 }

fn default_emergency_max() -> usize { 256 }

fn default_prune_batch() -> usize { 32 }

fn default_deprioritize_servers() -> RegexSet {
	RegexSet::new([r"matrix\.org"]).expect("valid set of regular expressions")
}

fn default_max_make_join_per_attempt() -> usize { 48 }

fn default_max_attempts_per_request() -> usize { 3 }

fn default_request_timeout() -> u64 { 5 }

fn default_thread_latest_reply_search_limit() -> usize { 1024 }

fn default_retention_seconds() -> u64 { 5_184_000 }
