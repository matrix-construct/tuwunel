mod groups;

use std::{collections::BTreeMap, fmt::Debug, path::Path};

use figment::Figment;
use serde_json::from_str;

use crate::{
	config::Config,
	utils::{BoolExt as _, sys::available_parallelism},
};

#[test]
fn defaults_match_frozen_baseline() {
	let config = Config::new(&Figment::new()).expect("default configuration parses");

	assert_defaults(&config);
}

#[test]
fn legacy_files_match_frozen_baseline() {
	for filename in ["legacy.toml", "global.toml"] {
		let path = Path::new(env!("CARGO_MANIFEST_DIR"))
			.join("config/tests/baseline")
			.join(filename);

		let source = Config::load_files([path].into_iter()).expect("legacy file loads");
		let config = Config::new(&source).expect("legacy configuration parses");

		assert_values(&config, include_str!("legacy.json"), false);
	}
}

pub(super) fn assert_defaults(config: &Config) {
	assert_values(config, include_str!("defaults.json"), true);
}

pub(super) fn assert_regenerated(config: &Config) {
	let expected: BTreeMap<String, String> =
		from_str(include_str!("legacy.json")).expect("frozen baseline parses");

	assert_eq!(expected.len(), 364);
	for (name, actual) in values(config) {
		let expected = match name {
			| "database_restore_backup" => "None",
			| "force_migration" => "false",
			| _ => expected[name].as_str(),
		};

		assert_eq!(actual, expected, "{name}");
	}
}

fn assert_values(config: &Config, snapshot: &str, defaults: bool) {
	let expected: BTreeMap<String, String> = from_str(snapshot).expect("frozen baseline parses");

	assert_eq!(expected.len(), 364);
	for (name, actual) in values(config) {
		let conditional = defaults.and_then(|| conditional_default(name));

		let expected = conditional
			.as_ref()
			.unwrap_or_else(|| &expected[name]);

		assert_eq!(&actual, expected, "{name}");
	}
}

fn values(config: &Config) -> impl Iterator<Item = (&'static str, String)> + '_ {
	fields_1(config)
		.chain(fields_2(config))
		.chain(fields_3(config))
		.chain(fields_4(config))
		.map(|(name, value)| (name, format!("{value:?}")))
}

fn conditional_default(name: &str) -> Option<String> {
	let cpus = u32::try_from(available_parallelism()).expect("CPU count fits u32");
	let cache = |scale: u32, base: u32| {
		cpus.saturating_mul(scale)
			.saturating_add(base)
			.to_string()
	};

	let debug =
		|debug, release| format!("{:?}", if cfg!(debug_assertions) { debug } else { release });

	let value = match name {
		| "db_write_buffer_capacity_mb" =>
			format!("{:?}", 4.0_f64.mul_add(f64::from(cpus), 48.0)),
		| "db_cache_capacity_mb" => format!("{:?}", 64.0_f64.mul_add(f64::from(cpus), 128.0)),
		| "pdu_cache_capacity" | "resolver_cache_capacity" => cache(10_000, 100_000),
		| "auth_chain_cache_capacity" => cache(250_000, 750_000),
		| "shorteventid_cache_capacity" => cache(200_000, 400_000),
		| "eventidshort_cache_capacity" | "eventid_pdu_cache_capacity" => cache(100_000, 400_000),
		| "eventid_backoff_cache_capacity" => cache(4_000, 200_000),
		| "shortstatekey_cache_capacity" | "statekeyshort_cache_capacity" => cache(4_000, 40_000),
		| "servernameevent_data_cache_capacity" => cache(50_000, 200_000),
		| "servername_status_cache_capacity" => cache(15_000, 200_000),
		| "stateinfo_cache_capacity" => cache(100, 0),
		| "db_pool_workers" => available_parallelism()
			.saturating_mul(4)
			.clamp(32, 1024)
			.to_string(),
		| "log" | "admin_log_capture" => debug("debug", "info"),
		| "tracing_flame_filter" | "jaeger_filter" => debug("trace,h2=off", "info"),
		| "rocksdb_compression_algo" => format!(
			"{:?}",
			if cfg!(feature = "zstd_compression") {
				"zstd"
			} else {
				"none"
			}
		),
		| _ => return None,
	};

	Some(value)
}

fn fields_1(config: &Config) -> impl Iterator<Item = (&'static str, &dyn Debug)> {
	let thread_limit = &config
		.rooms
		.redaction
		.thread_latest_reply_search_limit;

	let fetch_contexts = &config
		.federation
		.history
		.fetch_unreceived_contexts;

	let open_registration = &config
		.registration
		.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse;

	let fields: [(&str, &dyn Debug); 117] = [
		("server_name", &config.server_name),
		("database_path", &config.database_path),
		("new_user_displayname_suffix", &config.registration.displayname_suffix),
		("address", &config.http.listen.address),
		("port", &config.http.listen.port),
		("tls", &config.tls),
		("unix_socket_path", &config.http.listen.unix_socket_path),
		("unix_socket_perms", &config.http.listen.unix_socket_perms),
		("error_on_unknown_config_opts", &config.configuration.error_on_unknown_opts),
		("database_backup_path", &config.database.backup.path),
		("database_backups_to_keep", &config.database.backup.keep),
		("database_restore_backup", &config.database_restore_backup),
		("cache_capacity_modifier", &config.cache.capacity_modifier),
		("db_cache_capacity_mb", &config.rocksdb.cache_capacity_mb),
		("db_write_buffer_capacity_mb", &config.rocksdb.write_buffer_capacity_mb),
		("pdu_cache_capacity", &config.cache.pdu_capacity),
		("auth_chain_cache_capacity", &config.cache.auth_chain_capacity),
		("shorteventid_cache_capacity", &config.cache.shorteventid_capacity),
		("eventidshort_cache_capacity", &config.cache.eventidshort_capacity),
		("eventid_pdu_cache_capacity", &config.cache.eventid_pdu_capacity),
		("eventid_backoff_cache_capacity", &config.cache.eventid_backoff_capacity),
		("shortstatekey_cache_capacity", &config.cache.shortstatekey_capacity),
		("statekeyshort_cache_capacity", &config.cache.statekeyshort_capacity),
		(
			"servernameevent_data_cache_capacity",
			&config.cache.servernameevent_data_capacity,
		),
		("mediaid_lazycontent_cache_capacity", &config.cache.mediaid_lazycontent_capacity),
		("resolver_cache_capacity", &config.cache.resolver_capacity),
		("servername_status_cache_capacity", &config.cache.servername_status_capacity),
		("stateinfo_cache_capacity", &config.cache.stateinfo_capacity),
		("spacehierarchy_cache_ttl_min", &config.cache.space_hierarchy.ttl_min),
		("spacehierarchy_cache_ttl_max", &config.cache.space_hierarchy.ttl_max),
		("client_sync_timeout_min", &config.client.sync.timeout_min),
		("client_sync_timeout_default", &config.client.sync.timeout_default),
		("client_sync_timeout_max", &config.client.sync.timeout_max),
		("dns_servers", &config.network.dns.servers),
		("dns_cache_entries", &config.network.dns.cache_entries),
		("dns_min_ttl", &config.network.dns.min_ttl),
		("dns_min_ttl_nxdomain", &config.network.dns.min_ttl_nxdomain),
		("dns_attempts", &config.network.dns.attempts),
		("dns_timeout", &config.network.dns.timeout),
		("dns_tcp_fallback", &config.network.dns.tcp_fallback),
		("query_all_nameservers", &config.network.dns.query_all_nameservers),
		("query_over_tcp_only", &config.network.dns.query_over_tcp_only),
		("ip_lookup_strategy", &config.network.dns.ip_lookup_strategy),
		("dns_passthru_domains", &config.network.dns.passthru_domains),
		("dns_passthru_appservices", &config.network.dns.passthru_appservices),
		("dns_case_randomization", &config.network.dns.case_randomization),
		("max_request_size", &config.http.max_request_size),
		("max_response_size", &config.network.http.max_response_size),
		("max_pending_media_uploads", &config.media.max_pending_uploads),
		(
			"media_create_unused_expiration_time",
			&config.media.create_unused_expiration_time,
		),
		("media_rc_create_per_second", &config.media.rc_create_per_second),
		("media_rc_create_burst_count", &config.media.rc_create_burst_count),
		("media_upload_min_free_space", &config.media.upload_min_free_space),
		("max_fetch_prev_events", &config.federation.history.max_fetch_prev_events),
		("thread_latest_reply_search_limit", thread_limit),
		("prev_events_concurrency", &config.federation.history.prev_events_concurrency),
		("fetch_prev_wait_ms", &config.federation.history.fetch_prev_wait_ms),
		("request_conn_timeout", &config.network.http.conn_timeout),
		("request_timeout", &config.network.http.timeout),
		("request_total_timeout", &config.network.http.total_timeout),
		("request_idle_timeout", &config.network.http.idle_timeout),
		("request_idle_per_host", &config.network.http.idle_per_host),
		("request_gzip", &config.network.http.gzip),
		("request_brotli", &config.network.http.brotli),
		("request_zstd", &config.network.http.zstd),
		("well_known_conn_timeout", &config.network.discovery.conn_timeout),
		("well_known_timeout", &config.network.discovery.timeout),
		("federation_timeout", &config.federation.transport.timeout),
		("federation_keys_timeout", &config.federation.transport.keys_timeout),
		("feds_timeout", &config.federation.fanout.timeout),
		("federation_idle_timeout", &config.federation.transport.idle_timeout),
		("federation_idle_per_host", &config.federation.transport.idle_per_host),
		("sender_timeout", &config.sending.timeout),
		("sender_idle_timeout", &config.sending.idle_timeout),
		("sender_retry_backoff_limit", &config.sending.retry_backoff_limit),
		("sender_retry_grace", &config.sending.retry_grace),
		("appservice_timeout", &config.appservice_client.timeout),
		("appservice_idle_timeout", &config.appservice_client.idle_timeout),
		("pusher_idle_timeout", &config.push.idle_timeout),
		("client_receive_timeout", &config.http.receive_timeout),
		("client_request_timeout", &config.http.request_timeout),
		("client_response_timeout", &config.http.response_timeout),
		("client_shutdown_timeout", &config.http.shutdown_timeout),
		("ip_source", &config.http.ip_source),
		("ip_source_trusted_subnets", &config.http.ip_source_trusted_subnets),
		("sender_shutdown_timeout", &config.sending.shutdown_timeout),
		("allow_registration", &config.registration.enable),
		(
			"yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse",
			open_registration,
		),
		("registration_token", &config.registration.token),
		("registration_token_file", &config.registration.token_file),
		("registration_shared_secret", &config.registration.shared_secret),
		("registration_shared_secret_file", &config.registration.shared_secret_file),
		("mas_secret", &config.authentication.mas_secret),
		("argon2_m_cost", &config.authentication.argon2.m_cost),
		("argon2_t_cost", &config.authentication.argon2.t_cost),
		("argon2_p_cost", &config.authentication.argon2.p_cost),
		("allow_encryption", &config.encryption.allow),
		(
			"encryption_enabled_by_default_for_room_type",
			&config.encryption.enabled_by_default_for_room_type,
		),
		("allow_federation", &config.federation.enable),
		("fetch_unreceived_contexts_over_federation", fetch_contexts),
		("fetch_fanout_max_width", &config.federation.history.fetch_fanout_max_width),
		("fetch_fanout_rounds", &config.federation.history.fetch_fanout_rounds),
		("feds_max_width", &config.federation.fanout.max_width),
		("feds_destination_limit", &config.federation.fanout.destination_limit),
		("resolve_state_locally", &config.federation.resolve_state_locally),
		("resolve_state_locally_max", &config.federation.resolve_state_locally_max),
		("resolve_state_locally_shadow", &config.federation.resolve_state_locally_shadow),
		("forward_extremities_max", &config.rooms.extremities.max),
		("forward_extremities_emergency_max", &config.rooms.extremities.emergency_max),
		("forward_extremities_prune_batch", &config.rooms.extremities.prune_batch),
		("federate_created_rooms", &config.rooms.federate_by_default),
		("federation_loopback", &config.federation.loopback),
		("forget_forced_upon_leave", &config.rooms.forget_forced_upon_leave),
		("require_auth_for_profile_requests", &config.profile.require_auth),
		("max_remote_profile_fields", &config.profile.max_remote_fields),
		("max_profile_fields_per_request", &config.profile.max_fields_per_request),
		("enable_set_displayname", &config.profile.enable_set_displayname),
	];

	fields.into_iter()
}

fn fields_2(config: &Config) -> impl Iterator<Item = (&'static str, &dyn Debug)> {
	let trusted_first = &config
		.federation
		.keys
		.query_trusted_servers_first_on_join;

	let batch_concurrency = &config
		.federation
		.keys
		.trusted_server_batch_concurrency;

	let shared_rooms = &config
		.profile
		.limit_requests_to_users_who_share_rooms;

	let registration_token = &config
		.authentication
		.oidc
		.registration_access_token;

	let redirect_hosts = &config
		.authentication
		.oidc
		.registration_allowed_redirect_hosts;

	let require_client_uri = &config
		.authentication
		.oidc
		.registration_require_client_uri;

	let fields: [(&str, &dyn Debug); 88] = [
		("limit_profile_requests_to_users_who_share_rooms", shared_rooms),
		("preserve_room_profile_overrides", &config.profile.preserve_room_overrides),
		(
			"allow_public_room_directory_over_federation",
			&config.directory.rooms.allow_over_federation,
		),
		(
			"allow_public_room_directory_without_auth",
			&config.directory.rooms.allow_without_auth,
		),
		(
			"allow_public_room_search_by_id",
			&config.directory.rooms.allow_public_search_by_id,
		),
		(
			"allow_unlisted_room_search_by_id",
			&config.directory.rooms.allow_unlisted_search_by_id,
		),
		("show_all_local_users_in_user_directory", &config.directory.users.show_all_local),
		(
			"show_appservice_users_in_user_directory",
			&config.directory.users.show_appservice,
		),
		("turn_allow_guests", &config.turn.allow_guests),
		("lockdown_public_room_directory", &config.directory.rooms.lockdown),
		("allow_device_name_federation", &config.profile.allow_device_name_federation),
		(
			"allow_inbound_profile_lookup_federation_requests",
			&config.profile.allow_inbound_federation_lookups,
		),
		("allow_room_creation", &config.rooms.allow_creation),
		("allow_room_alias_creation", &config.rooms.allow_alias_creation),
		("allow_unstable_room_versions", &config.rooms.allow_unstable_versions),
		("allow_experimental_room_versions", &config.rooms.allow_experimental_versions),
		("enable_policy_servers", &config.rooms.policy_server.enable),
		("policy_server_request_timeout", &config.rooms.policy_server.request_timeout),
		("bundle_edit_relations", &config.rooms.relations.bundle_edits),
		("bundle_reference_relations", &config.rooms.relations.bundle_references),
		("default_room_version", &config.rooms.default_version),
		(
			"default_power_level_content_override",
			&config.rooms.default_power_level_content_override,
		),
		("well_known", &config.well_known),
		("allow_jaeger", &config.telemetry.jaeger.enable),
		("jaeger_filter", &config.telemetry.jaeger.filter),
		("tracing_flame", &config.tracing.flame.enable),
		("tracing_flame_filter", &config.tracing.flame.filter),
		("tracing_flame_output_path", &config.tracing.flame.output_path),
		("proxy", &config.proxy),
		("trusted_servers", &config.federation.keys.trusted_servers),
		(
			"query_trusted_key_servers_first",
			&config.federation.keys.query_trusted_servers_first,
		),
		("query_trusted_key_servers_first_on_join", trusted_first),
		(
			"only_query_trusted_key_servers",
			&config.federation.keys.only_query_trusted_servers,
		),
		("trusted_server_batch_size", &config.federation.keys.trusted_server_batch_size),
		("trusted_server_batch_concurrency", batch_concurrency),
		("log", &config.logging.filter),
		("log_colors", &config.logging.colors),
		("log_compact", &config.logging.compact),
		("log_span_events", &config.logging.span_events),
		("log_filter_regex", &config.logging.filter_regex),
		("log_thread_ids", &config.logging.thread_ids),
		("log_to_stderr", &config.logging.to_stderr),
		("log_journald", &config.logging.journald),
		("log_enable", &config.logging.enable),
		("log_global_default", &config.logging.global_default),
		("openid_token_ttl", &config.authentication.openid_token_ttl),
		("login_via_existing_session", &config.authentication.login.via_existing_session),
		("login_via_token", &config.authentication.login.via_token),
		("login_with_password", &config.authentication.login.with_password),
		("rate_limiting", &config.rate_limiting),
		("login_token_ttl", &config.authentication.login.token_ttl),
		("access_token_ttl", &config.authentication.access_token_ttl),
		("refresh_token_ttl", &config.authentication.refresh_token.ttl),
		("refresh_token_idle_only", &config.authentication.refresh_token.idle_only),
		("refresh_token_hard_logout", &config.authentication.refresh_token.hard_logout),
		("refresh_token_reuse_grace", &config.authentication.refresh_token.reuse_grace),
		("refresh_token_reuse_revoke", &config.authentication.refresh_token.reuse_revoke),
		("oidc_native_auth", &config.authentication.oidc.native_auth),
		("oidc_require_device_scope", &config.authentication.oidc.require_device_scope),
		("oidc_require_pkce", &config.authentication.oidc.require_pkce),
		("oidc_strict_scope", &config.authentication.oidc.strict_scope),
		("oidc_registration_access_token", registration_token),
		("oidc_registration_allowed_redirect_hosts", redirect_hosts),
		(
			"oidc_require_client_approval",
			&config.authentication.oidc.require_client_approval,
		),
		("oidc_registration_require_client_uri", require_client_uri),
		("oidc_rc_per_second", &config.authentication.oidc.rc_per_second),
		("oidc_rc_burst_count", &config.authentication.oidc.rc_burst_count),
		("rendezvous_enabled", &config.rendezvous.enable),
		("rendezvous_session_max_bytes", &config.rendezvous.session_max_bytes),
		("rendezvous_session_ttl", &config.rendezvous.session_ttl),
		("rendezvous_max_sessions", &config.rendezvous.max_sessions),
		("rendezvous_authenticated_only", &config.rendezvous.authenticated_only),
		("rendezvous_rc_per_second", &config.rendezvous.rc_per_second),
		("rendezvous_rc_burst_count", &config.rendezvous.rc_burst_count),
		("turn_username", &config.turn.username),
		("turn_password", &config.turn.password),
		("turn_uris", &config.turn.uris),
		("turn_secret", &config.turn.secret),
		("turn_secret_file", &config.turn.secret_file),
		("turn_ttl", &config.turn.ttl),
		("auto_join_rooms", &config.registration.auto_join_rooms),
		(
			"auto_deactivate_banned_room_attempts",
			&config.rooms.auto_deactivate_banned_room_attempts,
		),
		("rocksdb_log_level", &config.rocksdb.log_level),
		("rocksdb_log_stderr", &config.rocksdb.log_stderr),
		("rocksdb_max_log_file_size", &config.rocksdb.max_log_file_size),
		("rocksdb_log_time_to_roll", &config.rocksdb.log_time_to_roll),
		(
			"rocksdb_optimize_for_spinning_disks",
			&config.rocksdb.optimize_for_spinning_disks,
		),
		("rocksdb_direct_io", &config.rocksdb.direct_io),
	];

	fields.into_iter()
}

fn fields_3(config: &Config) -> impl Iterator<Item = (&'static str, &dyn Debug)> {
	let allowed_servers = &config
		.federation
		.allowed_remote_server_names_experimental;

	let enforce_stripped = &config
		.federation
		.enforce_stripped_state_pdu_validation;

	let directory_servers = &config
		.directory
		.rooms
		.forbidden_remote_server_names;

	let fields: [(&str, &dyn Debug); 101] = [
		("rocksdb_parallelism_threads", &config.rocksdb.parallelism_threads),
		("rocksdb_max_log_files", &config.rocksdb.max_log_files),
		("rocksdb_compression_algo", &config.rocksdb.compression_algo),
		("rocksdb_compression_level", &config.rocksdb.compression_level),
		(
			"rocksdb_bottommost_compression_level",
			&config.rocksdb.bottommost_compression_level,
		),
		("rocksdb_bottommost_compression", &config.rocksdb.bottommost_compression),
		("rocksdb_recovery_mode", &config.rocksdb.recovery_mode),
		("rocksdb_paranoid_file_checks", &config.rocksdb.paranoid_file_checks),
		("rocksdb_checksums", &config.rocksdb.checksums),
		("rocksdb_atomic_flush", &config.rocksdb.atomic_flush),
		("rocksdb_repair", &config.rocksdb.repair),
		("rocksdb_read_only", &config.rocksdb.read_only),
		("rocksdb_secondary", &config.rocksdb.secondary),
		("rocksdb_compaction_prio_idle", &config.rocksdb.compaction_prio_idle),
		("rocksdb_compaction_ioprio_idle", &config.rocksdb.compaction_ioprio_idle),
		("rocksdb_compaction", &config.rocksdb.compaction),
		("rocksdb_stats_level", &config.rocksdb.stats_level),
		("rocksdb_never_drop_columns", &config.rocksdb.never_drop_columns),
		("rocksdb_allow_fallocate", &config.rocksdb.allow_fallocate),
		("emergency_password", &config.authentication.emergency_password),
		("notification_push_path", &config.push.notification_path),
		("push_everything", &config.push.everything),
		("msc3664_related_event_match", &config.push.msc3664_related_event_match),
		("calculate_heroes", &config.client.sync.calculate_heroes),
		("allow_local_presence", &config.presence.allow_local),
		("allow_incoming_presence", &config.presence.allow_incoming),
		("allow_outgoing_presence", &config.presence.allow_outgoing),
		("presence_idle_timeout_s", &config.presence.idle_timeout_s),
		("presence_offline_timeout_s", &config.presence.offline_timeout_s),
		("presence_timeout_remote_users", &config.presence.timeout_remote_users),
		("suppress_push_when_active", &config.push.suppress_when_active),
		("allow_incoming_read_receipts", &config.read_receipts.allow_incoming),
		("allow_outgoing_read_receipts", &config.read_receipts.allow_outgoing),
		("allow_outgoing_typing", &config.typing.allow_outgoing),
		("allow_incoming_typing", &config.typing.allow_incoming),
		("typing_federation_timeout_s", &config.typing.federation_timeout_s),
		("typing_client_timeout_min_s", &config.typing.client_timeout_min_s),
		("typing_client_timeout_max_s", &config.typing.client_timeout_max_s),
		("zstd_compression", &config.http.compression.zstd),
		("gzip_compression", &config.http.compression.gzip),
		("brotli_compression", &config.http.compression.brotli),
		("allow_guest_registration", &config.registration.guests.enable),
		("log_guest_registrations", &config.admin.room.log_guest_registrations),
		(
			"allow_guests_auto_join_rooms",
			&config.registration.guests.allow_auto_join_rooms,
		),
		("media_deny_framing", &config.media.deny_framing),
		("media_deny_inline_styles", &config.media.deny_inline_styles),
		("allow_legacy_media", &config.media.allow_legacy),
		("request_legacy_media", &config.media.request_legacy),
		("freeze_legacy_media", &config.media.freeze_legacy),
		("media_startup_check", &config.media.startup_check),
		("media_compat_file_link", &config.media.compat_file_link),
		("prune_missing_media", &config.media.prune_missing),
		("media_thumbnail_max_pixels", &config.media.thumbnail.max_pixels),
		("media_thumbnail_animated", &config.media.thumbnail.animated),
		("media_thumbnail_max_frames", &config.media.thumbnail.max_frames),
		(
			"media_thumbnail_animated_concurrency",
			&config.media.thumbnail.animated_concurrency,
		),
		("media_video_thumbnail_command", &config.media.video_thumbnail.command),
		("media_video_thumbnail_timeout", &config.media.video_thumbnail.timeout),
		("media_video_thumbnail_concurrency", &config.media.video_thumbnail.concurrency),
		("media_video_thumbnail_max_size", &config.media.video_thumbnail.max_size),
		("media_video_thumbnail_path", &config.media.video_thumbnail.path),
		("media_storage_providers", &config.media.storage_providers),
		("store_media_on_providers", &config.media.store_on_providers),
		("media_allow_redirect", &config.media.allow_redirect),
		("prevent_media_downloads_from", &config.media.prevent_downloads_from),
		(
			"forbidden_remote_server_names",
			&config.federation.forbidden_remote_server_names,
		),
		("allowed_remote_server_names_experimental", allowed_servers),
		("forbidden_remote_room_directory_server_names", directory_servers),
		("ip_range_denylist", &config.network.http.ip_range_denylist),
		("url_preview_bound_interface", &config.media.url_preview.bound_interface),
		(
			"url_preview_domain_contains_allowlist",
			&config.media.url_preview.domain_contains_allowlist,
		),
		(
			"url_preview_domain_explicit_allowlist",
			&config.media.url_preview.domain_explicit_allowlist,
		),
		(
			"url_preview_domain_explicit_denylist",
			&config.media.url_preview.domain_explicit_denylist,
		),
		(
			"url_preview_url_contains_allowlist",
			&config.media.url_preview.url_contains_allowlist,
		),
		("url_preview_max_spider_size", &config.media.url_preview.max_spider_size),
		("url_preview_max_media_size", &config.media.url_preview.max_media_size),
		("url_preview_cache_ttl", &config.media.url_preview.cache_ttl),
		("url_preview_check_root_domain", &config.media.url_preview.check_root_domain),
		("url_preview_user_agent", &config.media.url_preview.user_agent),
		("url_preview_media_user_agent", &config.media.url_preview.media_user_agent),
		("url_preview_accept_language", &config.media.url_preview.accept_language),
		("forbidden_alias_names", &config.rooms.forbidden_alias_names),
		("forbidden_usernames", &config.registration.forbidden_usernames),
		("deprioritize_joins_through_servers", &config.rooms.joins.deprioritize_servers),
		(
			"max_make_join_attempts_per_join_attempt",
			&config.rooms.joins.max_make_join_per_attempt,
		),
		(
			"max_join_attempts_per_join_request",
			&config.rooms.joins.max_attempts_per_request,
		),
		("startup_netburst", &config.sending.startup_netburst),
		("startup_netburst_keep", &config.sending.startup_netburst_keep),
		("block_non_admin_invites", &config.rooms.invites.block_non_admin),
		("auto_accept_invites", &config.rooms.invites.auto_accept),
		("auto_accept_invites_direct_only", &config.rooms.invites.auto_accept_direct_only),
		("auto_accept_invites_local_only", &config.rooms.invites.auto_accept_local_only),
		("enforce_stripped_state_pdu_validation", enforce_stripped),
		("admin_escape_commands", &config.admin.escape_commands),
		("admin_console_automatic", &config.admin.console_automatic),
		("admin_execute", &config.admin.execute),
		("admin_execute_errors_ignore", &config.admin.execute_errors_ignore),
		("admin_signal_execute", &config.admin.signal_execute),
		("admin_log_capture", &config.admin.log_capture),
		("admin_room_tag", &config.admin.room.tag),
		("server_user_localpart", &config.admin.server_user_localpart),
	];

	fields.into_iter()
}

fn fields_4(config: &Config) -> impl Iterator<Item = (&'static str, &dyn Debug)> {
	let allow_unredacted = &config
		.rooms
		.redaction
		.allow_room_admins_to_request_unredacted_events;

	let enforce_erasure = &config
		.rooms
		.redaction
		.enforce_erasure_over_federation;

	let custom_sso_page = &config
		.authentication
		.login
		.sso_custom_providers_page;

	let encrypted_only = &config
		.encryption
		.device_key_update_encrypted_rooms_only;

	let fields: [(&str, &dyn Debug); 58] = [
		("report_room", &config.admin.report_room),
		("grant_admin_to_first_user", &config.registration.grant_admin_to_first_user),
		("create_admin_room", &config.admin.room.create),
		("federate_admin_room", &config.admin.room.federate),
		("sentry", &config.telemetry.sentry.enable),
		("sentry_endpoint", &config.telemetry.sentry.endpoint),
		("sentry_send_server_name", &config.telemetry.sentry.send_server_name),
		("sentry_traces_sample_rate", &config.telemetry.sentry.traces_sample_rate),
		("sentry_attach_stacktrace", &config.telemetry.sentry.attach_stacktrace),
		("sentry_send_panic", &config.telemetry.sentry.send_panic),
		("sentry_send_error", &config.telemetry.sentry.send_error),
		("sentry_filter", &config.telemetry.sentry.filter),
		("tokio_console", &config.tracing.tokio_console),
		("test", &config.test),
		("maintenance", &config.maintenance),
		("admin_room_notices", &config.admin.room.notices),
		("admin_output_max_events", &config.admin.output_max_events),
		("admin_output_threads", &config.admin.output_threads),
		("save_unredacted_events", &config.rooms.redaction.save_unredacted_events),
		("redaction_retention_seconds", &config.rooms.redaction.retention_seconds),
		("allow_room_admins_to_request_unredacted_events", allow_unredacted),
		("disable_local_redactions", &config.rooms.redaction.disable_local),
		("enforce_erasure_over_federation", enforce_erasure),
		("db_pool_affinity", &config.database.pool.affinity),
		("db_pool_workers", &config.database.pool.workers),
		("db_pool_workers_limit", &config.database.pool.workers_limit),
		("db_pool_max_workers", &config.database.pool.max_workers),
		("db_pool_queue_mult", &config.database.pool.queue_mult),
		("stream_width_default", &config.runtime.stream.width_default),
		("stream_width_scale", &config.runtime.stream.width_scale),
		("stream_amplification", &config.runtime.stream.amplification),
		("sender_workers", &config.sending.workers),
		("listening", &config.listening),
		("config_reload_signal", &config.configuration.reload_signal),
		(
			"allow_invalid_tls_certificates",
			&config.network.http.allow_invalid_tls_certificates,
		),
		("access_control_allow_origin", &config.http.access_control_allow_origin),
		("hydra_backports", &config.compatibility.hydra_backports),
		("delete_rooms_after_leave", &config.rooms.delete_after_leave),
		("one_time_key_limit", &config.encryption.one_time_key_limit),
		("single_sso", &config.authentication.login.single_sso),
		("sso_custom_providers_page", custom_sso_page),
		("oidc_aware_preferred", &config.authentication.login.oidc_aware_preferred),
		("appservice_dir", &config.appservice_dir),
		("database_migrations", &config.database_migrations),
		("force_migration", &config.force_migration),
		("conduit_source_media_path", &config.media.import.conduit.source_path),
		("conduit_media_directory_depth", &config.media.import.conduit.directory_depth),
		("conduit_media_directory_length", &config.media.import.conduit.directory_length),
		("conduit_source_media_provider", &config.media.import.conduit.source_provider),
		("device_key_update_encrypted_rooms_only", encrypted_only),
		("storage_provider", &config.storage_provider),
		("registration_terms", &config.registration_terms),
		("ldap", &config.ldap),
		("jwt", &config.jwt),
		("smtp", &config.smtp),
		("appservice", &config.appservice),
		("identity_provider", &config.identity_provider),
		("catchall", &config.catchall),
	];

	fields.into_iter()
}
