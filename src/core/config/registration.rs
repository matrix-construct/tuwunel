//! Configures account registration and initial user setup.
//!
//! Registration tokens and shared secrets control separate account-creation
//! paths. New-user defaults and guest policy govern initial account
//! behavior.

use serde::Deserialize;
use tuwunel_macros::config_example_generator;

use super::{OwnedRoomOrAliasId, PathBuf, RegexSet, true_fn};

/// Configures account registration and initial user setup.
///
/// Registration tokens and shared secrets control separate account-creation
/// paths. New-user defaults and guest policy govern initial account
/// behavior.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Debug, Deserialize)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.registration",
	group = "true",
	ignore = "guests"
)]
pub struct Registration {
	/// Appends text to a new user's display name at registration, separated by
	/// a space.
	///
	/// Conduit used a lightning-bolt emoji for this suffix. Set to an empty
	/// string to disable it.
	///
	/// reloadable: yes
	/// default: "💕"
	/// legacy: new_user_displayname_suffix
	#[serde(default = "default_displayname_suffix")]
	pub displayname_suffix: String,

	/// Enables ordinary new account registration.
	///
	/// When false, ordinary registration is disabled; appservice registration
	/// is exempt from this switch. Without a configured token, registration
	/// without a second step requires
	/// `registration.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse`
	/// to be true. For token-only registration, configure `registration.token`
	/// or `registration.token_file`.
	///
	/// reloadable: yes
	/// legacy: allow_registration
	#[serde(default)]
	pub enable: bool,

	/// Opens registration to anyone without restrictions.
	///
	/// This makes the server vulnerable to abuse.
	///
	/// reloadable: yes
	/// legacy: yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse
	#[serde(default)]
	pub yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse: bool,

	/// Requires new users to supply this static token when creating an
	/// account.
	///
	/// Configure this value or use `registration.token_file`. If unset while
	/// `registration.enable` is true, unrestricted registration requires
	/// `registration.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse`
	/// to be true.
	///
	/// reloadable: yes
	/// example: "o&^uCtes4HPf0Vu@F20jQeeWE7"
	///
	/// display: sensitive
	/// legacy: registration_token
	pub token: Option<String>,

	/// Reads additional registration tokens from a file.
	///
	/// Separate multiple tokens with whitespace. The file must be nonempty and
	/// accessible to the server.
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.reg_token"
	/// legacy: registration_token_file
	pub token_file: Option<PathBuf>,

	/// Enables out-of-band account creation with a pre-shared secret.
	///
	/// The Synapse-style `/_synapse/admin/v1/register` endpoint requires a
	/// build with `synapse_admin` and is not routed when MAS delegation is
	/// active. Configure this value or `registration.shared_secret_file`;
	/// requests authenticate with HMAC-SHA1 keyed by the resolved secret and
	/// bypass UIAA. The file takes precedence when successfully read. Use a
	/// high-entropy secret of at least 32 bytes and protect it like a server
	/// admin's access token.
	///
	/// reloadable: yes
	/// example: "kZ2hN5pQ8wXyL4mR7tBfCgJxV3aD6sE1u"
	///
	/// display: sensitive
	/// legacy: registration_shared_secret
	pub shared_secret: Option<String>,

	/// Reads the registration shared secret from a file.
	///
	/// This takes precedence over `registration.shared_secret` and falls back
	/// to it only when the file cannot be read. Surrounding whitespace is
	/// trimmed, so a trailing newline is not part of the secret. A present but
	/// blank file resolves to no secret without falling back.
	///
	/// reloadable: yes
	/// example: "/etc/tuwunel/.reg_shared_secret"
	/// legacy: registration_shared_secret_file
	pub shared_secret_file: Option<PathBuf>,

	#[expect(clippy::doc_link_with_quotes)]
	/// Lists room IDs or aliases that eligible newly registered users
	/// automatically join.
	///
	/// The server must already participate in each room, and the user must
	/// satisfy its normal join authorization rules, including for restricted
	/// rooms. Rooms are processed in configured order, so an earlier join can
	/// satisfy a later room's membership requirement. Appservice registrations
	/// are excluded; guests require `registration.guests.allow_auto_join_rooms`.
	/// Failed joins are logged without failing registration.
	///
	/// reloadable: yes
	/// example: ["#tuwunel:grin.hu",
	/// "!l2xV0sd51lraysuRcsWVECge4NULaH3g-ou95vgDgiM"]
	///
	/// default: []
	/// legacy: auto_join_rooms
	#[serde(default = "Vec::new")]
	pub auto_join_rooms: Vec<OwnedRoomOrAliasId>,

	/// Forbids usernames matching listed patterns.
	///
	/// Use regular expressions or plain words for containment matches, as in
	/// the example. Checks run during username-availability requests and
	/// registration, with startup warnings for existing local users whose
	/// names match.
	///
	/// reloadable: yes
	/// example: ["administrator", "b[a4]dusernam[3e]", "badphrase"]
	///
	/// default: []
	/// legacy: forbidden_usernames
	#[serde(default, with = "serde_regex")]
	pub forbidden_usernames: RegexSet,

	/// Grants admin privileges during eligible registration by joining the
	/// user to the admin room.
	///
	/// The registering user must be neither a guest nor an appservice user,
	/// and the registration caller must permit the first-user grant. The admin
	/// room must exist and have exactly one joined member, normally the server
	/// user; an empty admin room does not qualify.
	///
	/// reloadable: yes
	/// default: true
	/// legacy: grant_admin_to_first_user
	#[serde(default = "true_fn")]
	pub grant_admin_to_first_user: bool,

	/// Settings for registration guests.
	///
	/// display: section
	#[serde(default)]
	pub guests: Guests,
}

/// Configures guest account registration.
///
/// Guest registration and automatic room joining have separate controls.
/// Automatic joins use the ordinary registration room list; some clients,
/// including Element, try to register guests automatically.
#[expect(rustdoc::broken_intra_doc_links, rustdoc::bare_urls)]
#[derive(Clone, Copy, Debug, Deserialize, Default)]
#[config_example_generator(
	filename = "tuwunel-example.toml",
	section = "global.registration.guests",
	group = "true"
)]
pub struct Guests {
	/// Allows registrations with user type "guest".
	///
	/// reloadable: yes
	/// legacy: allow_guest_registration
	#[serde(default)]
	pub enable: bool,

	/// Allows guest users to automatically join rooms listed in
	/// `registration.auto_join_rooms`.
	///
	/// reloadable: yes
	/// legacy: allow_guests_auto_join_rooms
	#[serde(default)]
	pub allow_auto_join_rooms: bool,
}

impl Default for Registration {
	fn default() -> Self {
		Self {
			displayname_suffix: default_displayname_suffix(),
			enable: Default::default(),
			yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse:
				Default::default(),
			token: None,
			token_file: None,
			shared_secret: None,
			shared_secret_file: None,
			auto_join_rooms: Vec::new(),
			forbidden_usernames: Default::default(),
			grant_admin_to_first_user: true_fn(),
			guests: Guests::default(),
		}
	}
}

fn default_displayname_suffix() -> String { "💕".to_owned() }
