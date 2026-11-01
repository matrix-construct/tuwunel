//! Configures room and user discovery.
//!
//! Room-directory settings govern publication and searching. User-directory
//! settings govern which local users can appear.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{RegexSet, true_fn};

/// Room and user directories
///
/// Groups the two discovery directories. `[global.directory.rooms]` covers
/// the public room directory: who can list it, who can publish to it, and
/// how it is searched. `[global.directory.users]` covers which accounts a
/// user search returns.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize, Default)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.directory",
	group = "true",
	ignore = "rooms users"
)]
pub struct Directory {
	/// Settings for directory rooms.
	///
	/// display: section
	#[serde(default)]
	pub rooms: Rooms,

	/// Settings for directory users.
	///
	/// display: section
	#[serde(default)]
	pub users: Users,
}

/// Public room directory
///
/// Controls who can read this server's public room directory, who can
/// publish rooms to it, and how searches behave. By default only clients
/// with an access token can list it, other servers cannot query it over
/// federation, and any user whose power level in a room allows changing its
/// history visibility can publish that room (guests cannot).
///
/// Tips:
/// - `lockdown` limits publishing to server admins and appservices, both
///   when a room is created and afterwards.
/// - Servers that `[global.federation]` already blocks, through its
///   `forbidden_remote_server_names` or its allowlist, are refused here
///   too; this section's `forbidden_remote_server_names` adds blocks that
///   apply only to browsing a remote server's directory.
/// - `forbidden_remote_server_names` takes regular expressions; startup
///   warns about a plain dotted name such as `example.com`, whose dots
///   match any character.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.directory.rooms",
	group = "true"
)]
pub struct Rooms {
	/// Allows federation access to the public room directory.
	///
	/// Disabling this protects against `/publicRooms` spiders but prevents
	/// external users from viewing the directory. This is inherently false
	/// when `federation.enable` is disabled.
	///
	/// reloadable: yes
	/// legacy: allow_public_room_directory_over_federation
	#[serde(default)]
	pub allow_over_federation: bool,

	/// Allows client API queries of the public room directory without an
	/// access token, exposing it to unauthenticated `/publicRooms` spiders.
	///
	/// reloadable: yes
	/// legacy: allow_public_room_directory_without_auth
	#[serde(default)]
	pub allow_without_auth: bool,

	/// Allows directory searches to match partial room IDs when the search
	/// starts with "!".
	///
	/// reloadable: yes
	/// default: true
	/// legacy: allow_public_room_search_by_id
	#[serde(default = "true_fn")]
	pub allow_public_search_by_id: bool,

	/// Allows room-ID searches to return joinable rooms absent from public
	/// listings.
	///
	/// The default true lets users paste room IDs into search boxes to join,
	/// matching their expectations even when rooms opt out of public listings.
	/// Set false to restrict results to rooms discoverable through aliases or
	/// other queries, specifically those in the public directory. To prevent
	/// abuse, several room-ID characters must be known before results are
	/// returned.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: allow_unlisted_room_search_by_id
	#[serde(default = "true_fn")]
	pub allow_unlisted_search_by_id: bool,

	/// Restricts publishing rooms in the public directory to admins.
	///
	/// All users can still unpublish rooms when this is enabled.
	///
	/// reloadable: yes
	/// legacy: lockdown_public_room_directory
	#[serde(default)]
	pub lockdown: bool,

	/// Blocks outgoing federated room-directory requests to server names
	/// matching listed regular expressions to prevent users from wandering
	/// into unwanted servers or spaces.
	///
	/// reloadable: yes
	/// example: ['badserver\.tld$', 'badphrase', '19dollarfortnitecards']
	///
	/// default: []
	/// legacy: forbidden_remote_room_directory_server_names
	#[serde(default, with = "serde_regex")]
	pub forbidden_remote_server_names: RegexSet,
}

/// User directory
///
/// Controls which accounts a local user's directory search can find. The
/// search covers local accounts and the remote users this server has seen
/// in its rooms. By default it returns only users who are in a room with a
/// public join rule or share a room with the searcher, and it hides users
/// that belong exclusively to an appservice. Other servers cannot query
/// it; federation profile lookups are set in `[global.profile]`.
///
/// Tips:
/// - `show_all_local` lifts the room filter for every account the server
///   knows, including those remote users, not only local accounts.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize, Default)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.directory.users",
	group = "true"
)]
pub struct Users {
	/// Shows all local users in directory searches.
	///
	/// When false, only users in public rooms or rooms shared with the
	/// searcher appear. Appservice senders and users in exclusive appservice
	/// namespaces remain hidden unless `directory.users.show_appservice` is
	/// also enabled.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: show_all_local_users_in_user_directory
	#[serde(default)]
	pub show_all_local: bool,

	/// Includes appservice senders and users in exclusive appservice
	/// namespaces in directory searches.
	///
	/// Normal room-visibility rules still apply unless
	/// `directory.users.show_all_local` is also enabled.
	///
	/// reloadable: yes
	/// default: false
	/// legacy: show_appservice_users_in_user_directory
	#[serde(default)]
	pub show_appservice: bool,
}

impl Default for Rooms {
	fn default() -> Self {
		Self {
			allow_over_federation: Default::default(),
			allow_without_auth: Default::default(),
			allow_public_search_by_id: true_fn(),
			allow_unlisted_search_by_id: true_fn(),
			lockdown: Default::default(),
			forbidden_remote_server_names: Default::default(),
		}
	}
}
