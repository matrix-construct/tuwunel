use std::env::consts::OS;

use either::Either;
use figment::Figment;
use itertools::Itertools;

use super::{DEPRECATED_KEYS, IdentityProvider};
use crate::{Config, Err, Result, Server, debug, debug_info, debug_warn, error, warn};

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

	Ok(())
}

pub fn check(config: &Config) -> Result {
	if cfg!(debug_assertions) {
		warn!("Note: tuwunel was built without optimisations (i.e. debug build)");
	}

	if config.allow_invalid_tls_certificates {
		warn!(
			"\n\nWARNING: \n\nTLS CERTIFICATE VALIDATION IS DISABLED, THIS IS HIGHLY INSECURE \
			 AND SHOULD NOT BE USED IN PRODUCTION.\n\n"
		);
	}

	warn_deprecated(config);
	warn_unknown_key(config)?;

	if config.sentry && config.sentry_endpoint.is_none() {
		return Err!(Config(
			"sentry_endpoint",
			"Sentry cannot be enabled without an endpoint set"
		));
	}

	if cfg!(all(feature = "hardened_malloc", feature = "jemalloc", not(target_env = "msvc"))) {
		debug_warn!(
			"hardened_malloc and jemalloc compile-time features are both enabled, this causes \
			 jemalloc to be used."
		);
	}

	if cfg!(not(unix)) && config.unix_socket_path.is_some() {
		return Err!(Config(
			"unix_socket_path",
			"UNIX socket support is only available on *nix platforms. Please remove \
			 'unix_socket_path' from your config."
		));
	}

	if config.unix_socket_path.is_none() && config.get_bind_hosts().is_empty() {
		return Err!(Config("address", "No TCP addresses were specified to listen on"));
	}

	if config.unix_socket_path.is_none() && config.get_bind_ports().is_empty() {
		return Err!(Config("port", "No ports were specified to listen on"));
	}

	if !config.listening {
		warn!("Configuration item `listening` is set to `false`. Cannot hear anyone.");
	}

	if config.unix_socket_path.is_none() {
		config.get_bind_addrs().iter().for_each(|addr| {
			use std::path::Path;

			if addr.ip().is_loopback() {
				debug_info!(
					"Found loopback listening address {addr}, running checks if we're in a \
					 container."
				);

				if Path::new("/proc/vz").exists() /* Guest */ && !Path::new("/proc/bz").exists()
				/* Host */
				{
					error!(
						"You are detected using OpenVZ with a loopback/localhost listening \
						 address of {addr}. If you are using OpenVZ for containers and you use \
						 NAT-based networking to communicate with the host and guest, this will \
						 NOT work. Please change this to \"0.0.0.0\". If this is expected, you \
						 can ignore.",
					);
				} else if Path::new("/.dockerenv").exists() {
					error!(
						"You are detected using Docker with a loopback/localhost listening \
						 address of {addr}. If you are using a reverse proxy on the host and \
						 require communication to tuwunel in the Docker container via NAT-based \
						 networking, this will NOT work. Please change this to \"0.0.0.0\". If \
						 this is expected, you can ignore.",
					);
				} else if Path::new("/run/.containerenv").exists() {
					error!(
						"You are detected using Podman with a loopback/localhost listening \
						 address of {addr}. If you are using a reverse proxy on the host and \
						 require communication to tuwunel in the Podman container via NAT-based \
						 networking, this will NOT work. Please change this to \"0.0.0.0\". If \
						 this is expected, you can ignore.",
					);
				}
			}
		});
	}

	// rocksdb does not allow max_log_files to be 0
	if config.rocksdb_max_log_files == 0 {
		return Err!(Config(
			"max_log_files",
			"rocksdb_max_log_files cannot be 0. Please set a value at least 1."
		));
	}

	// yeah, unless the user built a debug build hopefully for local testing only
	if cfg!(not(debug_assertions)) && config.server_name == "your.server.name" {
		return Err!(Config(
			"server_name",
			"You must specify a valid server name for production usage of tuwunel."
		));
	}

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

	// check if the user specified a registration token as `""`
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
			let Ok(token) = std::fs::read_to_string(path).inspect_err(|e| {
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

	if config.max_request_size < 10_000_000 {
		return Err!(Config(
			"max_request_size",
			"Max request size is less than 10MB. Please increase it as this is too low for \
			 operable federation."
		));
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

	if config.allow_registration
		&& !config.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse
		&& config.registration_token.is_none()
		&& config.registration_token_file.is_none()
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
		&& config.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse
		&& config.registration_token.is_none()
		&& config.registration_token_file.is_none()
	{
		warn!(
			"Open registration is enabled via setting \
			 `yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse` and \
			 `allow_registration` to true without a registration token configured. You are \
			 expected to be aware of the risks now. If this is not the desired behaviour, \
			 please set a registration token."
		);
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

	if config
		.url_preview_domain_contains_allowlist
		.contains(&"*".to_owned())
	{
		warn!(
			"All URLs are allowed for URL previews via setting \
			 \"url_preview_domain_contains_allowlist\" to \"*\". This opens up significant \
			 attack surface to your server. You are expected to be aware of the risks by doing \
			 this."
		);
	}
	if config
		.url_preview_domain_explicit_allowlist
		.contains(&"*".to_owned())
	{
		warn!(
			"All URLs are allowed for URL previews via setting \
			 \"url_preview_domain_explicit_allowlist\" to \"*\". This opens up significant \
			 attack surface to your server. You are expected to be aware of the risks by doing \
			 this."
		);
	}
	if config
		.url_preview_url_contains_allowlist
		.contains(&"*".to_owned())
	{
		warn!(
			"All URLs are allowed for URL previews via setting \
			 \"url_preview_url_contains_allowlist\" to \"*\". This opens up significant attack \
			 surface to your server. You are expected to be aware of the risks by doing this."
		);
	}

	if let Some(Either::Right(_)) = config.url_preview_bound_interface.as_ref()
		&& !matches!(OS, "android" | "fuchsia" | "linux")
	{
		return Err!(Config(
			"url_preview_bound_interface",
			"Not a valid IP address. Interface names not supported on {OS}."
		));
	}

	if !Server::available_room_versions()
		.any(|(version, _)| version == config.default_room_version)
	{
		return Err!(Config(
			"default_room_version",
			"Room version {:?} is not available",
			config.default_room_version
		));
	}

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
		if provider.client_secret.is_some() {
			continue;
		}

		let Some(secret_path) = &provider.client_secret_file else {
			return Err!(Config(
				"client_secret",
				"Either client secret or a client secret file must be set on identity provider \
				 №{i}."
			));
		};

		let Ok(secret) = std::fs::read_to_string(secret_path) else {
			return Err!(Config(
				"client_secret_file",
				"Client secret file was specified but failed to be read at identity provider \
				 №{i}"
			));
		};

		if secret.is_empty() {
			return Err!(Config(
				"client_secret_file",
				"Client secret file was specified but is empty on identity provider №{i}"
			));
		}
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
	let unknown_keys = config
		.catchall
		.keys()
		.filter_map(|key| {
			if key == "config" {
				None
			} else {
				if config.error_on_unknown_config_opts {
					error!("Config parameter \"{key}\" is unknown to tuwunel");
				} else {
					warn!("Config parameter \"{key}\" is unknown to tuwunel, ignoring.");
				}
				Some(key.as_str())
			}
		})
		.collect_vec();

	if !unknown_keys.is_empty() && config.error_on_unknown_config_opts {
		Err!("Unknown config options were found: {unknown_keys:?}")
	} else {
		Ok(())
	}
}

/// Checks the presence of the `address` and `unix_socket_path` keys in the
/// raw_config, exiting the process if both keys were detected.
pub(super) fn is_dual_listening(raw_config: &Figment) -> Result {
	let contains_address = raw_config.contains("address");
	let contains_unix_socket = raw_config.contains("unix_socket_path");
	if contains_address && contains_unix_socket {
		return Err!(
			"TOML keys \"address\" and \"unix_socket_path\" were both defined. Please specify \
			 only one option."
		);
	}

	Ok(())
}
