# Two Tuwunel

November 10, 2026

> [!IMPORTANT]
> Tuwunel 2.0 changes how it is built, packaged and configured. Before upgrading, note the following:
>
> - **Release artifacts are named `-default-` instead of `-all-`.** Published binaries, packages and container images are now built with the default feature set. Scripts that download a particular artifact need the new name.
>
> - **The build features have changed.** Email, JWT login, S3 and GCS storage, admin Markdown and CPU affinity gained their own switches (all on by default), and bzip2, console, direct TLS, LDAP and LZ4 joined the defaults. The full list is in [Build Features](https://matrix-construct.github.io/tuwunel/development/build-features.html#cargo-features).
>
> - **The crates have changed.** The API is split into `tuwunel_api_client`, `tuwunel_api_federation`, `tuwunel_api_oidc`, `tuwunel_api_mas` and `tuwunel_api_admin`, with a new `tuwunel_matrix` crate, so log filters naming `tuwunel_api` need updating. See the [project layout](https://matrix-construct.github.io/tuwunel/development.html#tuwunel-project-layout).
>
> - **The configuration file is organized into tables, and existing files remain compatible.** Every flat setting name keeps working throughout 2.x. To move a file to the new layout, see [Configuration regeneration](https://matrix-construct.github.io/tuwunel/configuration/regeneration.html).
>
> - **Later 2.x releases will bring further feature and crate changes.** Check the pages linked above when upgrading within 2.x.

### New Features & Enhancements

- **Cancellable delayed events (MSC4140).** A client can schedule a message or state change for later, get back a `delay_id`, and restart the timer, cancel the event or send it early. The headline use is MatrixRTC: while someone is in an Element Call session, a supporting client keeps postponing a scheduled empty call-membership event, and if the app crashes, is force-closed or loses its connection, the server sends that leave itself and the stale caller drops out of the call. That is the ghost caller Element and Commet users described after a force-close or shutdown in (#178). matrix-js-sdk defaults to an 8 second delayed leave restarted every 5 seconds, and `lk-jwt-service` can manage a delegated leave without an access token. Scheduled events are stored, so they survive a restart, and overdue ones are attempted on the first pass after boot. `max_delay_s` (default 86400) and `max_scheduled` (default 100 per user) in `[global.rooms.delayed_events]` are reloadable, and 0 for either turns scheduling off. Thanks to @neovimbtw (#555) and @basnijholt (#684), whose pull requests shaped the design, to @utop-top for raising the need in (#178), and to @Lama-Thematique and @SCSIExpress for the force-close reproductions.

- **Email notifications for unread messages**, requested by @doits in (#620). In Element Web's notification settings, turn on email notifications for a verified address and unread events selected by your push rules arrive as a plain-text digest after `push.email_delay` (default 600 seconds, 0 for no wait). A digest groups up to 20 notifications by room, cuts excerpts at 300 characters and shows encrypted content as `Encrypted message`; reading the messages first keeps them out of the mail, and turning the toggle off removes the pusher. It needs `smtp.connection_uri`, `smtp.sender` and `well_known.client` (all unset by default) and the `email` build feature, on in default builds. Without SMTP, enabling the toggle now fails with `M_INVALID_PARAM` instead of storing a pusher that can never send.

- **Profile change propagation (MSC4466).** A client can choose which rooms receive a new global display name or avatar by sending `computer.gingershaped.msc4466.propagate_to` as `all`, `unchanged` or `none`. Sable Legacy offers it under Settings > Account > Profile as "Profile change propagation" with "All rooms", "Unchanged rooms" (its default when the server advertises support) and "Global only". A request without the parameter updates every room as before, and room-specific custom membership keys now survive a global change. Tuwunel also caps client-supplied membership content with `profile.member_event_max_bytes` (default 4096 canonical JSON bytes, reloadable) across joins, initial state, state events and scheduled state, answering `M_BAD_JSON` when it is exceeded; display names are limited to 256 characters and avatar strings to 1000 bytes. Shipped by @x86pup.

- **`!admin integrity` inspects a database without changing it**, opened by @grinapo in (#478). `!admin integrity list` shows the checks and `check` reports each as clean, as findings with counts and up to 8 samples, or as failed with the error. They cover schema and migration markers, checksummed column scans, unused columns, malformed rows, derived indexes, foreign-database adoption, local media filenames and short-ID consistency. Checks that compare columns need a read-only open, so stop the server and use the maintenance guide's temporary read-only process (`rocksdb.read_only`). Five repairs (`pdu-room-ids`, `highlight-keys`, `relation-index`, `thread-summaries` and `thread-activity`) can be scheduled with an explicit confirmation flag and run at the next start, before the server takes traffic.

- **Room version 12 `createRoom` accepts a custom creation timestamp.** A client or bridge can set the `m.room.create` timestamp with `fi.mau.origin_server_ts` (the mautrix extension, also accepted as `origin_server_ts`) and send the hashed room ID it expects as `room_id` or `fi.mau.room_id` (MSC4291). Repeating the same fixed inputs returns `M_ROOM_IN_USE`; an ID mismatch, or a timestamp more than one minute ahead of the server clock, returns `M_INVALID_PARAM`. When the server picks the timestamp, a collision is retried once with an eight character `uwu.woof` nonce in the create content. Courtesy of @x86pup.

- **Nix builds choose a CPU with `target_cpu`**, with credit to @x86pup. The override replaces `x86_64_haswell_target_optimised` and applies the CPU to both Rust and RocksDB's C++; `"x86-64-v2"`, `"x86-64-v3"` and `"x86-64-v4"` also add PCLMUL for RocksDB, so those builds need the selected level plus PCLMUL on the machine that runs them. The Haswell-suffixed static outputs and the `x86_64-linux-gnu` family, which actually built musl, are gone; the static cross outputs are `x86_64-linux-musl` and `aarch64-linux-musl`, and the flake exposes 32 packages per system instead of 54. A configuration naming a removed output must switch to a musl output or override `target_cpu`.

- Hosted TURN relays work without a shared secret, raised by @k3-cat in (#27). Set `turn.cloudflare.key_id` with a token, or point `turn.endpoint.url` at any endpoint that returns expiring credentials and TURN URIs, and Tuwunel serves them from `/_matrix/client/v3/voip/turnServer`, which matrix-js-sdk's legacy calling uses. Credentials are cached and renewed after half their lifetime (`turn.ttl` defaults to 86400 seconds; Cloudflare caps at 48 hours). Configure one provider, not both: a provider replaces local TURN URIs, shared secret and static credentials, and neither is selected by default.

- Dehydrated devices page their queued key messages over `GET` (MSC3814), graciously contributed by @livenson in (#646). Clients on the current matrix-js-sdk rehydration loop can fetch the room keys that arrived while no ordinary client was online, using the `from` token, instead of failing with a 405. A page holds at most 50 events and `next_batch` is omitted when nothing remains, so a last page of exactly 50 costs no empty extra request. `POST` stays for older clients.

- Event reports tell moderators what was reported, with appreciation to @villetacore for (#680), inspired by @iwalkalone69's request in (#191). The report-room notice shows the first 500 characters of the message (literally, so Markdown or HTML in it stays text), the room type and local alias, and for a local sender the `!admin users redact-event <event_id>` command. Encrypted events show `unavailable`, and automation that parses the old notice text will see the added fields.

- `!admin query pusher list-queue <user_id> <pushkey>`, `retry-queue` and `drop-queue` manage one pusher's backlog without removing the pusher; thank you @sanyamseac for opening (#600). Retry keeps the backoff and presence suppression, and new notifications can refill a dropped queue.

- `profile.enable_set_avatar_url = false` (default `true`, reloadable) stops ordinary users setting or clearing their avatar, alongside the display-name control from 1.9.4; tip of the hat to @arnolicious, who raised (#607). Blocked requests get 403 `M_FORBIDDEN`, clients see it in the `m.set_avatar_url` and `m.profile_fields.disallowed` capabilities, and server admins and appservices are exempt.

- `!admin debug show-remote-pdu <event_id> <server>` asks a chosen server for an event and prints the returned PDU as JSON without importing it, courtesy of @dasha-uwu.

- **Configuration is organized into subsystem tables.** Settings sit in documented sections such as `[global.http]`, `[global.rocksdb]` and `[global.media.url_preview]`, with comments beside each table in `tuwunel-example.toml`. Every supported flat name keeps its meaning throughout 2.x, so existing files need no rewrite: `rocksdb_checksums` and `rocksdb.checksums` are one setting, and `TUWUNEL_ROCKSDB__CHECKSUMS` is the hierarchical environment spelling while `TUWUNEL_ROCKSDB_CHECKSUMS` still works. Regeneration writes canonical paths without applying anything; keep your original file if you may return to 1.x. Spelling one setting two ways in a file is an error even when the values agree, and a configuration path that is a directory or device now fails startup instead of reading as empty.

- **Published downloads and container tags are now default-feature builds.** Artifact names that carried `-all-` now carry `-default-`, for example `release-default-x86_64-v1-linux-gnu-book`, so scripts that fetch a download need the new name. Default builds include email, JWT login, S3 and GCS storage, admin Markdown and CPU affinity, and leave out five opt-in features: `jemalloc_prof`, `jemalloc_stats`, `perf_measurements`, `tokio_console` and `tuwunel_mods`. Debian and RPM packages and the Nix Docker build use the same set. Debug and trace messages removed by the default `release_max_log_level` feature cannot be restored with a log filter, so build without it to get them. BSD builds should leave out `io_uring` and `systemd`.

- **Build features gain switches for email, JWT login, S3 and GCS storage, admin Markdown and CPU affinity.** `email`, `jwt_login`, `s3_storage`, `gcs_storage`, `markdown` and `cpu_affinity` are independent features, all on by default, so a custom build can drop their dependencies. Without them a configured SMTP URI is rejected, enabled JWT login is rejected at startup and reload, enabled cloud storage providers are rejected at startup, admin messages go out as literal Markdown source, and database workers are no longer pinned to CPUs. Bzip2, console, direct TLS, LDAP and LZ4 join the defaults, and the OIDC, MAS and Synapse-admin APIs become separate default-on features. Custom feature recipes that relied on the old forwarding need review.

- **The API surface is split into crates.** Client, federation, OIDC, MAS and Synapse-admin routes move into `tuwunel_api_client`, `tuwunel_api_federation`, `tuwunel_api_oidc`, `tuwunel_api_mas` and `tuwunel_api_admin` (the Synapse-compatible API; `tuwunel_admin` stays the admin command crate), and event and PDU types plus state resolution move into a new `tuwunel_matrix` crate. For operators it shows in log filters: one written for `tuwunel_api` no longer covers handlers now in the new crates, and state resolution logs appear under `tuwunel_matrix`.

- Simplified sliding sync delivers to-device messages in acknowledged pages and reports current one-time-key counts on every pass (MSC4538, MSC4543). The to-device extension acknowledges only the `since` the client sends and pages queued messages (100 by default), so a matrix-rust-sdk client such as Element X can drain pending room keys without acknowledging ones it never received. Clients opt in with `to_device.enabled` and `e2ee.enabled`.

- An appservice registration can name a Unix socket, for example `url: unix:///run/mautrix-whatsapp/appservice.sock`, so a bridge needs no TCP listener; inline TOML registrations work too. Transactions and pings both use the socket, which Tuwunel must be able to write to. Unix platforms only, and the bridge must listen on the socket itself.

- `smtp.forbidden_email_domains` (default empty, reloadable) holds regular expressions matched against the email domain when someone registers with an address or adds one; a match fails with `M_THREEPID_DENIED`. `["(^|[.])example[.]com$"]` blocks the domain and its subdomains, since expressions are not anchored for you. SSO registration, identity-provider addresses, existing bindings and password reset are unaffected.

- Sentry reporting is built into default builds and stays off until `telemetry.sentry.enable = true`. Changing any Sentry setting needs a restart; a reload that touches one returns an error. Reports can include user IDs, room IDs, OS and Rust details and the host's hostname, even with `send_server_name` left `false`.

- `/_matrix/client/v3/capabilities` advertises the stable `m.preview_url` alongside `io.element.msc4452.preview_url` (MSC4452), so a client can learn that URL previews are off before asking for one. Both read `{"enabled": false}` until a URL preview allowlist is non-empty, and builds without the `url_preview` feature refuse previews with 403.

### Bug Fixes

- **Media uploads stop before the disk fills.** Local uploads are refused with 403 `M_RESOURCE_LIMIT_EXCEEDED` while a receiving local filesystem has less than `media.upload_min_free_space` free, and resume when space returns. The default is 1 GiB (reloadable, `0` disables it), so a server already below it refuses uploads after upgrading until space is freed or the floor lowered; a mount reporting zero free space needs another floor or `0`. Messages, sync, downloads and remote media are unaffected. Graciously contributed by @obodnikov in (#648).

- **Faster joins from Synapse no longer lose members.** Federation `/state` and `/state_ids` now include the auth chains for every returned state event, not only the requested one, so a Synapse server completing a faster join gets the memberships it needs. @dark-collective reported (#667) a Synapse 1.161.0 case where the joining server saw 7 of 12 members. Thank you!

- **The `.deb` needs glibc 2.39 or newer, so Debian 12 and Ubuntu 22.04 cannot install it.** The 2.0 package declares `libc6 (>= 2.39)` on amd64 and arm64, as 1.9.4 did. Debian 13 and Ubuntu 24.04 install it; on Debian 12 (glibc 2.36) and Ubuntu 22.04 (2.35) apt keeps offering 1.9.3-1, the last static build, so use the container image or a static binary. @afiskon reported this in (#679).

- The container image now carries Debian's current CA bundle, without the `emSign ECC Root CA - G3` and `AAA Certificate Services` roots the 1.9.3 and 1.9.4 images trusted, and `rustls-webpki` moves to 0.103.16. Older images can use `SSL_CERT_FILE` or mount the host bundle at `/etc/ssl/certs/ca-certificates.crt`. Thank you @TroyHernandez (#665).

- Thanks to @geraldpeng6, who reported (#685) and fixed (#686) sliding sync showing an accepted invitation as invited: the timeline carried the `join` while `required_state` still said `invite`, which matrix-rust-sdk 0.19.1 reads. Current room state is now installed before the event is published to sync.

- Tip of the hat to @rferranti for (#687): simplified sliding sync now packs all public receipts and the requester's private one on a shared event, not one user per event. With seven readers on the latest message, Element X could drop your own receipt and show the room unread.

- **Idle sliding sync connections wait instead of polling every half second.** With E2EE on, an empty `device_unused_fallback_key_types` list counted as content, ending each long poll on any server activity, as MindRoom bots saw. Polls now answer on timeout or relevant activity. Shipped by @basnijholt in (#658).

- Sliding sync stops resending unchanged room state, with credit to @dark-collective for (#662) and its measurements. Wildcard state in a remotely joined room no longer resends unchanged pre-join events on each unrelated change, and a retry repeats only the delta; in the reproduction, five rooms with 50 call-member events each had grown from 820 to 59,487 bytes on retry.

- Uploaded JSON files come back byte for byte. A 23-byte pretty-printed object was stored as the 13-byte `{"a":2,"b":1}`; both upload paths now skip JSON handling whatever the content type, though already rewritten files stay that way. @ivanopcode reported (#666) and fixed it in (#668). Thank you!

- Leaving and forgetting a room on one device now removes it on your others. Forgetting deleted the leave record other devices' incremental sync needed, so in @PaulaBras's case (#677) the room stayed joined on a second device. Thanks also @furfy (#542). `!admin rooms prune-empty` keeps rooms a local user left without forgetting, as `rooms.prune_require_forgotten` defaults to `true`; `false` restores the old pruning.

- @barnabys-drew reported in (#670) that after logging in on a new browser and joining an encrypted space, matrix-js-sdk's Rust crypto encrypted only to old devices, so the new one showed "Unable to decrypt". Every encrypted-room joiner now appears in `device_lists.changed`, even if you share another encrypted room. Thank you!

- Refresh and logout no longer race, with appreciation to @basnijholt for (#629). Device lifecycle and token operations share a per-device lock (Matrix and OIDC alike), so two exchanges of one refresh token within `authentication.refresh_token.reuse_grace` (default 15 seconds) yield one successor.

- Thanks to @1chunghu for (#672): once every reply in an Element Web or Desktop thread is deleted, the empty thread leaves `/threads`, so no deleted-message placeholder survives Clear cache and reload. The first start after upgrading rebuilds the thread activity index once, in time proportional to stored thread roots.

- A stale device no longer drags the fully-read marker backwards, courtesy of @ruka-hamanasu (#565), seen in FluffyChat and SchildiChat Next. Identical or older `m.fully_read` positions are ignored; clients may move it back with `com.beeper.allow_backward` (MSC4446, advertised as `com.beeper.msc4446`).

- Bridges and bots using their own token are no longer treated as guests, graciously contributed by @PolynomialDivision in (#654). Appservice users can join guest-forbidding rooms, browse the directory and get TURN credentials; `whoami` reports `is_guest: false`.

- JWT sessions can expire, shipped by @ivanopcode (#683), who requested it (#674). With `jwt.expire_access_token = true` (default `false`), a JWT login gets an access token lasting `authentication.access_token_ttl` (default seven days, not the JWT's `exp`) and no refresh token. On expiry the client gets `M_UNKNOWN_TOKEN` with `soft_logout: true`, keeping device and keys, and signs in again. Existing sessions keep their lifetime; remove old devices when enabling this.

- Former members read room state as of their departure, co-authored by @basnijholt. After a leave or ban, state, state-key and member-list reads return the departure snapshot. Forgetting a non-world-readable room ends this access, and past membership alone no longer grants a private room's summary or alias list.

- Federation event storage keeps verified copies, co-authored by @basnijholt. During `send_join` an accepted copy is kept, and a redacted incoming copy never replaces an existing one. Events without recorded state, such as creates or backfilled events, are no longer visible to everyone, and `/context` selects state like `/state` does.

- Native OIDC clients can return on any loopback port. @elee1766 reported in (#655) FluffyChat Desktop failing on `http://localhost:3001` as an invalid redirect; the port may now differ from the registered redirect for `localhost`, `127.0.0.1` and `[::1]`, while scheme, host, path and query must match. Requires built-in OIDC. Thank you!

- QR login scans reach the server again (MSC4388). Sable-next's scan sent the transaction-bearing `PUT`, got 404 and called QR login unsupported; sends now use that path, retries replay the original token, and a stale send under another transaction gets 409 `IO_ELEMENT_MSC4388_CONCURRENT_WRITE`. Reported by @s3-odara in (#682).

- SSO sign-in works when the client's redirect URL has its own query. @valentimarco reported in (#659) Authentik callbacks failing with `M_UNKNOWN: unsupported value`, because a redirect like `https://client.example/return?a=1&state=abc` leaked its `state` into a second form field. The grant cookie is now encoded.

- Passwordless SSO accounts on multi-provider servers can re-authenticate. @basnijholt reported in (#688) that replacing cross-signing keys, deleting devices or deactivating gave empty UIAA `flows` for a legacy SSO device; the account's linked, still-configured provider now supplies the `m.login.sso` challenge.

- A message backdated by an appservice on another server (`ts=`) now appears in the timeline, not only by event ID, when it extends known history; credit to @cfpwastaken, who reported it in (#673) with Continuwuity 26.9.1 and Element 1.12.25.

- Retrying `PUT /_matrix/client/v3/rooms/{roomId}/redact/{eventId}/{txnId}` after a lost response returns the original redaction, not a second one; thanks to @changl for (#675).

- With federation disabled, a remote room summary (`/_matrix/client/v1/room_summary/...`) returns 403 `M_FORBIDDEN` instead of a 500, as @rijenkii reported in (#664). Thank you!

- Thank you @FlorianBerthold for catching in (#647) that `PUT /_synapse/admin/v2/users/{user_id}` with `admin: false` returned 500 for ordinary accounts after applying other fields. Demoting a non-admin now succeeds, repeatedly too.

- Configuration mistakes now fail at startup, not in use. Credit to @Lasanyan for (#663), which quotes size examples so `max_request_size = "24 MiB"` is valid TOML. Startup and reload also refuse equal or inverted space-cache TTL bounds, inverted typing timeouts, JWT login with an empty key and media storage with no upload destination; fix these before upgrading.

- The 1.9.4 startup message about short-id repair residue is now debug level, not a warning. @dogarrowtype reported in (#676) a single residue sample after upgrading on AlmaLinux 10, the server otherwise healthy.

- Rust 1.98.1 builds no longer overflow the compiler's query depth on the `admin_grant_registration` test; the redaction check is now boxed. @NiklausHofer reported it on Gentoo (#650) and proposed a fix (#651); @divideableZero confirmed on Alpine.

- Guides now match the software, thanks to @nisbet-hubbard (#681), @misterboo (#638) and @bdfd9 (#661). The Nginx guide names `X-Forwarded-For` for client-IP handling; FluffyChat Web operators point `website`, `logoUrl`, `tos` and `privacyPolicy` at the serving origin; and the ffmpeg recipe starts from `debian:trixie-slim`, since the scratch image has no `/bin/sh`.

- **`media.url_preview.domain_contains_allowlist` now matches as documented: the host must contain the entry.** `"youtube.com"` now admits `www.youtube.com` and any host containing that text, lookalikes included, while an entry longer than its host (`"www.youtube.com"` for `youtube.com`) or a host that is only part of an entry (`oogle.com` under `"google.com"`) stops matching. Empty entries and entries with `/` warn at startup; the longer-than-host case does not, so review the list. `domain_explicit_allowlist` matches exact hosts (207e4be08b).

- With `authentication.login.with_password = false`, the native OIDC page offers identity providers instead of a password form, and password registration (now also needing `registration.enable`) is refused instead of creating an account that cannot sign in. LDAP uses the password route; keep it `true` for LDAP (0ee8f0722f, c805db25d0).

- Password attempts are isolated by client address. Wrong-password limits are keyed by account and address, so one address no longer drains another's allowance, and a per-address bucket (`rate_limiting.login.address`, burst 20 then one per second, IPv6 per /64) bounds attempts across accounts; loopback peers skip only that bucket. The address is the socket peer unless `http.ip_source` names a header source, so a proxy relying on forwarded headers must set it (restart required) (ed73ced05f, b9cf0a6848).

- `!admin users reset-password` now signs the user out of every device unless `no-logout-devices` is given. User-wide OAuth deletion removes sessions with unresolvable providers and reports them, and new SSO users get a verified email bound for password reset when SMTP is configured (82e806e821, cfa1987daa, 9863bc3521).

- Federation requests authenticated by server signature, GET included, must carry canonical JSON: `{"x":0.5}` or an out-of-range integer like `9007199254740992` gets 400 `M_BAD_JSON`, and malformed or whitespace-only signed bodies get `M_NOT_JSON`. Client requests stay lenient (fe793a77a9).

- Federated room version 12 invites must carry the create event in stripped state (MSC4311), or get `M_MISSING_PARAM` even with `federation.enforce_stripped_state_pdu_validation` at default `false`. Inviting, joining or knocking on an unsupported room version returns `M_INCOMPATIBLE_ROOM_VERSION`, and stripped creates served to clients carry only content, sender, state_key and type (601cd25007, 82f951eefd, feb951646a).

- Another user's `transaction_id` and `org.matrix.msc4140.delay_id` no longer appear via `/event`, `/context`, search, notifications or state; federation strips both, appservices keep their own users', and senders still see theirs, redactions included (aef72e84a8, 642b39f366, 64549e2c85).

- Debug and trace output no longer carries secrets: JWT keys, appservice and OAuth tokens, client secrets, PKCE verifiers and grant nonces print as `<redacted>`, also in `!admin query oauth show`, and SSO callback traces omit cookies, codes and tokens (765d1a3b89, 793a4b4a1a, 21e5b877cd, ccc26cfbe0).

- Knocking on a remote room works with sliding sync even without local state: the client gets the knock plus the room's name, avatar, encryption and type as `invite_state`, and re-knocking no longer puts an old leave first. Full-PDU summary entries are checked for room binding, signatures and hashes (MSC4311; 0e7f9ddb66, 18e952c72a).

- Once every local user leaves a room, federation pushes can no longer resolve against its frozen state and make a local user look joined, and a replayed leave or an older invite's rescission cannot undo a newer invitation (2139047614, 39d4a0ed22, 1bbd5013bd).

- Sync fixes: a list matching no rooms reports `count: 0` instead of vanishing, so SDKs stop polling with timeout zero; an opening request ignores `timeout`, and one deadline bounds later long polls; withdrawn federated invites disappear without local state; and legacy `/sync` includes every nonzero thread count when asked (588a060ed8, b1b8c4c9b0).

- Redacting a notifying message removes its share of room, thread and account badges before any receipt; unreadable notification rows are skipped with a log, not a panic. Thread-reply scans read at most `rooms.redaction.thread_latest_reply_search_limit` rows (default 1024, reloadable) (0177e2de8f, d6870147de).

- `GET /_synapse/admin/v2/users` and `/v3/users` list local accounts only, excluding remote profiles from pages and totals (062bf5614e). Local `publicRooms` without `limit`, or with `limit=0`, returns up to 100 rooms (78993390f2).

- `/versions` lists the OIDC entries (`org.matrix.msc3861` and five companions) only while OIDC runs, and drops the obsolete `org.matrix.msc4284` (cae4679777, 955a580353).

- The runtime admits fewer I/O events per poll, so queued work waits less behind sockets: `TOKIO_MAX_IO_EVENTS_PER_TICK` defaults to 256 (was 512) and `TOKIO_MAX_IO_EVENTS_PER_BUSY_TICK` to 64; environment overrides still win (6021d2be1f).

- Room upgrades copy the MatrixRTC slot configuration (MSC4143), `m.rtc.slot` or `org.matrix.msc4143.rtc.slot`, open or closed (df5135a9c9). `createRoom` rejects malformed initial membership up front and drops `initial_state` names and topics that top-level `name` and `topic` supersede (70828ae3ee, 04b400a4e0).

- A database opened read-only for an integrity check no longer delivers overdue delayed events or queued notification emails (91fc682d13). `!admin appservices register` replaces a database-owned appservice registration in place, without unregister or restart (6a403d2ac7).

- Concurrent writers take turns: account-data updates and erasure per user (86c6cef7e6, ca75e84fac), and key and signature uploads per key row, so concurrent signature uploads keep each other's signatures and an upload racing device removal cannot restore its keys (6f1899dcb2, 8b001e0641). Alias creation commits its three indexes together, and an unreachable remote alias server returns 502, not 500 (350f4adb49, 7d8751a1c7).

- `PUT /_matrix/client/v3/devices/{deviceId}` with `{}` keeps the display name (f50b07d5de), a profile change no longer looks like a fresh join to sync (b890115247), and an undecodable path parameter returns `M_INVALID_PARAM` (75ad674150). Threads record each participant once (716d4aa35b), storage faults surface instead of reading as missing events (17a441c9dd), and room deletion clears its attributable soft-fail markers (3ba689bf2e).

- `media.allow_legacy` docs (also `allow_legacy_media`) no longer list upload among the unauthenticated endpoints it controls; upload is unchanged (59ee29c7e9).
