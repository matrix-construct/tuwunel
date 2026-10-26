//! Configures room and user discovery.
//!
//! Room-directory settings govern publication and searching. User-directory
//! settings govern which local users can appear.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{RegexSet, true_fn};

/// Configures room and user discovery.
///
/// Room-directory settings govern publication and searching. User-directory
/// settings govern which local users can appear.
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

/// Configures room-directory publication and searches.
///
/// Client and federation access have separate controls. Search policy
/// determines when room IDs can find listed or unlisted rooms.
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
	/// example: ["badserver\.tld$", "badphrase", "19dollarfortnitecards"]
	///
	/// default: []
	/// legacy: forbidden_remote_room_directory_server_names
	#[serde(default, with = "serde_regex")]
	pub forbidden_remote_server_names: RegexSet,
}

/// Configures local user visibility in directory searches.
///
/// Room visibility limits ordinary search results unless all local users
/// are shown. Appservice users require their separate visibility setting.
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
