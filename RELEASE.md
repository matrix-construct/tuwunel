# Tuwunel 1.9.0

August 18, 2026

### New Features & Enhancements

- **URL previews are richer and more reliable**, shipped by @x86pup. The fetch budget rises from 256,000 to 786,432 bytes, each request gets its own `User-Agent`, and oEmbed recovers pages that otherwise yield nothing. YouTube links now show their title, channel and thumbnail instead of a bare hostname. Previews remain disabled until at least one `url_preview_*_allowlist` option is non-empty. See `docs/media/url-previews.md`.

- **TLS now uses aws-lc-rs and the platform trust store**, courtesy of @dasha-uwu. `ring` leaves the build, while SMTP, LDAPS, S3 media, client and federation pools share one crypto provider. `allow_invalid_tls_certificates` now covers LDAP TLS too. Operators running the published standalone binary in a chroot, distroless image or hand-rolled `FROM scratch` image must install a CA bundle or set `SSL_CERT_FILE` before upgrading. Unlike 1.8.3, version 1.9.0 will not start without one. The container image and both packages already include a bundle.

- **Push rules can now match related events (MSC3664)**, graciously contributed by @x86pup and raised by @random-mcrafter in (#544). The default `.im.nheko.msc3664.reply` rule is added to every account on upgrade regardless of configuration, so nheko users get reply notifications immediately through client-side evaluation. Server-side evaluation for notification counts and pusher delivery requires `msc3664_related_event_match`, which defaults to `false`. The default covers replies; `.m.rule.reaction` still suppresses reactions, but custom rules can now match `m.annotation`.

- **Tuwunel can now rewrite your configuration file.** The new command-line regeneration mode and `!admin server regenerate-config` render the example document from the live schema with your values, canonical option names, and unknown keys preserved under `# DEPRECATED`, `# UNKNOWN` or `# UNDOCUMENTED`. Regenerate and diff to review the result. Nothing is applied or reloaded, and the default output is a new file beside the original. A separate mode emits a clean example. See `docs/configuration/regeneration.md`.

- Migrating a conduwuit-lineage database now preserves account state, with appreciation to @x86pup. On the next boot, passwordless accounts no longer appear deactivated, deactivated accounts remain inactive with their old passwords disabled, and email addresses keep their owners.

- `!admin query oauth adopt <provider_id>` converts a migrated conduwuit-lineage database's OIDC subject map into durable identity associations in one pass. Without it, each migrated user's next login creates a fresh account with a generated localpart.

- Thanks to @x86pup, **FreeBSD and OpenBSD arm64 build without custom `CXXFLAGS`**, while NetBSD arm64 selects hardware CRC32C at runtime. New guides cover FreeBSD, NetBSD, OpenBSD, RISC-V 64 and 32-bit ARM, each with a verified build and its remaining limits: OpenBSD needs Rust from `-current`, ARMv6 lacks a working C++ runtime here, and none of the five is covered by CI or ships an artifact.

- Nine cache-capacity options that previously read "This item is undocumented" now explain that their values are entry counts, not byte sizes, graciously contributed by @byteflavour in (#541).

- Startup now warns when write-ahead log preallocation is enabled on btrfs or ZFS, where it reserves much more disk than the logs use. Raised by @byteflavour in (#535). `rocksdb_allow_fallocate` still defaults to `true`; the server warns without changing the engine setting.

- Thanks to @x86pup, each account's push ruleset is capped at 10,000 rules, with each serialized rule capped at 1024 bytes. The limits apply only when the ruleset grows; deletes and disables remain unconditional.

- `force_migration` is now hidden from generated configuration. If it is still set at boot, Tuwunel warns and waits fifteen seconds. Tip of the hat to @x86pup.

- With appreciation to @x86pup, the `.deb` and `.rpm` install checks now resolve their dependencies, and the `.deb` check confirms that the programs invoked by the unit exist.

- Credit to @dasha-uwu for reworking registration-token handling and correcting two admin API responses: deleting a config-file token now returns 403 instead of 500, and updating one returns 403 instead of 404.

- RocksDB advances to 11.8.1 and jemalloc to 5.3.1-2. Eleven low-traffic cache columns now share a pool, while five hot columns retain dedicated pools sized by four new options. Failed write-ahead log flushes and syncs are now logged.

- Fifteen subsystems moved their multi-row writes into single database transactions, so a related group of rows is either all visible or none.

- One option added in 1.8.3 went undocumented. `refresh_token_reuse_grace` controls how long a rotated refresh token remains valid; setting it to 0 treats any reuse as a compromise. The option is not new, and its behavior is unchanged in this release.

- The Matrix Authentication Service guide now distinguishes MAS as a login provider from MAS as a provisioner and lists all twelve provisioning routes.

- The reverse-proxy guide now correctly explains that the `/_synapse/admin/v1/register` pair uses an HMAC over `registration_shared_secret`, not an admin token. Restrict this path to trusted networks.

- Internal Rust API documentation now covers the public surfaces of `tuwunel-core` and the database crate. Complement gains shared-secret registration and an advanced fork pin. Dependencies also advance, including `h2` 0.4.16 for RUSTSEC-2026-0258.

### Bug Fixes

- LDAP logins broken by 1.8.3 work again. That release rejected binds for existing local accounts without LDAP origin, locking out deployments that added LDAP after creating accounts. The gate is reverted (bf1ba04eb), with only its deactivation check restored (b1eee0313). Before upgrading, confirm that `[global.ldap] base_dn` and `filter`, or the `bind_dn` subtree in direct-bind mode when no search runs, resolve only principals allowed to log in as that localpart. If `admin_filter` is set, a directory entry can now promote an existing local account to admin. Use `!admin query users search-ldap` on 1.8.3 to check. Sincere apologies to everyone locked out by 1.8.3.

- @dasha-uwu removed the LDAP `name_attribute` option and stopped directory searches by `givenName`; an entry must put the Matrix localpart in `uid_attribute` to allow login. A leftover `[global.ldap] name_attribute` produces no boot warning. Regenerate and diff the configuration to reveal it as `# DEPRECATED`, then remove it manually.

- Push notifications reach gateways again, and failures are now retried. Rejected pushes were counted as delivered and deleted, leaving nothing to retry and permanently sidelining the pushkey. Tuwunel now deletes only accepted events, arms the destination's retry timer, and logs the user, pushkey and error chain on failure. Reported by @xcysy32 in (#543), which remains open pending confirmation. Thanks also to @NekoCWD for separating the proxy case into (#554).

- Three related push defects are also fixed: rules combining `notify` with historical actions the spec says to ignore are accepted; gateway path stripping now matches only at the URL's end; and pushes during shutdown no longer panic.

- Thanks to @haydonryan for reporting the stale iOS unread badge in (#538) and fixing it in (#539). Read advances refreshed pushers only when the stored count cleared from nonzero, so a badge left nonzero after server counts reached zero never received the explicit zero. Refresh is now unconditional, bounded by each pusher's record of the last count the gateway accepted. Leaving or being banned from an unread room updates it too.

- A one-time migration repairs two kinds of latent database damage on the first 1.9.0 boot. Releases before 1.8.3 could assign two short ids to one identity, leaving state entries that later removal could not cancel. Every release through 1.8.3 also cached auth chains truncated when an ancestor was missing, a normal backfill gap. Those chains are cleared once, and incomplete walks are now marked. A narrower cross-room case is a genuine regression first shipped in 1.5.0 (944f16520).

- Expect a longer first boot before the listener opens while a clean database performs four full column scans once. The duration is unknown; this was found internally and not reported by a user.

- A federation disclosure is fixed. `/state`, `/state_ids` and `/event_auth` looked up an event by id alone, allowing a server that held any event id from a room it had never been in, and could pass the access check on any other room here, to read the first room's state through the second room's URL. Tuwunel now validates the stored event's room, and wrong-room responses are byte-identical to missing-event responses. The issue dates to the server ACL implementation in early 2022.

- `timestamp_to_event` previously ran only an ACL check, letting any server we federate with obtain a real event id from a room it had not joined and satisfying the disclosure's precondition. It now runs the shared access check.

- Federated events that authorize at their own position but not against current room state are now soft-failed and re-examined on a widening schedule, as the spec requires, instead of being permanently rejected.

- Remote joins and inbound transactions acquired the room-state and federation mutexes in opposite orders, so joining an active room could freeze it until timeout. The order is now consistent everywhere.

- The `get_missing_events` walk now has its own bound, preventing a large response limit from forcing unbounded timeline lookups. Ingest also no longer panics when a signed event's `auth_events` names a non-state event we already hold; the caught panic previously returned 500.

- Element X can again preview or open an unjoined room, fixing the retry-dialog loop reported by @utop-top in (#470). Room v12 ids lack a server name, so an empty hierarchy-root `via` list prevented federation. It is now seeded from servers recorded on invites. A room with no local invite record and no usable client-supplied `via` still cannot be previewed.

- @dasha-uwu changed remote space-summary fan-out to wait for the first successful response instead of the first completed one.

- Thanks to @schlessera for reporting in (#552) that `POST /keys/signatures/upload` returned `200` with an empty `failures` map for valid, forged and malformed signatures alike, while storing unverified signatures from arbitrary relationships in other users' keys. Signatures are now verified against the signer's stored public key, only the three relationships defined by the spec are accepted, and every rejection returns a real errcode.

- @basnijholt fixed (#537) a sliding-sync room returning to the window after a new message with `initial: true` but no `num_live`, causing clients to classify the whole timeline, including that message, as history. MindRoom exposed the bug when agents went silent in idle rooms; both matrix-nio and matrix-js-sdk read the field.

- Typing indicators could remain active forever in sliding-sync clients such as Element X, diagnosed by @lhjt in (#519). Explicit stops were filtered out as empty lists, while expiry sweeps ran only during classic `/sync`. Both paths are fixed.

- Servers behind a forward proxy can reach push gateways again, reported by @NekoCWD in (#554). The denylist rejected the proxy's private address before requests left the process, leaving no proxy log. Proxy endpoints named in configuration or the environment are now exempt, with a guard rejecting any destination that merely names the proxy host. Direct requests and locally resolving `socks4` and `socks5` proxies still filter destinations.

- Thanks to @furfy for reporting in (#542) that `/forget` rejected rooms with no membership event, preventing users from removing admin-pruned rooms from their archive. It now refuses only senders who are still joined, knocked or invited.

- @obodnikov repaired non-unix builds again by fixing the journald Unix datagram socket (#547), sysfs storage-device discovery (#548), and a platform-neutral file symlink in media migrations (#549). All three surfaced while building 1.8.3 for Windows; CI still covers no non-unix target.

- Media downloads whose `Content-Type` differed only by case missed all framing and content-security headers, while a type merely mentioning HTML in a parameter wrongly received them. Headers now depend only on the trimmed, case-folded media type. Credit to @x86pup.

- Locked-account rejections now return the spec-required HTTP 401 instead of 400, thanks to @x86pup. Separately, remote errors are no longer relayed verbatim: only errors describing the remote room pass through, a remote 401 becomes 400, and all others become 502 with the origin named.

- Admin commands prefixed by a space matched neither the command prefix nor the escape check, so they vanished without a reply or log entry. Fixed by @x86pup.

- A successful `media delete` no longer reports failure or aborts startup command lists. The command had returned confirmation through the error path, so success appeared as failure and stopped boot. Also fixed by @x86pup.

- Thanks to @isosphere for reporting in (#540) that `media delete-range` rejected its direction at runtime using flag spellings the parser did not accept. The constraint now lives in the parser and appears in the usage line.

- @x86pup raised the report-reason limit from 750 to 2000 bytes.

- Logged-out email password resets can complete again. The final step was rejected with `M_MISSING_TOKEN` before reaching the route because logged-out users have no access token. Element Web then showed "make sure you clicked the link in the email," although they had already done so.

- Validated registration email proofs are now single-use, so one verification email creates one account. Reuse required `smtp.connection_uri` plus either `require_email_for_registration` or `require_email_for_token_registration`; all three default to off.

- A UIAA short-circuit is removed (c564d9e23). With the passwordless sentinel and an OAuth session row, any `m.login.password` dictionary could satisfy the entire interactive-auth challenge, even for flows with no password stage. This covered password changes, device deletion, deactivation and cross-signing uploads, and required a valid access token for the account.

- Under `shared` history visibility, checks used current membership, hiding all history from users who had left, including their own messages from before departure. Former members can now see history through their latest leave event.

- `!admin server show-config` now masks the OIDC registration access token. Because `federate_admin_room` defaults to `true`, the cleartext bearer credential may have reached remote servers whose admins were invited to the admin room. Rotate or unset it, and redact the old message.

- Setting that token blocks next-generation auth for every ordinary client, including Element X, because Matrix clients do not send an RFC 7591 initial access token. Startup now warns about it.

- Sliding-sync rooms whose payload failed to build no longer advance the cursor and disappear permanently from the connection. Only delivered rooms advance now; each room's payload, receipts, private read marker and account data commit together or not at all.

- Rejected federated invites can now be dismissed. Sending the leave retraction previously required resolved room state, so every request failed forever; it now depends only on membership.

- A user's own private read marker, room account data and notification-read state no longer affect sliding-sync room ranking or displace another room from the list window.

- An identical `PUT` to `/state` now returns the existing event id instead of appending a duplicate, so bots that reassert room state on restart stop creating copies. The guard deliberately applies only to joined senders because it returns before authorization; otherwise, a departed author could confirm the current value of any state key they once authored. That matters only with `joined` or `invited` history visibility.

- Room push rules disabled by the user no longer reactivate in successor rooms after an upgrade, and upgraded rooms no longer become quieter than the user's setting (55a09e63f).

- Positioning a push rule after the last rule of its kind, or first in an empty kind, no longer panics or returns 500. Any authenticated local user could trigger this through `PUT /_matrix/client/v3/pushrules/{scope}/{kind}/{ruleId}` with `after`; fixed in the ruma pin (2de190c11).

- Conduwuit media imports no longer treat destination storage failures as success (0282290c9). Previously, the import counted the failure as skipped and wrote its completion marker, permanently losing the media even though the source rows remained in the same database. This fix prevents future loss but does not repair imports already run on 1.8.0 through 1.8.3, where the marker is already set.

- A panicking database write no longer poisons the sequence-counter lock and causes every `/sync` and new event to panic until restart (92c81ea87, b8e2f5753).

- The appservice registry's spawned-worker load no longer races registration. Before the load finished, a new registration could be reread by its scan, rejected as a duplicate id, and shut down the server (d5207d4d1).

- Invalid client `since` tokens on purely local `/publicRooms` queries no longer return a 502 blaming an upstream server that was never contacted. The failure wrapper now covers only the federated path (760dbd3ac, regression 9a879776c shipped in v1.7.1).

- Dynamic client registration now rejects dotless native redirect schemes (cd2c6bde8).

- `/search` now limits context to 20 events on either side of each result instead of accepting the client's value without a bound (44632fb44).

- With `bundle_edit_relations` enabled, bundled edits no longer expose the sending client's transaction id to other users (c11a2c213). The always-on thread `latest_event` embed still does and is unchanged.

- The container image now includes `procps`, providing the `kill` binary required by the documented container reload (276b5358f).
