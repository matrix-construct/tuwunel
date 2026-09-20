# Tuwunel 1.9.2

September 20, 2026

### New Features & Enhancements

- **New rooms are created as room version 12 by default.** A room created without an explicit `room_version` gets a hashed room ID with no server name and a creator nobody can demote or list in `power_levels.users`; existing rooms are unaffected. A `default_power_level_content_override`, or a client override, whose `users` map names the creating user now fails `createRoom`, matching Synapse; `default_room_version = "11"` restores the previous default.

- **User status and extended profile fields now arrive through sync (MSC4133, MSC4429, MSC4262)**, shipped by @x86pup after @doits raised the request in (#582). Legacy `/sync` carries an `org.matrix.msc4429.users` block for the fields selected by the client's filter. The Sliding Sync profiles extension also sends the syncing user's own profile base, so status changes reach peers on Element Web and Element X without separate profile requests for each user. Atomic snapshots and whole-user drops are not delivered yet. Over Sliding Sync, a change in a room outside the client's window waits until that room enters it.

- **The Debian package confines the server with an AppArmor profile**, graciously contributed by @x86pup. This shipped without mention in 1.9.1, so operators on 1.9.1 already have it. The profile grants no capabilities and limits writes to `/var/lib/tuwunel`, `/run/tuwunel` and `/etc/tuwunel`. A data or config directory relocated through a unit drop-in or symlink is denied until it is added to `/etc/apparmor.d/local/usr.sbin.tuwunel` and the profile is reloaded with `apparmor_parser -r`; postinst names each uncovered path. Root invocations of the execute, regenerate-config and restore-backup modes must run as `sudo -u tuwunel`. See [debian/README.md](https://github.com/matrix-construct/tuwunel/blob/v1.9.1/debian/README.md); the RPM package is unaffected.

- Containers get two hardening aids from @x86pup, carried over from 1.9.1. `docker/seccomp-io-uring.sh` derives a profile from the runtime default and re-permits only the io_uring syscalls omitted by Docker and Podman, replacing `seccomp=unconfined`. The database pool also warns at startup when its thread count would exceed the container's cgroup pids limit and explains how to fix it. See [docs/deploying/container-security.md](https://github.com/matrix-construct/tuwunel/blob/v1.9.1/docs/deploying/container-security.md).

- Allocator tuning now takes effect on musl, macOS and OpenBSD builds, where the override variable is `_RJEM_MALLOC_CONF` rather than `MALLOC_CONF`. OpenBSD also builds and runs with `jemalloc`. No GitHub release asset is affected. With appreciation to @x86pup.

- The appservice one-time-key claim and key query proxies (MSC3983, MSC3984) are removed, following an initial disable option in (#587). Tip of the hat to @Lama-Thematique for (#593). A bridge that relied on Tuwunel to forward those requests must answer them itself. A leftover `appservice_keys_claims` key produces a deprecation warning rather than a parse error.

- Media download and thumbnail responses adopt the MSC4149 Content-Security-Policy: `plugin-types` and `object-src` are removed, while `font-src`, `form-action`, and `base-uri 'none'` are added. Two opt-in options go further. Both default to `false` and require a restart: `media_deny_framing` adds `frame-ancestors 'none'`, while `media_deny_inline_styles` removes `style-src 'unsafe-inline'`.

- The admin `query feds ping` command surveys federation peers and reports the round-trip latency distribution beside each origin's peer-status record. `query feds version` gains field selection, column sorting and a result-count footer.

### Bug Fixes

- **A hand-rebuilt admin room no longer stops the server from booting.** The 1.9.1 startup guard inferred whether `server_user_localpart` had changed from the admin room's creator, so it rejected an operator-rebuilt room with a mismatch error. The first boot now records the configured localpart in the database, and later boots compare against that value. Thank you @exentio for reporting (#589).

- **Debian and RPM packages stop planting `/var/lib/conduwuit` and `/var/lib/matrix-conduit` symlinks on every install**, courtesy of @meoovv in (#595), with follow-ups by @x86pup. With these links present, `rm -rf /var/lib/conduwuit/` with a trailing slash followed the link into the live database. Fresh installs create neither link, adopted databases keep the link they came with, and upgrades remove a leftover unless `database_path` is set. Point any backup script that uses an old path at `/var/lib/tuwunel`, or set `database_path` before upgrading.

- Deleting a device now removes its uploaded identity keys too, graciously contributed by @basnijholt in (#591); the stale row outlived the device and could be served by `/keys/query` when a later login reused the device ID. Login now refuses a device ID that collides with lingering keys.

- With LDAP enabled, a passwordless SSO account is no longer offered the password stage it can never complete, so Element's confirmation dialog on deactivation or an email change stops asking for a password that does not exist. Credit to @basnijholt for (#590).

- Appservice-managed users can appear in user directory searches again through the new `show_appservice_users_in_user_directory` option, which defaults to `false`. The exclusion added in 1.8.3 ran before `show_all_local_users_in_user_directory` was consulted, causing bridge puppets and bot accounts to vanish from invite search. Shipped by @basnijholt in (#594) after MindRoom's agents disappeared on 1.9.1.

- Federation requests whose `X-Matrix` Authorization header omits `destination` are now accepted for compatibility, as the specification requires. Older Synapse peers such as 1.56.0 previously received a constant stream of 403 responses. A present but incorrect `destination` still fails, now with 401 instead of 403. With appreciation to @kybe236 for (#588).

- Live location sharing in Element X no longer sticks when the client changes what state it asks for mid-session. Sliding Sync remembers the required-state selectors last delivered for each room and sends only the delta instead of treating a configuration change as a full replay. Thank you @utop-top for reporting (#569) and its recurrence in (#596).

- Startup warns when a regex-valued list option such as `forbidden_remote_server_names` or `dns_passthru_domains` contains a plain name with unescaped dots, which matches more than it appears to. The warning prints the escaped form to paste in without rejecting the configuration. The example `deprioritize_joins_through_servers` line in `tuwunel-example.toml` also contained an invalid TOML escape and could not be uncommented as shipped. Both fixes are courtesy of @x86pup.

- Importing a Conduit or fork database now preserves the expiry of each origin-issued access token. The shared token column previously dropped the expiry, making every imported session appear non-expiring. The one-time migration runs on the next boot and leaves tokens issued after the import untouched. Credit to @x86pup.

- Test fixtures now place their databases under the platform temp directory, so `cargo test` no longer fails with a permission error when `TMPDIR` is unset or restricted. Reported by @vehlwn in (#592) while packaging for Arch Linux.

- Outbound federation now sends per-device device-list updates. Every device-key or cross-signing change previously produced an EDU with an empty `prev_id`, causing each peer to discard and fetch the user's entire device list again. Peers now receive the specific device with its real `prev_id` and `deleted` flag, plus a signing-key update when cross-signing changes. A full resync happens only above ten changed devices, and queued updates drain before fresh ones are selected (e5da06b49, c0d0528ec, 62a608553).

- Plaintext rooms no longer over-report device-list changes. Sliding Sync placed a plaintext room's entire member list in `device_lists.changed` after every state change (regression 0adec1e3a, shipped in 1.6.1), while legacy sync counted lazy-loaded members as joins and flagged every speaker on each round (regression c337ea186, shipped in 1.3.0). FluffyChat, nheko and bridges then downloaded every listed user's keys again (48769caed, 1ecd6c5d7).

- `/event_auth` chains for room version 12 were one event short. The implied create event was marked as seen before it could be yielded, so peers walking a v12 room's auth chain from this server received the rest of the chain without its create event (818e64f3d, regression 944f16520 shipped in 1.5.0).

- `/members?at=` now serves the membership snapshot at the requested token instead of ignoring `at`. A timeline beginning at the room's create event returns the window's end as `prev_batch` rather than the create event's own position. Visibility is still decided from the caller's current membership, a documented divergence from Synapse (6fe0095dd, 5895a78b9, 15201a4e0).

- The admin database flush command reported success after flushing only RocksDB's empty default column family. It now flushes every column family, so a flush-then-snapshot backup captures buffered writes (ad13deab9).

- User directory search now paginates results in user order. The concurrent visibility walk previously yielded in completion order, so identical queries could return a different set of users each time (773bb5ce2).

- A profile write that restores the value a room already holds appends no member event there, so it no longer wipes that room's membership `reason` (13804b576). Account deactivation separates cleanup from room departures and bounds concurrent leaves at eight, with a lifecycle test covering erasure across every membership state (c53d80d80, 259337054).

- Profile field publication and its discovery entry now commit in one transaction (925975ceb).

- The sending service is split into focused units, carrying four fixes: queue writes hold their counter permit until the rows land, a panicking sender shard restarts the service instead of remaining dead, the presence cap counts distinct users again, and a malformed stored receipt is dropped with a log entry instead of causing a panic (dec5bc10a, 54ebeaa96). A failed post-write flush now logs an error instead of panicking; the write itself was already durable (042769db8).

- State resolution now decodes only the fields it needs and borrows authorization state instead of cloning it for each lookup. A send-join check whose held state names an event absent from the timeline reports a storage error rather than not-found (d741a57ad, cb3ed70ed, 49a6fcb62, 6b736549a, cfb93dac4). Log string truncation no longer panics inside a multibyte character, and subsecond durations now format correctly (e373df7c3).

- Dependencies advance to sentry 0.49, argon2 0.6, rustls 0.23.45 and ipaddress 0.2. With Sentry enabled, a `sentry_traces_sample_rate` outside 0.0 to 1.0 now prevents startup with a specific configuration error. The argon2 update leaves verification of existing hashes unchanged. The Ruma pin carries the final MSC4140 delayed-events types, but the endpoints are not served in this release.

- Bug reporters are now asked whether they tested a `main` build and are pointed to the prebuilt `main` image tags, courtesy of @x86pup (0de3d08b5).
