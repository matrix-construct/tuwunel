# Tuwunel 1.8.2

July 17, 2026

### New Features & Enhancements

- **URL preview media proxying** relays link-preview media through the server, now covering `og:video` and `og:audio` alongside images, so the third party sees Tuwunel rather than the requesting client. Nothing is stored permanently: preview media becomes a lazy `mxc://` reference fetched from source on demand. A `url_preview_user_agent` option, with a separate `url_preview_media_user_agent`, lets previews work for sites that block the default agent. Shipped by @az4521 in (#508), closing their own request for video previews (#394). The preview store is rebuilt on CBOR at the same time, replacing a byte-separated format that could shear fields.

- **Distribution packaging** expands to RPM with a COPR build pipeline (fixes #251) and a SELinux policy module shipped as a `selinux` subpackage (#412), plus an apt repository published from CI and Debian packaging that adopts an existing conduwuit or Conduit database in place. Courtesy of @x86pup.

- Online backups can now be restored and verified, joined by a `delete-backups` admin command, graciously contributed by @x86pup.

- MatrixRTC transport discovery (MSC4143) is served without an access token, so Element Call can find a server's transports; contributed by @basnijholt in (#512).

- A container `HEALTHCHECK`, backed by a new liveness-probe mode, lets orchestrators track readiness, with appreciation to @x86pup.

- A `dns_servers` config option makes the `/etc/resolv.conf` dependency optional, tip of the hat to @x86pup.

- **The Synapse-compatible admin API** grows again (#38): server-notice endpoints, user redaction and login-as, federation destination management, and media info, purge, and statistics.

- **Appservice transaction extensions** deliver richer data to appservices: device-list changes and one-time-key counts with unused fallback key types (MSC3202), and to-device events (MSC4203). Opened by @dark-collective in (#502) and (#501).

- **Forward-extremity capping and pruning** guards against extremity blowup with a scored prune engine that always leaves a survivor and protects the local server's own leaves, a cap applied on the federation receive path, and admin `room` commands to list and prune a room's extremities.

- **Local state derivation** for incoming federation events lands in observation mode (#419). The server derives an event's state from local ancestry and calls `/state_ids` only for physically absent events, running alongside the existing fetch and comparing while the fetched result stays authoritative.

- Long admin command output is split across chained reply or thread events, and oversized output is attached as an uploaded file, raised by @grinapo in (#471).

- Pushers rejected by the push gateway are now removed, backed by push-gateway conformance tests and UnifiedPush documentation, raised by @NinekoTheCat in (#20).

- Support for Matrix v1.18 and v1.19 is declared in `/versions`.

- The `max_fetch_prev_events` default is raised to 1024.

### Bug Fixes

- Federation delivery no longer runs hot against a peer that has come back. The per-server backoff gate consulted only the current time bucket, so it re-authorized attempts at every timeout boundary and never honored the computed earliest retry, and a stale set of reachability rows could keep muting a recovered server. The verdict now derives from a server's full failure history, a returning peer clears the whole streak, and the old rows are cleared once on upgrade (da0c3f600, ec049f61c). Sincere apologies to anyone whose outbound federation lagged to a server that had recovered.

- A proxy or CDN answering a federation request with non-JSON is treated as transient rather than evicting the route outright, and route override eviction is fixed for well-known and SRV-delegated topologies where it was a no-op (b33415d50).

- Tuwunel refuses to initialize over the remnants of a database that lacks a readable manifest, instead of treating them as obsolete files and deleting them on open (fixes #510). Reported and diagnosed by @ItsLiyua, whose detail on the two parallel database directories localized the cause.

- Native OIDC login completes again: the redirect-completion path returned 405 and produced a redirect Chrome refused (fixes #504, #505). Reported by @isniz and @achetronic.

- One-time-key counts match Synapse's shape, and an explicit zero count is preserved, so a client whose key pool is drained still sees `signed_curve25519` and replenishes instead of starving (007033cd5, 164b8da61). Contributed by @basnijholt in (#511).

- A soft-failed inbound event could compute an empty forward-extremity set and, once persisted, remove every leaf and wedge local sends until a remote event arrived; the previous band is now preserved, and a detached non-create local event on an empty frontier is refused rather than silently forking the room (e0f10343e, 1f1dea699).

- The inbound federation profile query returns 404 `M_NOT_FOUND` for an unknown user instead of an empty 200 (76ea07fc9).

- The room ephemeral section is always present in `/sync` responses now, thanks to @x86pup (79bb4af09).

- An empty `device_id` is treated as unspecified and a device id is generated (cfe73cbb2).

- A systemd unit no longer sticks in the deactivating state after an in-place admin restart, fixed by @x86pup (68e034d84).

- Non-Linux builds get several repairs, courtesy of @obodnikov: resource-usage reporting compiles on non-unix and no longer panics in macOS thread usage (#509), Ctrl+C actually shuts the server down on non-unix targets (#507), and platform-gated admin commands compile on every target (#506).

- Backup requests that cannot create a backup error instead of reporting success, and backup engine errors propagate rather than being swallowed (f6de800b5, 9b54209d0). Credit to @x86pup.

- @x86pup corrected documented config defaults that disagreed with the code (bb9dfb25c), and the `notification_push_path` description is set right (e16a3aea5).
