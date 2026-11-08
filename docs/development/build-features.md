# Build Features

The public Cargo feature surface is defined in [`src/main/Cargo.toml`](https://github.com/matrix-construct/tuwunel/blob/main/src/main/Cargo.toml).

## Cargo features

| Feature | Default | Description |
| --- | :---: | --- |
| `brotli_compression` | ✔️ | Brotli HTTP response compression and outbound response decompression. Enable `http.compression.brotli = true`; the client must accept Brotli; `network.http.brotli` controls outbound negotiation. |
| `bzip2_compression` | ✔️ | Bzip2 compression for RocksDB. Select `rocksdb.compression_algo = "bz2"`. |
| `console` | ✔️ | Interactive administration console on the terminal. Start with `--console`; an interactive terminal is needed. |
| `cpu_affinity` | ✔️ | Pin Tokio worker threads and database pool workers to CPU cores. A build without it leaves thread placement to the operating system, ignores `database.pool.affinity`, sizes the database pool from `database.pool.workers` instead of detected storage queues, and counts CPUs with the Rust standard library, which also applies cgroup CPU quotas. OpenBSD counts CPUs with `num_cpus` either way. |
| `direct_tls` | ✔️ | Serve HTTPS directly from Tuwunel. Configure the certificate and key in `[global.tls]`. |
| `element_hacks` | ✔️ | Workarounds for Element: password authentication accepts the deprecated `user` field, and lazy-loaded syncs always include redundant members. These compatibility workarounds apply without additional runtime configuration. |
| `gzip_compression` | ✔️ | Gzip HTTP response compression and outbound response decompression. Enable `http.compression.gzip = true`; the client must accept Gzip; `network.http.gzip` controls outbound negotiation. |
| `io_uring` | ✔️ | RocksDB I/O through io_uring. Requires Linux with kernel support for io_uring; liburing is needed when dynamically linked. |
| `jemalloc` | ✔️ | Use jemalloc for Tuwunel allocations. MSVC uses the system allocator; dynamically linked jemalloc builds need the allocator library. |
| `jemalloc_conf` | ✔️ | Apply Tuwunel's built-in jemalloc tuning. The `jemalloc` feature must also be enabled; this feature configures the allocator but does not select it. |
| `jemalloc_prof` | × | Compile jemalloc heap profiling support. Enable `jemalloc` and profiling through the allocator's runtime configuration; a substituted allocator must support profiling. |
| `jemalloc_stats` | × | Compile jemalloc allocation statistics support. Enable `jemalloc`; a substituted allocator must support statistics. |
| `ldap` | ✔️ | Authenticate users against LDAP. Enable and configure `[global.ldap]`; an LDAP server is needed. |
| `lz4_compression` | ✔️ | LZ4 compression for RocksDB. Select `rocksdb.compression_algo = "lz4"` or `"lz4hc"`. |
| `mas` | ✔️ | Matrix Authentication Service provisioning API. Required when `authentication.mas_secret` is set; a build without it refuses to start with a nonempty secret. |
| `media_thumbnail` | ✔️ | Generate media thumbnails. Video extraction also needs `media.video_thumbnail.command` and its external program. |
| `oidc` | ✔️ | Native OIDC server API. Required when `well_known.client` is set together with `authentication.oidc.native_auth` or any `identity_provider`; a build without it refuses to start then. Legacy SSO alone does not need this feature, but SSO with `well_known.client` set does. |
| `perf_measurements` | × | Export tracing spans and write flamegraph data. Enable `telemetry.jaeger.enable` with an OTLP collector, or `tracing.flame.enable` with a writable `tracing.flame.output_path`. |
| `release_max_log_level` | ✔️ | Omit debug and trace logging from release builds. Remove this feature to use those log levels in a release build; runtime log filters cannot restore messages omitted at compile time. |
| `sentry_telemetry` | ✔️ | Sentry error reporting and tracing integration. Enable `telemetry.sentry.enable` and configure `telemetry.sentry.endpoint`. |
| `synapse_admin` | ✔️ | Synapse admin API, client account suspend and lock endpoints, and the client `whois` alias. Most endpoints require an administrator token; shared-secret registration requires `registration.shared_secret`. |
| `systemd` | ✔️ | Systemd readiness, watchdog, socket activation, and configuration reload notifications. Requires Linux and systemd. |
| `tokio_console` | × | Tokio task instrumentation for tokio-console. Build with `--cfg tokio_unstable`, enable `tracing.tokio_console` and `logging.global_default`, and omit `release_max_log_level` for release builds. |
| `tuwunel_mods` | × | Hot reloadable development modules. Build with `--cfg tuwunel_mods` and provide loadable modules; see [Hot Reloading](hot_reload.md). |
| `url_preview` | ✔️ | Fetch URL previews for clients. Configure `media.url_preview.domain_contains_allowlist` or `media.url_preview.domain_explicit_allowlist` for permitted domains. |
| `zstd_compression` | ✔️ | Zstd HTTP compression/decompression and RocksDB compression. HTTP uses `http.compression.zstd = true` and client negotiation; `network.http.zstd` controls outbound negotiation; RocksDB uses `rocksdb.compression_algo`. |

Journal logging works independently of the `systemd` feature. It requires `logging.journald` and the presence of both `SYSTEMD_EXEC_PID` and `JOURNAL_STREAM` in the environment.

> [!IMPORTANT]
> Some features reject enabling configuration when absent from the build; see the individual descriptions above.

> [!NOTE]
>
> - The `oidc`, `mas`, and `synapse_admin` features can each be left out independently.

## Feature projection and dependency implications

The `main` crate defines every public feature and forwards it to the workspace crates that apply it. The crate column lists those crates, whether they use the feature in conditional code or to enable a dependency feature. The last column lists direct optional dependencies enabled along the forwarding path and feature changes to existing dependencies.

| Feature | Crates involved | Dependency implications |
| --- | --- | --- |
| `brotli_compression` | `api`, `core`, `router`, `service` | Compression features on existing HTTP dependencies. |
| `bzip2_compression` | `database` | RocksDB Bzip2 support. |
| `console` | `service` | `rustyline-async`, `termimad`. |
| `cpu_affinity` | `core` | `core_affinity`. |
| `direct_tls` | `router` | `axum-server-dual-protocol`, `rustls`. |
| `element_hacks` | `service` | No optional dependency activated by this feature. |
| `gzip_compression` | `api`, `core`, `router`, `service` | Compression features on existing HTTP dependencies. |
| `io_uring` | `database` | RocksDB io_uring support and its native library requirements. |
| `jemalloc` | `core`, `database` | `jevmalloc`; native allocator build. |
| `jemalloc_conf` | `core` | Allocator configuration; does not itself enable `jemalloc`. |
| `jemalloc_prof` | `core` | Profiling on `jevmalloc` if the allocator is enabled. |
| `jemalloc_stats` | `core` | Statistics on `jevmalloc` if the allocator is enabled. |
| `ldap` | `api`, `api-client`, `service` | `ldap3`. |
| `lz4_compression` | `database` | RocksDB LZ4 support. |
| `mas` | `api-mas`, `router` | Optional workspace crate `tuwunel-api-mas`. |
| `media_thumbnail` | `service` | `image`. |
| `oidc` | `api-oidc`, `router` | Optional workspace crate `tuwunel-api-oidc`; service-side issuer code remains compiled without this API feature. |
| `perf_measurements` | None | `opentelemetry`, `opentelemetry-otlp`, `opentelemetry_sdk`, `tracing-flame`, `tracing-opentelemetry`. |
| `release_max_log_level` | `admin`, `api`, `api-admin`, `api-client`, `api-federation`, `api-mas`, `api-oidc`, `core`, `database`, `matrix`, `router`, `service` | Log-level filtering on existing `log` and `tracing` dependencies. |
| `sentry_telemetry` | `router` | `reqwest`, `sentry`, `sentry-tower`, `sentry-tracing`. |
| `synapse_admin` | `api-admin`, `router` | Optional workspace crate `tuwunel-api-admin`. |
| `systemd` | `router`, `service` | `sd-notify` on Linux Unix targets. |
| `tokio_console` | None | `console-subscriber`, plus Tokio tracing support. |
| `tuwunel_mods` | `core` | `libloading`. |
| `url_preview` | `service` | `image`, `webpage`. |
| `zstd_compression` | `api`, `core`, `database`, `router`, `service` | HTTP and RocksDB Zstd support. |

> [!NOTE]
>
> - `jemalloc_conf`, `jemalloc_prof`, and `jemalloc_stats` do not select the allocator themselves.
>
> - `image` is shared by thumbnails and URL previews, so removing only one feature can leave that dependency enabled.
>
> - Shared dependencies can already be enabled by another feature.

### Crate defaults

Each workspace crate that defines `default` lists its own default features and enables the `default` of each workspace dependency that defines one. A crate built alone, such as with `cargo build -p tuwunel_service`, gets the same features it has in the full server.

The `default` of `main` also enables these crate defaults. A build from the workspace root and a build of `-p tuwunel` therefore resolve the same features and share compiled artifacts.

To build a crate alone with a dependency's feature, name the feature through that dependency. For example, use `cargo build -p tuwunel_service --features tuwunel-core/jemalloc_prof`.

## Rust `cfg` selectors

These configurations are primarily developer debugging, instruments and internal controls. The workspace registers them with `check-cfg` so rustc accepts their spelling. Build recipes can supply them separately through Rust compiler flags. They can be enabled by default in `.cargo/config.toml`.

| `cfg` name | Ours | Default | Crates involved | Description |
| --- | :---: | :---: | --- | --- |
| `core_intrinsics` | × | × | `core` | Uses the compiler breakpoint intrinsic rather than the x86_64 breakpoint fallback. |
| `disable` | ✔️ | × | `core`, `database`, `service` | Marks dormant or unfinished code. Enabling it can introduce conflicting definitions. |
| `panic_trap` | ✔️ | × | `main` | Internal modular-loader panic-hook path; not a supported standalone build switch. |
| `tokio_unstable` | × | × | `core`, `main`, `admin` | Enables unstable Tokio metrics and runtime controls; also required by `tokio_console`. |
| `tuwunel_always_prove_sendness` | ✔️ | × | `service` | Asks the trait solver to prove `OnceServices` is `Send` instead of using its manual implementation; increases compilation work. |
| `tuwunel_always_prove_syncness` | ✔️  |× | `service` | Asks the trait solver to prove `OnceServices` is `Sync` instead of using its manual implementation; increases compilation work. |
| `tuwunel_debug_logging` | ✔️ | × | `core` | Preserves the requested severity of debug-gated logging macros. Does not override compile-time maximum log levels. |
| `tuwunel_mods` | ✔️ | × | `core`, `main` | Enables development hot-reload paths together with the Cargo feature of the same name. |
| `unabridged` | ✔️ | × | `database`, `service` | Adds fine-grained tracing spans. |

> [!NOTE]
>
> - The `tokio_console` Cargo feature needs `tokio_unstable`.
>
> - The `tuwunel_mods` cfg and Cargo feature must both be enabled for hot reloading. See [Hot Reloading](hot_reload.md) for the modular build workflow.
