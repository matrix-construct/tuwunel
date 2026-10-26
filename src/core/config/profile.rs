//! Configures profile access, updates, and remote caching.
//!
//! Visibility settings distinguish client and federation requests. Field
//! limits bound cached profiles and requested selections.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::true_fn;

/// Configures profile access, updates, and remote caching.
///
/// Visibility settings distinguish client and federation requests. Field
/// limits bound cached profiles and requested selections.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.profile",
	group = "true"
)]
pub struct Profile {
	/// Requires authentication on profile retrieval endpoints when enabled.
	///
	/// This covers normally unauthenticated GET requests to
	/// `/_matrix/client/v3/profile/{userId}` and can prevent profile scraping.
	///
	/// reloadable: yes
	/// legacy: require_auth_for_profile_requests
	#[serde(default)]
	pub require_auth: bool,

	/// Limits fields retained in a remote user's cached profile.
	///
	/// The count includes `displayname` and `avatar_url` and must exceed zero.
	/// A refresh exceeding it is rejected without changing the cache. Lowering
	/// the limit leaves existing profiles intact, but subsequent refreshes
	/// must fit.
	///
	/// reloadable: yes
	/// default: 100
	/// legacy: max_remote_profile_fields
	#[serde(default = "default_max_remote_fields")]
	pub max_remote_fields: usize,

	/// Limits selected profile fields in a sync request or stored filter.
	///
	/// This positive limit counts raw selectors before deduplication in
	/// sliding and legacy sync; requests exceeding it or the 255-byte name
	/// limit are rejected. Sliding sync retains normalized selections when
	/// selectors are omitted, so after lowering the limit an oversized
	/// selection needs replacement or a restarted connection. Unfiltered
	/// sliding-sync profiles are not limited by this count.
	///
	/// reloadable: yes
	/// default: 64
	/// legacy: max_profile_fields_per_request
	#[serde(default = "default_max_fields_per_request")]
	pub max_fields_per_request: usize,

	/// Allows standard users to set or clear display names through the client
	/// profile API.
	///
	/// Server admins and appservices can always change display names.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: enable_set_displayname
	#[serde(default = "true_fn")]
	pub enable_set_displayname: bool,

	/// Restricts client profile retrieval to the user themselves or users
	/// sharing a currently joined room.
	///
	/// Appservices are exempt; other users receive 403 `M_FORBIDDEN`. Startup
	/// and configuration reload refuse this setting unless
	/// `profile.require_auth` is also enabled. Federation profile lookups are
	/// unaffected and remain controlled by
	/// `profile.allow_inbound_federation_lookups`.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: limit_profile_requests_to_users_who_share_rooms
	#[serde(default)]
	pub limit_requests_to_users_who_share_rooms: bool,

	/// Preserves per-room profile overrides when no propagation mode is
	/// supplied to the profile service.
	///
	/// For such calls, true, the default, skips display-name or avatar changes
	/// in rooms where that field differs from the prior global profile,
	/// preserving overrides such as `/myroomnick`; false applies the new
	/// global value throughout the joined rooms. The client profile endpoints
	/// supply an explicit mode and default to `all`, so this setting does not
	/// protect overrides through those endpoints. Clients can request `all`,
	/// `unchanged`, or `none` with the
	/// [MSC4466](https://github.com/matrix-org/matrix-spec-proposals/pull/4466)
	/// `computer.gingershaped.msc4466.propagate_to` query parameter.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: preserve_room_profile_overrides
	#[serde(default = "true_fn")]
	pub preserve_room_overrides: bool,

	/// Allows device display names to be federated and seen by external users.
	///
	/// This is inherently false when `federation.enable` is disabled. Leave
	/// disabled for privacy.
	///
	/// reloadable: yes
	/// legacy: allow_device_name_federation
	#[serde(default)]
	pub allow_device_name_federation: bool,

	/// Allows incoming federation profile lookups for local users.
	///
	/// Disabling `/_matrix/federation/v1/query/profile` lookups protects local
	/// profile information such as display names and can deter scraping, but
	/// remote users may see a false "this user does not exist" error when
	/// inviting local users to a room or direct message. This is inherently
	/// false when `federation.enable` is disabled.
	///
	/// reloadable: yes
	/// legacy: allow_inbound_profile_lookup_federation_requests allow_profile_lookup_federation_requests
	#[serde(
		default = "true_fn",
		alias = "allow_profile_lookup_federation_requests"
	)]
	pub allow_inbound_federation_lookups: bool,
}

impl Default for Profile {
	fn default() -> Self {
		Self {
			require_auth: Default::default(),
			max_remote_fields: default_max_remote_fields(),
			max_fields_per_request: default_max_fields_per_request(),
			enable_set_displayname: true_fn(),
			limit_requests_to_users_who_share_rooms: Default::default(),
			preserve_room_overrides: true_fn(),
			allow_device_name_federation: Default::default(),
			allow_inbound_federation_lookups: true_fn(),
		}
	}
}

fn default_max_remote_fields() -> usize { 100 }

fn default_max_fields_per_request() -> usize { 64 }
