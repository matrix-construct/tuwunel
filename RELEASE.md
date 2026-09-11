# Tuwunel 1.9.1

September 12, 2026

> [!IMPORTANT]
> **Upgrading from 1.8.3 or earlier?** The first boot runs a one-time database migration before the listener opens, so back up first ([docs/backups.md](https://github.com/matrix-construct/tuwunel/blob/v1.9.1/docs/backups.md)). It can take a while on a large database, but progress now appears in the log and in `systemctl status` every fifteen seconds, and a normal stop is honored at the next safe point and resumes on restart; only a forced kill is unsafe, and the recovery is your backup. A standalone binary in a chroot, distroless, or `FROM scratch` image also needs a CA bundle or `SSL_CERT_FILE` since 1.9.0; the container image and both packages include one.

### New Features & Enhancements

- **Animated thumbnails are generated on demand (MSC2705)**, shipped by @x86pup. An explicit `animated=true` request can produce a bounded, cached GIF from animated GIF, WebP, or APNG media, while absent or false requests remain still. Current shipping clients omit the parameter, so the server capability arrives ahead of client adoption. Normal builds include it and `media_thumbnail_animated` defaults to `true`; frame, pixel, and concurrency limits bound its resource use.

- **Prebuilt Nix store paths are available from project caches**, courtesy of @x86pup. The flake covers packages and development shells, although unattended Nix installations must trust the substituters and public keys explicitly. Cache upload is non-fatal, so an individual release may still have incomplete coverage.

- **Debian and RPM packages can adopt an existing Conduwuit or Conduit database**, with appreciation to @x86pup. Adoption uses a same-filesystem rename into `/var/lib/tuwunel`, refuses a nonempty destination, and waits for the old service to stop. This relocates the database; normal startup migrations still perform any format conversion.

- **Users can control invitations by sender (MSC4155)** through stable and unstable invite-permission account data. Blocked invitations are refused, while ignored invitations stay out of sync, push, and automatic acceptance until policy changes. Graciously added by @x86pup.

- Operators can enable automatic invite acceptance for local users with the reloadable `auto_accept_invites` option, which defaults to `false`. Direct-only and local-sender-only filters are available, and accepted direct chats are recorded in `m.direct`. Opened by @sanyamseac in (#513) and implemented by @x86pup.

- Tuwunel now validates custom status, call, and timezone profile fields and serves changed values through the client-enabled Sliding Sync profiles extension (MSC4262 and MSC4426). This is the server foundation used by clients such as Element X, but does not add the separate legacy `/sync` delivery path. Credit to @x86pup.

- Federated events can build state from complete local history, reducing dependence on an origin server's `/state_ids` response. `resolve_state_locally` is reloadable and defaults to `true`; incomplete or unsafe local results retain the existing federation fallback.

- New admin commands provide diagnostics through parallel federation requests: `query feds version`, `event`, `state`, and `head`. Surveys report per-peer outcomes and require explicit confirmation above 2,048 destinations; the `head` query can cause a remote server to persist an otherwise unused short event identifier (e2c74fda9, 23789f48d, 9acf26f69, 6bfb79717, 85db28f0b, e77de564f, 3e9db3f65, 84e83957b, 5a2b4d25e).

- URL preview cache lifetime is configurable with reloadable `url_preview_cache_ttl`, courtesy of @x86pup and inspired by @alemidev in (#558). The 24-hour default is unchanged; values above seven days require a restart for the RocksDB retention floor, and size limits can still evict entries earlier.

- URL preview requests can carry an operator-selected `Accept-Language` header for pages, media, and oEmbed. The reloadable `url_preview_accept_language` option defaults to unset, and cached previews keep their prior language until expiry. Implemented by @tototomate123 in (#580), after @dlrudie opened (#575).

- A new admin command creates a complete RocksDB checkpoint or exports one named column family. A single-column export is not a restorable server backup, and the default destination shares the live database filesystem (b11b5e123).

- Argon2id memory, time, and parallelism costs are configurable for newly written password hashes. Existing hashes retain their stored parameters, and the production defaults remain 19,456 KiB, two iterations, and one lane (0635833d1, 3d8267c31).

- Alpine source builds have a recipe tested on x86_64 and aarch64, and musl builds now route Tuwunel and RocksDB allocations through the pinned jemalloc. No Alpine artifact or support commitment is added; `-C target-feature=-crt-static` is required, and host-specific `target-cpu=native` output may not run on older CPUs. Thanks to @x86pup.

- Long startup migrations now report their phase, elapsed time, and position every fifteen seconds in logs and systemd status. Graceful stops are honored at step boundaries and resume unfinished work on restart; forced kills remain unsafe. The shipped Podman unit gains migration-aware health and stop timing. Credit to @x86pup.

### Bug Fixes

- **Sliding Sync now delivers subscribed-room required state and changed room configuration without waiting for timeline events or a client reload.** The first collection after upgrading may resend the configured timeline window because older connection records lack its configuration hash. Reported by @AngelBePro in (#560) and fixed in (#561).

- **Legacy full-state sync includes quiet joined rooms and their latest state**, even with an empty timeline or `timeline.limit: 0`. Graciously contributed by @basnijholt in (#583).

- **Element X for iOS location markers move as room state changes arrive.** Incremental Sliding Sync now compares selected state at the delivered cursor with current resolved state, including across federation forks. Thank you @utop-top for reporting (#569).

- FluffyChat can clear stale unread markers after a receipt advances because legacy sync now emits an explicit zero when the room or thread read cursor proves a reset, including on initial sync. Credit to @ruka-hamanasu for (#564).

- Private read markers targeting backfilled events become a safe no-op instead of failing the entire read-marker request, so public receipts and unread resets can still complete. Shipped by @ruka-hamanasu in (#566).

- Directional `/messages` bounds now honor valid global sync tokens even when the room has no event at that exact position, while malformed tokens return `M_INVALID_PARAM`. With appreciation to @basnijholt for (#574).

- Device-key uploads fail safely when an existing row cannot be read or decoded instead of overwriting identity material. Courtesy of @basnijholt in (#577).

- Large `query storage sync` operations now report a single copy summary rather than overflowing the admin response with one line per object. Thanks to @tototomate123 for (#579) and @justinbrick for reporting (#571).

- Both Synapse-compatible account-deactivation routes now make the local user leave their rooms. Reactivation does not restore prior memberships. Graciously contributed by @obodnikov in (#584) and (#585).

- Native OIDC browser callbacks now complete in Chrome when the client and homeserver use different origins. This built-in authentication path remains gated by `oidc_native_auth`, which defaults to `false`. Reported by @asmj1108 in (#573).

- Room enable and unban commands now explain when an immutable `m.federate: false` creation setting prevents remote joins or invites, with remote-target invite coverage for MSC4361. Thank you @tcyrus for reporting (#568).

- Sliding Sync direct-message and room-tag filters now use stored `m.direct` and tag account data, including account-data changes during a long poll. Shipped by @x86pup.

- Deleting a local room alias removes only that alias from the reverse index and updates canonical-alias state when the sender has permission, courtesy of @x86pup (9221685cb, aa9bf1b86, 3f44bdaf6, 295c0377d).

- Invitation state handling now preserves authoritative and stripped state, including senderless stored invites, with appreciation to @x86pup (31c547f30, a10704671, 49f9d0e05, 36a73db5a).

- Backup restore is now an explicit one-process action through the `restore-backup` command-line option. Tuwunel refuses `database_restore_backup` in TOML, environment configuration, and generic option overrides, and regeneration omits it, preventing a destructive restore request from persisting or repeating (7a0cdb63f, 956d6c08b).

- OIDC authorization now requires explicit user approval before releasing a code to a dynamically registered client whose redirect target the operator has not vetted. `oidc_require_client_approval` defaults to `true`; the prompt informs but does not authenticate the client (143562be5).

- Server notices now create and reuse private notice rooms, deduplicate transactional retries, and prevent invite rejection while still allowing joined users to leave (868b846ae).

- Federation history and fetch recovery are more reliable: empty or duplicate-only backfill responses no longer count as progress, timestamp answers are validated against the room and requested time after ingestion, incompatible single-flight requests no longer share results, and previous-event recovery width is reloadable (65eb68d8c, e3a5dd107, b1389597b, 35fc5c123, 14bf02d5b).

- Declined short-ID migrations now finalize safely, forbidden-name scans stop promptly when shutdown is requested, and injectivity patch additions cannot collide with removals (8f0035e63, 032248ee8, bfe4a276e, 67fb14d54).

- Key-backup versions and etags are validated while mutations are serialized (1dd1b12f7, 4106440d6). Account erasure closes login and thread leaks and removes contact bindings (942bf2aab, 090e42001).

- Cross-signing uploads now validate ownership, role usage, key shape, and signature relationships while keeping foreign signatures private (e87a9aa2b, 1cd92aac5). To-device events addressed to the server user are delivered (ad1ed82c3).

- Room-send transaction identifiers are scoped by room and event type, with checked compatibility for legacy rows (99908c1e3). Redactions and private receipts reject cross-room event targets (6c02023c5, 323e1ba03).

- Thread updates are room-bound and transactional, and a receipt on the root no longer clears unread replies (5d4db96e9, bffd8e679). Retained events and expiry indexes are stored atomically and replay in order (65d2483eb, 3a8d1c482).

- Passwordless user creation writes its origin and disabled-password marker atomically (3a835c8e6). Presence queries return a default status when no row exists (3f7869bd9).

- Sync and timeline edge cases are corrected: left-room state uses the correct incremental anchor, ignored replies no longer remove the thread root, and exhausted filtered backward pages advance their cursor (83b6253a4, 0ea5de07b, 609c5118b).
