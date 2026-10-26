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
	let fields: [(&str, &dyn Debug); 117] = [
		("server_name", &config.server_name),
		("database_path", &config.database_path),
		("new_user_displayname_suffix", &config.new_user_displayname_suffix),
		("address", &config.address),
		("port", &config.port),
		("tls", &config.tls),
		("unix_socket_path", &config.unix_socket_path),
		("unix_socket_perms", &config.unix_socket_perms),
		("error_on_unknown_config_opts", &config.error_on_unknown_config_opts),
		("database_backup_path", &config.database_backup_path),
		("database_backups_to_keep", &config.database_backups_to_keep),
		("database_restore_backup", &config.database_restore_backup),
		("cache_capacity_modifier", &config.cache_capacity_modifier),
		("db_cache_capacity_mb", &config.rocksdb.cache_capacity_mb),
		("db_write_buffer_capacity_mb", &config.rocksdb.write_buffer_capacity_mb),
		("pdu_cache_capacity", &config.pdu_cache_capacity),
		("auth_chain_cache_capacity", &config.auth_chain_cache_capacity),
		("shorteventid_cache_capacity", &config.shorteventid_cache_capacity),
		("eventidshort_cache_capacity", &config.eventidshort_cache_capacity),
		("eventid_pdu_cache_capacity", &config.eventid_pdu_cache_capacity),
		("eventid_backoff_cache_capacity", &config.eventid_backoff_cache_capacity),
		("shortstatekey_cache_capacity", &config.shortstatekey_cache_capacity),
		("statekeyshort_cache_capacity", &config.statekeyshort_cache_capacity),
		(
			"servernameevent_data_cache_capacity",
			&config.servernameevent_data_cache_capacity,
		),
		("mediaid_lazycontent_cache_capacity", &config.mediaid_lazycontent_cache_capacity),
		("resolver_cache_capacity", &config.resolver_cache_capacity),
		("servername_status_cache_capacity", &config.servername_status_cache_capacity),
		("stateinfo_cache_capacity", &config.stateinfo_cache_capacity),
		("spacehierarchy_cache_ttl_min", &config.spacehierarchy_cache_ttl_min),
		("spacehierarchy_cache_ttl_max", &config.spacehierarchy_cache_ttl_max),
		("client_sync_timeout_min", &config.client_sync_timeout_min),
		("client_sync_timeout_default", &config.client_sync_timeout_default),
		("client_sync_timeout_max", &config.client_sync_timeout_max),
		("dns_servers", &config.dns_servers),
		("dns_cache_entries", &config.dns_cache_entries),
		("dns_min_ttl", &config.dns_min_ttl),
		("dns_min_ttl_nxdomain", &config.dns_min_ttl_nxdomain),
		("dns_attempts", &config.dns_attempts),
		("dns_timeout", &config.dns_timeout),
		("dns_tcp_fallback", &config.dns_tcp_fallback),
		("query_all_nameservers", &config.query_all_nameservers),
		("query_over_tcp_only", &config.query_over_tcp_only),
		("ip_lookup_strategy", &config.ip_lookup_strategy),
		("dns_passthru_domains", &config.dns_passthru_domains),
		("dns_passthru_appservices", &config.dns_passthru_appservices),
		("dns_case_randomization", &config.dns_case_randomization),
		("max_request_size", &config.max_request_size),
		("max_response_size", &config.max_response_size),
		("max_pending_media_uploads", &config.max_pending_media_uploads),
		(
			"media_create_unused_expiration_time",
			&config.media_create_unused_expiration_time,
		),
		("media_rc_create_per_second", &config.media_rc_create_per_second),
		("media_rc_create_burst_count", &config.media_rc_create_burst_count),
		("media_upload_min_free_space", &config.media_upload_min_free_space),
		("max_fetch_prev_events", &config.max_fetch_prev_events),
		("thread_latest_reply_search_limit", &config.thread_latest_reply_search_limit),
		("prev_events_concurrency", &config.prev_events_concurrency),
		("fetch_prev_wait_ms", &config.fetch_prev_wait_ms),
		("request_conn_timeout", &config.request_conn_timeout),
		("request_timeout", &config.request_timeout),
		("request_total_timeout", &config.request_total_timeout),
		("request_idle_timeout", &config.request_idle_timeout),
		("request_idle_per_host", &config.request_idle_per_host),
		("request_gzip", &config.request_gzip),
		("request_brotli", &config.request_brotli),
		("request_zstd", &config.request_zstd),
		("well_known_conn_timeout", &config.well_known_conn_timeout),
		("well_known_timeout", &config.well_known_timeout),
		("federation_timeout", &config.federation_timeout),
		("federation_keys_timeout", &config.federation_keys_timeout),
		("feds_timeout", &config.feds_timeout),
		("federation_idle_timeout", &config.federation_idle_timeout),
		("federation_idle_per_host", &config.federation_idle_per_host),
		("sender_timeout", &config.sender_timeout),
		("sender_idle_timeout", &config.sender_idle_timeout),
		("sender_retry_backoff_limit", &config.sender_retry_backoff_limit),
		("sender_retry_grace", &config.sender_retry_grace),
		("appservice_timeout", &config.appservice_timeout),
		("appservice_idle_timeout", &config.appservice_idle_timeout),
		("pusher_idle_timeout", &config.pusher_idle_timeout),
		("client_receive_timeout", &config.client_receive_timeout),
		("client_request_timeout", &config.client_request_timeout),
		("client_response_timeout", &config.client_response_timeout),
		("client_shutdown_timeout", &config.client_shutdown_timeout),
		("ip_source", &config.ip_source),
		("ip_source_trusted_subnets", &config.ip_source_trusted_subnets),
		("sender_shutdown_timeout", &config.sender_shutdown_timeout),
		("allow_registration", &config.allow_registration),
		(
			"yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse",
			&config.yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse,
		),
		("registration_token", &config.registration_token),
		("registration_token_file", &config.registration_token_file),
		("registration_shared_secret", &config.registration_shared_secret),
		("registration_shared_secret_file", &config.registration_shared_secret_file),
		("mas_secret", &config.mas_secret),
		("argon2_m_cost", &config.argon2_m_cost),
		("argon2_t_cost", &config.argon2_t_cost),
		("argon2_p_cost", &config.argon2_p_cost),
		("allow_encryption", &config.allow_encryption),
		(
			"encryption_enabled_by_default_for_room_type",
			&config.encryption_enabled_by_default_for_room_type,
		),
		("allow_federation", &config.allow_federation),
		(
			"fetch_unreceived_contexts_over_federation",
			&config.fetch_unreceived_contexts_over_federation,
		),
		("fetch_fanout_max_width", &config.fetch_fanout_max_width),
		("fetch_fanout_rounds", &config.fetch_fanout_rounds),
		("feds_max_width", &config.feds_max_width),
		("feds_destination_limit", &config.feds_destination_limit),
		("resolve_state_locally", &config.resolve_state_locally),
		("resolve_state_locally_max", &config.resolve_state_locally_max),
		("resolve_state_locally_shadow", &config.resolve_state_locally_shadow),
		("forward_extremities_max", &config.forward_extremities_max),
		("forward_extremities_emergency_max", &config.forward_extremities_emergency_max),
		("forward_extremities_prune_batch", &config.forward_extremities_prune_batch),
		("federate_created_rooms", &config.federate_created_rooms),
		("federation_loopback", &config.federation_loopback),
		("forget_forced_upon_leave", &config.forget_forced_upon_leave),
		("require_auth_for_profile_requests", &config.require_auth_for_profile_requests),
		("max_remote_profile_fields", &config.max_remote_profile_fields),
		("max_profile_fields_per_request", &config.max_profile_fields_per_request),
		("enable_set_displayname", &config.enable_set_displayname),
	];

	fields.into_iter()
}

fn fields_2(config: &Config) -> impl Iterator<Item = (&'static str, &dyn Debug)> {
	let fields: [(&str, &dyn Debug); 96] = [
		(
			"limit_profile_requests_to_users_who_share_rooms",
			&config.limit_profile_requests_to_users_who_share_rooms,
		),
		("preserve_room_profile_overrides", &config.preserve_room_profile_overrides),
		(
			"allow_public_room_directory_over_federation",
			&config.allow_public_room_directory_over_federation,
		),
		(
			"allow_public_room_directory_without_auth",
			&config.allow_public_room_directory_without_auth,
		),
		("allow_public_room_search_by_id", &config.allow_public_room_search_by_id),
		("allow_unlisted_room_search_by_id", &config.allow_unlisted_room_search_by_id),
		(
			"show_all_local_users_in_user_directory",
			&config.show_all_local_users_in_user_directory,
		),
		(
			"show_appservice_users_in_user_directory",
			&config.show_appservice_users_in_user_directory,
		),
		("turn_allow_guests", &config.turn_allow_guests),
		("lockdown_public_room_directory", &config.lockdown_public_room_directory),
		("allow_device_name_federation", &config.allow_device_name_federation),
		(
			"allow_inbound_profile_lookup_federation_requests",
			&config.allow_inbound_profile_lookup_federation_requests,
		),
		("allow_room_creation", &config.allow_room_creation),
		("allow_room_alias_creation", &config.allow_room_alias_creation),
		("allow_unstable_room_versions", &config.allow_unstable_room_versions),
		("allow_experimental_room_versions", &config.allow_experimental_room_versions),
		("enable_policy_servers", &config.enable_policy_servers),
		("policy_server_request_timeout", &config.policy_server_request_timeout),
		("bundle_edit_relations", &config.bundle_edit_relations),
		("bundle_reference_relations", &config.bundle_reference_relations),
		("default_room_version", &config.default_room_version),
		(
			"default_power_level_content_override",
			&config.default_power_level_content_override,
		),
		("well_known", &config.well_known),
		("allow_jaeger", &config.allow_jaeger),
		("jaeger_filter", &config.jaeger_filter),
		("tracing_flame", &config.tracing_flame),
		("tracing_flame_filter", &config.tracing_flame_filter),
		("tracing_flame_output_path", &config.tracing_flame_output_path),
		("proxy", &config.proxy),
		("trusted_servers", &config.trusted_servers),
		("query_trusted_key_servers_first", &config.query_trusted_key_servers_first),
		(
			"query_trusted_key_servers_first_on_join",
			&config.query_trusted_key_servers_first_on_join,
		),
		("only_query_trusted_key_servers", &config.only_query_trusted_key_servers),
		("trusted_server_batch_size", &config.trusted_server_batch_size),
		("trusted_server_batch_concurrency", &config.trusted_server_batch_concurrency),
		("log", &config.log),
		("log_colors", &config.log_colors),
		("log_compact", &config.log_compact),
		("log_span_events", &config.log_span_events),
		("log_filter_regex", &config.log_filter_regex),
		("log_thread_ids", &config.log_thread_ids),
		("log_to_stderr", &config.log_to_stderr),
		("log_journald", &config.log_journald),
		("log_enable", &config.log_enable),
		("log_global_default", &config.log_global_default),
		("openid_token_ttl", &config.openid_token_ttl),
		("login_via_existing_session", &config.login_via_existing_session),
		("login_via_token", &config.login_via_token),
		("login_with_password", &config.login_with_password),
		("rate_limiting", &config.rate_limiting),
		("login_token_ttl", &config.login_token_ttl),
		("access_token_ttl", &config.access_token_ttl),
		("refresh_token_ttl", &config.refresh_token_ttl),
		("refresh_token_idle_only", &config.refresh_token_idle_only),
		("refresh_token_hard_logout", &config.refresh_token_hard_logout),
		("refresh_token_reuse_grace", &config.refresh_token_reuse_grace),
		("refresh_token_reuse_revoke", &config.refresh_token_reuse_revoke),
		("oidc_native_auth", &config.oidc_native_auth),
		("oidc_require_device_scope", &config.oidc_require_device_scope),
		("oidc_require_pkce", &config.oidc_require_pkce),
		("oidc_strict_scope", &config.oidc_strict_scope),
		("oidc_registration_access_token", &config.oidc_registration_access_token),
		(
			"oidc_registration_allowed_redirect_hosts",
			&config.oidc_registration_allowed_redirect_hosts,
		),
		("oidc_require_client_approval", &config.oidc_require_client_approval),
		(
			"oidc_registration_require_client_uri",
			&config.oidc_registration_require_client_uri,
		),
		("oidc_rc_per_second", &config.oidc_rc_per_second),
		("oidc_rc_burst_count", &config.oidc_rc_burst_count),
		("rendezvous_enabled", &config.rendezvous_enabled),
		("rendezvous_session_max_bytes", &config.rendezvous_session_max_bytes),
		("rendezvous_session_ttl", &config.rendezvous_session_ttl),
		("rendezvous_max_sessions", &config.rendezvous_max_sessions),
		("rendezvous_authenticated_only", &config.rendezvous_authenticated_only),
		("rendezvous_rc_per_second", &config.rendezvous_rc_per_second),
		("rendezvous_rc_burst_count", &config.rendezvous_rc_burst_count),
		("turn_username", &config.turn_username),
		("turn_password", &config.turn_password),
		("turn_uris", &config.turn_uris),
		("turn_secret", &config.turn_secret),
		("turn_secret_file", &config.turn_secret_file),
		("turn_ttl", &config.turn_ttl),
		("auto_join_rooms", &config.auto_join_rooms),
		(
			"auto_deactivate_banned_room_attempts",
			&config.auto_deactivate_banned_room_attempts,
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
	];

	fields.into_iter()
}

fn fields_3(config: &Config) -> impl Iterator<Item = (&'static str, &dyn Debug)> {
	let fields: [(&str, &dyn Debug); 111] = [
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
		("emergency_password", &config.emergency_password),
		("notification_push_path", &config.notification_push_path),
		("push_everything", &config.push_everything),
		("msc3664_related_event_match", &config.msc3664_related_event_match),
		("calculate_heroes", &config.calculate_heroes),
		("allow_local_presence", &config.allow_local_presence),
		("allow_incoming_presence", &config.allow_incoming_presence),
		("allow_outgoing_presence", &config.allow_outgoing_presence),
		("presence_idle_timeout_s", &config.presence_idle_timeout_s),
		("presence_offline_timeout_s", &config.presence_offline_timeout_s),
		("presence_timeout_remote_users", &config.presence_timeout_remote_users),
		("suppress_push_when_active", &config.suppress_push_when_active),
		("allow_incoming_read_receipts", &config.allow_incoming_read_receipts),
		("allow_outgoing_read_receipts", &config.allow_outgoing_read_receipts),
		("allow_outgoing_typing", &config.allow_outgoing_typing),
		("allow_incoming_typing", &config.allow_incoming_typing),
		("typing_federation_timeout_s", &config.typing_federation_timeout_s),
		("typing_client_timeout_min_s", &config.typing_client_timeout_min_s),
		("typing_client_timeout_max_s", &config.typing_client_timeout_max_s),
		("zstd_compression", &config.zstd_compression),
		("gzip_compression", &config.gzip_compression),
		("brotli_compression", &config.brotli_compression),
		("allow_guest_registration", &config.allow_guest_registration),
		("log_guest_registrations", &config.log_guest_registrations),
		("allow_guests_auto_join_rooms", &config.allow_guests_auto_join_rooms),
		("media_deny_framing", &config.media_deny_framing),
		("media_deny_inline_styles", &config.media_deny_inline_styles),
		("allow_legacy_media", &config.allow_legacy_media),
		("request_legacy_media", &config.request_legacy_media),
		("freeze_legacy_media", &config.freeze_legacy_media),
		("media_startup_check", &config.media_startup_check),
		("media_compat_file_link", &config.media_compat_file_link),
		("prune_missing_media", &config.prune_missing_media),
		("media_thumbnail_max_pixels", &config.media_thumbnail_max_pixels),
		("media_thumbnail_animated", &config.media_thumbnail_animated),
		("media_thumbnail_max_frames", &config.media_thumbnail_max_frames),
		(
			"media_thumbnail_animated_concurrency",
			&config.media_thumbnail_animated_concurrency,
		),
		("media_video_thumbnail_command", &config.media_video_thumbnail_command),
		("media_video_thumbnail_timeout", &config.media_video_thumbnail_timeout),
		("media_video_thumbnail_concurrency", &config.media_video_thumbnail_concurrency),
		("media_video_thumbnail_max_size", &config.media_video_thumbnail_max_size),
		("media_video_thumbnail_path", &config.media_video_thumbnail_path),
		("media_storage_providers", &config.media_storage_providers),
		("store_media_on_providers", &config.store_media_on_providers),
		("media_allow_redirect", &config.media_allow_redirect),
		("prevent_media_downloads_from", &config.prevent_media_downloads_from),
		("forbidden_remote_server_names", &config.forbidden_remote_server_names),
		(
			"allowed_remote_server_names_experimental",
			&config.allowed_remote_server_names_experimental,
		),
		(
			"forbidden_remote_room_directory_server_names",
			&config.forbidden_remote_room_directory_server_names,
		),
		("ip_range_denylist", &config.ip_range_denylist),
		("url_preview_bound_interface", &config.url_preview_bound_interface),
		(
			"url_preview_domain_contains_allowlist",
			&config.url_preview_domain_contains_allowlist,
		),
		(
			"url_preview_domain_explicit_allowlist",
			&config.url_preview_domain_explicit_allowlist,
		),
		(
			"url_preview_domain_explicit_denylist",
			&config.url_preview_domain_explicit_denylist,
		),
		("url_preview_url_contains_allowlist", &config.url_preview_url_contains_allowlist),
		("url_preview_max_spider_size", &config.url_preview_max_spider_size),
		("url_preview_max_media_size", &config.url_preview_max_media_size),
		("url_preview_cache_ttl", &config.url_preview_cache_ttl),
		("url_preview_check_root_domain", &config.url_preview_check_root_domain),
		("url_preview_user_agent", &config.url_preview_user_agent),
		("url_preview_media_user_agent", &config.url_preview_media_user_agent),
		("url_preview_accept_language", &config.url_preview_accept_language),
		("forbidden_alias_names", &config.forbidden_alias_names),
		("forbidden_usernames", &config.forbidden_usernames),
		("deprioritize_joins_through_servers", &config.deprioritize_joins_through_servers),
		(
			"max_make_join_attempts_per_join_attempt",
			&config.max_make_join_attempts_per_join_attempt,
		),
		("max_join_attempts_per_join_request", &config.max_join_attempts_per_join_request),
		("startup_netburst", &config.startup_netburst),
		("startup_netburst_keep", &config.startup_netburst_keep),
		("block_non_admin_invites", &config.block_non_admin_invites),
		("auto_accept_invites", &config.auto_accept_invites),
		("auto_accept_invites_direct_only", &config.auto_accept_invites_direct_only),
		("auto_accept_invites_local_only", &config.auto_accept_invites_local_only),
		(
			"enforce_stripped_state_pdu_validation",
			&config.enforce_stripped_state_pdu_validation,
		),
		("admin_escape_commands", &config.admin_escape_commands),
		("admin_console_automatic", &config.admin_console_automatic),
		("admin_execute", &config.admin_execute),
		("admin_execute_errors_ignore", &config.admin_execute_errors_ignore),
		("admin_signal_execute", &config.admin_signal_execute),
		("admin_log_capture", &config.admin_log_capture),
		("admin_room_tag", &config.admin_room_tag),
		("server_user_localpart", &config.server_user_localpart),
		("report_room", &config.report_room),
		("grant_admin_to_first_user", &config.grant_admin_to_first_user),
		("create_admin_room", &config.create_admin_room),
		("federate_admin_room", &config.federate_admin_room),
		("sentry", &config.sentry),
		("sentry_endpoint", &config.sentry_endpoint),
		("sentry_send_server_name", &config.sentry_send_server_name),
		("sentry_traces_sample_rate", &config.sentry_traces_sample_rate),
		("sentry_attach_stacktrace", &config.sentry_attach_stacktrace),
		("sentry_send_panic", &config.sentry_send_panic),
		("sentry_send_error", &config.sentry_send_error),
		("sentry_filter", &config.sentry_filter),
		("tokio_console", &config.tokio_console),
		("test", &config.test),
		("maintenance", &config.maintenance),
		("admin_room_notices", &config.admin_room_notices),
		("admin_output_max_events", &config.admin_output_max_events),
		("admin_output_threads", &config.admin_output_threads),
	];

	fields.into_iter()
}

fn fields_4(config: &Config) -> impl Iterator<Item = (&'static str, &dyn Debug)> {
	let fields: [(&str, &dyn Debug); 40] = [
		("save_unredacted_events", &config.save_unredacted_events),
		("redaction_retention_seconds", &config.redaction_retention_seconds),
		(
			"allow_room_admins_to_request_unredacted_events",
			&config.allow_room_admins_to_request_unredacted_events,
		),
		("disable_local_redactions", &config.disable_local_redactions),
		("enforce_erasure_over_federation", &config.enforce_erasure_over_federation),
		("db_pool_affinity", &config.db_pool_affinity),
		("db_pool_workers", &config.db_pool_workers),
		("db_pool_workers_limit", &config.db_pool_workers_limit),
		("db_pool_max_workers", &config.db_pool_max_workers),
		("db_pool_queue_mult", &config.db_pool_queue_mult),
		("stream_width_default", &config.stream_width_default),
		("stream_width_scale", &config.stream_width_scale),
		("stream_amplification", &config.stream_amplification),
		("sender_workers", &config.sender_workers),
		("listening", &config.listening),
		("config_reload_signal", &config.config_reload_signal),
		("allow_invalid_tls_certificates", &config.allow_invalid_tls_certificates),
		("access_control_allow_origin", &config.access_control_allow_origin),
		("hydra_backports", &config.hydra_backports),
		("delete_rooms_after_leave", &config.delete_rooms_after_leave),
		("one_time_key_limit", &config.one_time_key_limit),
		("single_sso", &config.single_sso),
		("sso_custom_providers_page", &config.sso_custom_providers_page),
		("oidc_aware_preferred", &config.oidc_aware_preferred),
		("appservice_dir", &config.appservice_dir),
		("database_migrations", &config.database_migrations),
		("force_migration", &config.force_migration),
		("conduit_source_media_path", &config.conduit_source_media_path),
		("conduit_media_directory_depth", &config.conduit_media_directory_depth),
		("conduit_media_directory_length", &config.conduit_media_directory_length),
		("conduit_source_media_provider", &config.conduit_source_media_provider),
		(
			"device_key_update_encrypted_rooms_only",
			&config.device_key_update_encrypted_rooms_only,
		),
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
