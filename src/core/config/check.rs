use std::{env::consts::OS, fs::read_to_string, net::SocketAddr};

use either::Either;
use http::HeaderValue;
use itertools::Itertools;
use regex::RegexSet;
use url::Url;

use super::{DEPRECATED_KEYS, IdentityProvider, IpSource, KNOWN_KEYS};
use crate::{Config, Err, Result, debug, debug_info, err, error, warn};

/// Performs check() with additional checks specific to reloading old config
/// with new config.
pub fn reload(old: &Config, new: &Config) -> Result {
	check(new)?;

	if new.server_name != old.server_name {
		return Err!(Config(
			"server_name",
			"You can't change the server's name from {:?}.",
			old.server_name
		));
	}

	if new.ip_source != old.ip_source {
		return Err!(Config(
			"ip_source",
			"ip_source cannot be changed at runtime; restart the server to apply this change."
		));
	}

	Ok(())
}

pub fn check(config: &Config) -> Result {
	#[cfg(debug_assertions)]
	warn!("Note: tuwunel was built without optimisations (i.e. debug build)");

	warn_deprecated(config);
	warn_unknown_key(config)?;

	#[cfg(all(
		feature = "hardened_malloc",
		feature = "jemalloc",
		not(target_env = "msvc")
	))]
	debug_warn!(
		"hardened_malloc and jemalloc compile-time features are both enabled, this causes \
		 jemalloc to be used."
	);

	check_observability(config)?;
	check_network(config)?;
	check_storage(config)?;
	check_registration(config)?;
	check_registration_terms(config)?;
	check_turn_and_media_misc(config)?;
	check_url_previews(config)?;
	check_room_version(config)?;
	check_identity_providers(config)?;
	check_media_providers(config)?;
	check_well_known_support_contact_validity(config)?;
	check_email(config)?;

	Ok(())
}

fn check_observability(config: &Config) -> Result {
	if config.sentry && config.sentry_endpoint.is_none() {
		return Err!(Config(
			"sentry_endpoint",
			"Sentry cannot be enabled without an endpoint set"
		));
	}

	Ok(())
}

fn check_network(config: &Config) -> Result {
	#[cfg(not(unix))]
	if config.unix_socket_path.is_some() {
		return Err!(Config(
			"unix_socket_path",
			"UNIX socket support is only available on *nix platforms. Please remove \
			 'unix_socket_path' from your config."
		));
	}

	let certs_set = config.tls.certs.is_some();
	let key_set = config.tls.key.is_some();
	if certs_set ^ key_set {
		return Err!(Config("tls", "tls.certs and tls.key must either both be set or unset"));
	}

	// A non-zero depth shards the 64-char SHA-256 hex digest into `depth`
	// segments of `length` plus a remainder, so the product must stay below 64.
	let depth = config.conduit_media_directory_depth;
	let length = config.conduit_media_directory_length;
	if depth > 0 && length == 0 {
		return Err!(Config(
			"conduit_media_directory_length",
			"must be non-zero when conduit_media_directory_depth is non-zero"
		));
	}
	if depth > 0 && usize::from(depth).saturating_mul(usize::from(length)) >= 64 {
		return Err!(Config(
			"conduit_media_directory_depth",
			"conduit_media_directory_depth times conduit_media_directory_length must be less \
			 than 64, the length of a SHA-256 hex digest"
		));
	}

	if let Some(source) = config.ip_source
		&& !matches!(source, IpSource::ConnectInfo)
	{
		warn!(
			"ip_source is set to {source:?}, a header-based source. Ensure a trusted reverse \
			 proxy populates this header for every request; otherwise clients can spoof their \
			 IP address."
		);
	}

	if !config.listening {
		warn!("Configuration item `listening` is set to `false`. Cannot hear anyone.");
	}

	if config.unix_socket_path.is_none() {
		config
			.get_bind_addrs()
			.iter()
			.for_each(warn_loopback_in_container);
	}

	// check if user specified valid IP CIDR ranges on startup
	for cidr in &config.ip_range_denylist {
		if let Err(e) = ipaddress::IPAddress::parse(cidr) {
			return Err!(Config(
				"ip_range_denylist",
				"Parsing specified IP CIDR range from string failed: {e}."
			));
		}
	}

	Ok(())
}

fn warn_loopback_in_container(addr: &SocketAddr) {
	use std::path::Path;

	if !addr.ip().is_loopback() {
		return;
	}

	debug_info!(
		"Found loopback listening address {addr}, running checks if we're in a container."
	);

	if Path::new("/proc/vz").exists() /* Guest */ && !Path::new("/proc/bz").exists()
	/* Host */
	{
		error!(
			"You are detected using OpenVZ with a loopback/localhost listening address of \
			 {addr}. If you are using OpenVZ for containers and you use NAT-based networking to \
			 communicate with the host and guest, this will NOT work. Please change this to \
			 \"0.0.0.0\". If this is expected, you can ignore.",
		);
	} else if Path::new("/.dockerenv").exists() {
		error!(
			"You are detected using Docker with a loopback/localhost listening address of \
			 {addr}. If you are using a reverse proxy on the host and require communication to \
			 tuwunel in the Docker container via NAT-based networking, this will NOT work. \
			 Please change this to \"0.0.0.0\". If this is expected, you can ignore.",
		);
	} else if Path::new("/run/.containerenv").exists() {
		error!(
			"You are detected using Podman with a loopback/localhost listening address of \
			 {addr}. If you are using a reverse proxy on the host and require communication to \
			 tuwunel in the Podman container via NAT-based networking, this will NOT work. \
			 Please change this to \"0.0.0.0\". If this is expected, you can ignore.",
		);
	}
}

fn check_storage(config: &Config) -> Result {
	// rocksdb does not allow max_log_files to be 0
	if config.rocksdb_max_log_files == 0 {
		return Err!(Config(
			"max_log_files",
			"rocksdb_max_log_files cannot be 0. Please set a value at least 1."
		));
	}

	// yeah, unless the user built a debug build hopefully for local testing only
	#[cfg(not(debug_assertions))]
	if config.server_name == "your.server.name" {
		return Err!(Config(
			"server_name",
			"You must specify a valid server name for production usage of tuwunel."
		));
	}

	Ok(())
}

fn check_registration(config: &Config) -> Result {
	if config
		.emergency_password
		.as_ref()
		.is_some_and(|emergency_password| emergency_password == "F670$2CP@Hw8mG7RY1$%!#Ic7YA")
	{
		return Err!(Config(
			"emergency_password",
			"The public example emergency password is being used, this is insecure. Please \
			 change this."
		));
	}

	if config
		.emergency_password
		.as_ref()
		.is_some_and(String::is_empty)
	{
		return Err!(Config(
			"emergency_password",
			"Emergency password was set to an empty string, this is not valid. Unset \
			 emergency_password to disable it or set it to a real password."
		));
	}

	if config
		.registration_token
		.as_ref()
		.is_some_and(String::is_empty)
	{
		return Err!(Config(
			"registration_token",
			"Registration token was specified but is empty (\"\")"
		));
	}

	// check if we can read the token file path, and check if the file is empty
	if config
		.registration_token_file
		.as_ref()
		.is_some_and(|path| {
			let Ok(token) = read_to_string(path).inspect_err(|e| {
				error!("Failed to read the registration token file: {e}");
			}) else {
				return true;
			};

			token == String::new()
		}) {
		return Err!(Config(
			"registration_token_file",
			"Registration token file was specified but is empty or failed to be read"
		));
	}

	let no_token =
		config.registration_token.is_none() && config.registration_token_file.is_none();

	if config.allow_registration
		&& no_token
		&& !config.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse
	{
		return Err!(Config(
			"registration_token",
			"!! You have `allow_registration` enabled without a token configured in your config \
			 which means you are allowing ANYONE to register on your tuwunel instance without \
			 any 2nd-step (e.g. registration token). If this is not the intended behaviour, \
			 please set a registration token. For security and safety reasons, tuwunel will \
			 shut down. If you are extra sure this is the desired behaviour you want, please \
			 set the following config option to true:
`yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse`"
		));
	}

	if config.allow_registration
		&& no_token
		&& config.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse
	{
		warn!(
			"Open registration is enabled via setting \
			 `yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse` and \
			 `allow_registration` to true without a registration token configured. You are \
			 expected to be aware of the risks now. If this is not the desired behaviour, \
			 please set a registration token."
		);
	}

	Ok(())
}

fn check_registration_terms(config: &Config) -> Result {
	for (id, policy) in &config.registration_terms {
		let opaque = !id.is_empty()
			&& id.len() <= 255
			&& id.bytes().all(
				|b| matches!(b, b'0'..=b'9' | b'a'..=b'z' | b'A'..=b'Z' | b'.' | b'_' | b'~' | b'-'),
			);

		if !opaque {
			return Err!(Config(
				"registration_terms",
				"Policy id {id:?} must be a non-empty opaque identifier of at most 255 \
				 characters from [0-9a-zA-Z._~-]."
			));
		}

		for (lang, translation) in &policy.translations {
			if !matches!(translation.url.scheme(), "http" | "https") {
				return Err!(Config(
					"registration_terms",
					"Policy {id:?} translation {lang:?} url must use the http or https scheme."
				));
			}
		}
	}

	Ok(())
}

fn check_turn_and_media_misc(config: &Config) -> Result {
	if !config.turn_uris.is_empty()
		&& config.turn_secret.is_none()
		&& config.turn_secret_file.is_none()
		&& config.turn_username.is_empty()
		&& config.turn_password.is_empty()
	{
		warn!(
			"turn_uris is configured but no credential source is set; the endpoint \
			 /_matrix/client/v3/voip/turnServer will return empty username and password. Set \
			 turn_secret, turn_secret_file, or both turn_username and turn_password."
		);
	}

	if config.max_request_size < 10_000_000 {
		return Err!(Config(
			"max_request_size",
			"Max request size is less than 10MB. Please increase it as this is too low for \
			 operable federation."
		));
	}

	if config.allow_outgoing_presence && !config.allow_local_presence {
		return Err!(Config(
			"allow_local_presence",
			"Outgoing presence requires allowing local presence. Please enable \
			 'allow_local_presence' or disable outgoing presence."
		));
	}

	if config.suppress_push_when_active {
		warn!(
			"Push suppression when active is enabled (EXPERIMENTAL): behavior may change or be \
			 unstable. Disable by removing or setting suppress_push_when_active to false."
		);
	}

	Ok(())
}

fn check_url_previews(config: &Config) -> Result {
	let wildcard = "*".to_owned();
	let url_preview_wildcards = [
		(
			"url_preview_domain_contains_allowlist",
			&config.url_preview_domain_contains_allowlist,
		),
		(
			"url_preview_domain_explicit_allowlist",
			&config.url_preview_domain_explicit_allowlist,
		),
		("url_preview_url_contains_allowlist", &config.url_preview_url_contains_allowlist),
	];

	for (name, list) in url_preview_wildcards {
		if list.contains(&wildcard) {
			warn!(
				"All URLs are allowed for URL previews via setting \"{name}\" to \"*\". This \
				 opens up significant attack surface to your server. You are expected to be \
				 aware of the risks by doing this."
			);
		}
	}

	if let Some(Either::Right(_)) = config.url_preview_bound_interface.as_ref()
		&& !matches!(OS, "android" | "fuchsia" | "linux")
	{
		return Err!(Config(
			"url_preview_bound_interface",
			"Not a valid IP address. Interface names not supported on {OS}."
		));
	}

	if let Some(user_agent) = config.url_preview_user_agent.as_deref()
		&& HeaderValue::from_str(user_agent).is_err()
	{
		return Err!(Config("url_preview_user_agent", "Not a valid HTTP header value."));
	}

	Ok(())
}

fn check_room_version(config: &Config) -> Result {
	if !config.supported_room_version(&config.default_room_version) {
		return Err!(Config(
			"default_room_version",
			"Room version {:?} is not available",
			config.default_room_version
		));
	}

	if config
		.default_power_level_content_override
		.as_ref()
		.is_some_and(|value| !value.is_object())
	{
		return Err!(Config(
			"default_power_level_content_override",
			"must be a table (a JSON object)"
		));
	}

	Ok(())
}

fn check_identity_providers(config: &Config) -> Result {
	for a in config.identity_provider.values() {
		let count = config
			.identity_provider
			.values()
			.filter(|b| a.id().eq(b.id()))
			.count();

		debug_assert_ne!(count, 0, "expected at least one identity_provider");
		if count > 1 {
			return Err!(Config(
				"client_id",
				"Duplicate identity_provider with client_id {}",
				a.client_id
			));
		}
	}

	for (i, provider) in &config.identity_provider {
		check_identity_provider_secret(i, provider)?;
	}

	if !config.sso_custom_providers_page
		&& config.identity_provider.len() > 1
		&& config
			.identity_provider
			.values()
			.filter(|idp| idp.default)
			.count()
			.eq(&0)
	{
		let default = config
			.identity_provider
			.values()
			.next()
			.map(IdentityProvider::id)
			.expect("Check at least one provider is configured to reach here");

		warn!(
			"More than one identity_provider has been configured without any default selected. \
			 To prevent this warning set `default = true` for one provider. Considering \
			 {default} the default for now..."
		);
	}

	Ok(())
}

fn check_identity_provider_secret(i: &str, provider: &IdentityProvider) -> Result {
	if provider.client_secret.is_some() {
		return Ok(());
	}

	let Some(secret_path) = &provider.client_secret_file else {
		return Err!(Config(
			"client_secret",
			"Either client secret or a client secret file must be set on identity provider №{i}."
		));
	};

	let secret = read_to_string(secret_path).map_err(|e| {
		err!(Config(
			"client_secret_file",
			"Failed to read client secret file {secret_path:?} on identity provider №{i}: {e}"
		))
	})?;

	if secret.trim().is_empty() {
		return Err!(Config(
			"client_secret_file",
			"Client secret file {secret_path:?} is empty on identity provider №{i}"
		));
	}

	Ok(())
}

fn check_media_providers(config: &Config) -> Result {
	for provider in &config.store_media_on_providers {
		if !config.media_storage_providers.contains(provider) {
			return Err!(Config(
				"store_media_on_providers",
				"Providers must be listed in 'media_storage_providers'"
			));
		}
	}

	if config
		.media_storage_providers
		.iter()
		.filter(|&provider| {
			if config.storage_provider.contains_key(provider) || provider == "media" {
				return false;
			}

			error!("`media_storage_providers` references non-existent provider {provider:?}");
			true
		})
		.count()
		.gt(&0)
	{
		return Err!(Config(
			"media_storage_providers",
			"Contains missing or unconfigured storage providers."
		));
	}

	if config.media_storage_providers.len() > 1 && config.store_media_on_providers.is_empty() {
		warn!(
			"Media will be duplicated to multiple providers {:?} until \
			 `store_media_on_providers` is configured. This warning can be suppressed by \
			 explicitly configuring `store_media_on_providers`",
			config.media_storage_providers
		);
	}

	Ok(())
}

fn check_well_known_support_contact_validity(config: &Config) -> Result {
	let well_known = &config.well_known;

	if well_known.support_role.is_some()
		&& well_known.support_email.is_none()
		&& well_known.support_mxid.is_none()
	{
		return Err!(
			"well_known.support_role is set but neither support_email nor support_mxid is \
			 configured to accompany it"
		);
	}

	if let Some(pgp_key) = well_known.support_pgp_key.as_deref() {
		validate_pgp_key(pgp_key).map_err(|e| err!("well_known.support_pgp_key: {e}"))?;
	}

	for (id, contact) in &well_known.support_contact {
		if contact.email_address.is_none() && contact.matrix_id.is_none() {
			return Err!(
				"well_known.support_contact.{id} has neither email_address nor matrix_id; at \
				 least one is required"
			);
		}

		if let Some(pgp_key) = contact.pgp_key.as_deref() {
			validate_pgp_key(pgp_key)
				.map_err(|e| err!("well_known.support_contact.{id}.pgp_key: {e}"))?;
		}
	}

	Ok(())
}

fn check_email(config: &Config) -> Result {
	let smtp = &config.smtp;

	if smtp.connection_uri.is_some() && config.well_known.client.is_none() {
		return Err!(Config(
			"well_known.client",
			"global.smtp is configured but well_known.client is unset. Email verification links \
			 are built from the public client base URL, so set well_known.client to a valid \
			 HTTPS URL alongside global.smtp."
		));
	}

	if smtp.connection_uri.is_none()
		&& (smtp.require_email_for_registration || smtp.require_email_for_token_registration)
	{
		return Err!(Config(
			"smtp.connection_uri",
			"global.smtp requires a verified email at registration but smtp.connection_uri is \
			 unset. Set smtp.connection_uri so verification mail can be sent, or unset \
			 require_email_for_registration and require_email_for_token_registration."
		));
	}

	Ok(())
}

/// Validates an MSC4439 `pgp_key`: a URI, never inline key material.
fn validate_pgp_key(value: &str) -> Result {
	if value.contains("BEGIN PGP") {
		return Err!(
			"must be a URI, not inlined key material; publish the key and reference it by URI \
			 (for example https://example.com/key.asc or openpgp4fpr:<fingerprint>)"
		);
	}

	let uri = Url::parse(value).map_err(|_| {
		err!("must be a URI; a bare fingerprint must be prefixed with `openpgp4fpr:`")
	})?;

	if uri.scheme() == "openpgp4fpr" && !valid_openpgp4fpr(uri.path()) {
		return Err!("`openpgp4fpr:` must be followed by a 40- or 64-character hex fingerprint");
	}

	Ok(())
}

fn valid_openpgp4fpr(fpr: &str) -> bool {
	matches!(fpr.len(), 40 | 64) && fpr.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Iterates over all the keys in the config file and warns if there is a
/// deprecated key specified
fn warn_deprecated(config: &Config) {
	debug!("Checking for deprecated config keys");
	let found_deprecated_keys = config
		.catchall
		.keys()
		.filter(|key| DEPRECATED_KEYS.iter().any(|s| s == key))
		.inspect(|key| warn!("Config parameter \"{key}\" is deprecated, ignoring."))
		.next()
		.is_some();

	if found_deprecated_keys {
		warn!(
			"Deprecated config keys were found. Read tuwunel config documentation at https://tuwunel.chat/configuration.html and \
			 check your configuration if any new configuration parameters should be adjusted"
		);
	}
}

/// iterates over all the catchall keys (unknown config options) and warns or
/// errors if there are any.
fn warn_unknown_key(config: &Config) -> Result {
	debug!("Checking for unknown config keys");
	let known_keys =
		RegexSet::new(KNOWN_KEYS).expect("Invalid regular expression set construction");

	let unknown_keys = config
		.catchall
		.keys()
		.filter(|key| !known_keys.is_match(key))
		.inspect(|key| {
			if config.error_on_unknown_config_opts {
				error!("Config parameter \"{key}\" is unknown to tuwunel");
			} else {
				warn!("Config parameter \"{key}\" is unknown to tuwunel, ignoring.");
			}
		})
		.collect_vec();

	if !unknown_keys.is_empty() && config.error_on_unknown_config_opts {
		Err!("Unknown config options were found: {unknown_keys:?}")
	} else {
		Ok(())
	}
}
