# Build Features

A plain `cargo build` uses the default features listed below, as do the published packages. Add an opt-in feature with `cargo build --features jemalloc_stats`. To remove features, use `--no-default-features --features <comma-separated-list>` and name the features you want to keep. These flags also work with `cargo build --release`.

The `oidc`, `mas`, and `synapse_admin` surfaces can each be left out independently. A server configured to use a surface its build lacks refuses to start where that surface has an activation setting, as listed below. `synapse_admin` has no activation setting, so a build without it simply omits those endpoints.

| Feature | Default | What it does | Runtime requirement |
| --- | --- | --- | --- |
| `brotli_compression` | Yes | Brotli HTTP response compression and outbound response decompression. | `brotli_compression = true`; the client must accept Brotli; `request_brotli` controls outbound negotiation. |
| `bzip2_compression` | Yes | Bzip2 compression for RocksDB. | Select `rocksdb_compression_algo = "bz2"`. |
| `console` | Yes | Interactive administration console on the terminal. | Start with `--console`; an interactive terminal is needed. |
| `direct_tls` | Yes | Serve HTTPS directly from Tuwunel. | Configure the certificate and key in `[global.tls]`. |
| `element_hacks` | Yes | Workarounds for Element: password authentication accepts the deprecated `user` field, and lazy-loaded syncs always include redundant members. | None. |
| `gzip_compression` | Yes | Gzip HTTP response compression and outbound response decompression. | `gzip_compression = true`; the client must accept Gzip; `request_gzip` controls outbound negotiation. |
| `io_uring` | Yes | RocksDB I/O through io_uring. | Linux with kernel support for io_uring; liburing is needed when dynamically linked. |
| `jemalloc` | Yes | Use jemalloc for Tuwunel allocations. | MSVC uses the system allocator; dynamically linked jemalloc builds need the allocator library. |
| `jemalloc_conf` | Yes | Apply Tuwunel's built-in jemalloc tuning. | The `jemalloc` feature must also be enabled. |
| `jemalloc_prof` | No | Compile jemalloc heap profiling support. | Enable `jemalloc` and profiling through the allocator's runtime configuration; a substituted allocator must support profiling. |
| `jemalloc_stats` | No | Compile jemalloc allocation statistics support. | Enable `jemalloc`; a substituted allocator must support statistics. |
| `ldap` | Yes | Authenticate users against LDAP. | Enable and configure `[global.ldap]`; an LDAP server is needed. |
| `lz4_compression` | Yes | LZ4 compression for RocksDB. | Select `rocksdb_compression_algo = "lz4"` or `"lz4hc"`. |
| `mas` | Yes | Matrix Authentication Service provisioning API. | Required when `mas_secret` is set; a build without it refuses to start with a nonempty secret. |
| `media_thumbnail` | Yes | Generate media thumbnails. | Video extraction also needs `media_video_thumbnail_command` and its external program. |
| `oidc` | Yes | Native OIDC server API. | Required when `well_known.client` is set together with `oidc_native_auth` or any `identity_provider`; a build without it refuses to start then. Legacy SSO alone does not need this feature, but SSO with `well_known.client` set does. |
| `perf_measurements` | No | Export tracing spans and write flamegraph data. | Enable `allow_jaeger` with an OTLP collector, or `tracing_flame` with a writable `tracing_flame_output_path`. |
| `release_max_log_level` | Yes | Omit debug and trace logging from release builds. | Remove this feature to use those log levels in a release build. |
| `sentry_telemetry` | Yes | Sentry error reporting and tracing integration. | Enable `sentry` and configure `sentry_endpoint`. |
| `synapse_admin` | Yes | Synapse admin API, client account suspend and lock endpoints, and the client `whois` alias. | Most endpoints require an administrator token; shared-secret registration requires `registration_shared_secret`. |
| `systemd` | Yes | Systemd readiness, watchdog, socket activation, journal logging, and configuration reload integration. | Linux and systemd; journal logging is controlled by `log_journald`. |
| `tokio_console` | No | Tokio task instrumentation for tokio-console. | Build with `--cfg tokio_unstable`, enable `tokio_console` and `log_global_default`, and omit `release_max_log_level` for release builds. |
| `tuwunel_mods` | No | Hot reloadable development modules. | Build with `--cfg tuwunel_mods` and provide loadable modules; see [Hot Reloading](hot_reload.md). |
| `url_preview` | Yes | Fetch URL previews for clients. | Configure `url_preview_domain_contains_allowlist` or `url_preview_domain_explicit_allowlist` for permitted domains. |
| `zstd_compression` | Yes | Zstd HTTP compression/decompression and RocksDB compression. | HTTP uses `zstd_compression = true` and client negotiation; `request_zstd` controls outbound negotiation; RocksDB uses `rocksdb_compression_algo`. |
