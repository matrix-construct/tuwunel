# Tuwunel 2.1.0

November 25, 2026

> [!NOTE]
> Before upgrading, note the following:
>
> - **The identity provider callback moved to `/_tuwunel/sso/callback/<client_id>`.** If a provider has `callback_url` unset and `well_known.client` is set, register `<well_known.client>/_tuwunel/sso/callback/<client_id>` with the provider before upgrading, or pin `callback_url` to the old URL. If `well_known.client` has a path, set `callback_url` to the full new URL and register that. Otherwise sign-in fails on the provider's own page and nothing is logged here. An explicit old `callback_url` keeps working through 2.x with a startup warning. The reverse proxy must forward `/_tuwunel/`, and a `callback_url` change needs a restart, not a reload.
>
> - **Access tokens in the query string are refused (MSC4127).** Clients, bridges and scripts must send the `Authorization` header, appservice tokens included.
>
> - **Room policy servers are on by default and fail closed (MSC4284).** In a room that has one, sends fail while it cannot be reached. Rooms without one are unaffected. Set `rooms.policy_server.enable = false` to keep the old default.
>
> - **The apt repository moved to `apt.tuwunel.chat`.** `apt.f.dog` still answers but stops at 1.9.4, so `apt update` reports no error and offers no 2.x. Replace the keyring and sources file as `debian/README.md` shows.

### New Features & Enhancements

- **Sticky events (MSC4354, MSC4480)**, opened by @dennisoderwald in (#698) after Sable 26.11.0 reported them missing and fell back to room state. Element Call has a mode that keeps call membership in events that stay with the room for up to an hour, so someone who was offline past the timeline limit, or who joins the room mid-call, still sees who is in the call. A client adds `org.matrix.msc4354.sticky_duration_ms` to a message or delayed message (at most one hour, zero refused); joined members get unexpired ones whatever the timeline limit or history visibility, and a server joining the room is sent those from our users. Nothing to configure. matrix-js-sdk reads them from legacy `/sync`; the sliding sync extension ships ahead of its clients (none we checked speaks it).

- **Native LiveKit calls (MSC4195).** Tuwunel can issue LiveKit tokens itself, so an operator running LiveKit needs no lk-jwt-service for clients that ask their homeserver for one. It is off until `url`, `api_key` and `api_secret` are set under `[global.livekit]` and the server restarted; default builds carry the `livekit` feature. Only users joined to the call's room get a token, callers on other servers included; LiveKit is asked to remove anyone who leaves or is banned, and a caller's delayed leave is sent when they go. Point LiveKit's webhook at `/_tuwunel/livekit/webhook`; without it, an earlier token lets someone who left or was banned rejoin for up to an hour. [The native mode guide](https://matrix-construct.github.io/tuwunel/calls/matrix_rtc.html#native-mode) lists Element Call 0.29 and later as using it directly, 0.28.1, 0.28.2 and Element X 26.11's built-in Element Call as needing an extra `livekit` transport entry, and older versions as still needing lk-jwt-service.

- **Application service mode for lk-jwt-service (MSC4502, MSC4512)**, raised by @nisbet-hubbard in (#695). lk-jwt-service can ask Tuwunel whether a caller, local or from another server, is joined to the call's room before it issues a token; its standalone mode never checks, and its README says future versions will require this one. Clients and other homeservers reach it through Tuwunel's `rtc/livekit` paths, so it needs no public address. The registration's new scope and proxy keys are unset by default, and one added with `!admin appservices register` on an earlier release must be registered again to use them; [the guide](https://matrix-construct.github.io/tuwunel/calls/matrix_rtc.html#application-service-mode) covers setup and the transports Element Call 0.28 reads. For every appservice, federated to-device messages to a namespace ID with no account are no longer delivered, and a user token on an appservice endpoint gets 403, not 401.

- **Room policy servers (MSC4284) are enforced by default.** Where a room names one, Tuwunel asks it to sign what your users send and checks incoming events for its signature; only a room with a valid `m.room.policy` event whose `via` server has a joined user is affected. With no verdict, a local send fails (503 when the policy server is down or slow) and an incoming event is soft-failed. Leaves, kicks and bans go out without a verdict, but a refusal still blocks them. Remote invites are signed before they leave, backfilled events the policy server refuses are withheld, and ordinary client messages no longer hold the room during signing, though state changes, membership changes and redactions still do. Reloadable in `[global.rooms.policy_server]`: `enable` (now `true`), `request_timeout` (now 30 seconds, was 5), `fail_open` (new, `false`) and `advisory_room_versions` (new, `true`). `enable = false` returns to the old default, though other servers keep enforcing; `fail_open = true` lets events through when there is no verdict. See `docs/moderation.md`.

- **Passwordless device approval**, inspired by @Zafford in (#652), who still typed a password in the browser when adding a device with Element Desktop and Element X. Sign in with a password on the built-in login page, and later device approvals in that browser, such as QR login, open straight on Approve or Deny. You still approve each device, and "Use another account" signs the browser out. It needs `authentication.oidc.native_auth` (default `false`), skips only that password step, and lasts `authentication.oidc.browser_session_ttl` (default 30 days, reloadable, 0 turns it off); a password change, deactivation or signing out of all devices ends it sooner. Accounts using an identity provider are unaffected.

- MatrixRTC transport discovery (MSC4143) is served at the stable `GET /_matrix/client/v1/rtc/transports`, beside the unstable path; tip of the hat to @rexbron for asking in (#697). Both paths now need an access token (401 `M_MISSING_TOKEN` without one). A `well_known.rtc_transports` entry without a string `type` of 1 to 255 characters from `[a-z0-9._-]`, starting with a lowercase letter, now stops startup and is refused on reload; `livekit` and `m.livekit` pass.

- The account page's Active Sessions list sorts by name, device ID, IP or last seen, and "Sign out selected" ends several sessions in one step without a new sign-in, opened by @rexbron in (#614). Confirm within `login_token_ttl` (two minutes by default) of signing in to the page.

- `!admin users clear-one-time-keys [username]` empties the one-time-key pool after a database restore, courtesy of @obodnikov in (#694). A restored backup brings back keys already handed out, and a session opened with one cannot be decrypted. Devices stay signed in and upload fresh keys at their next sync; run it before the restored server takes clients (see `docs/backups.md`).

- A bridge can list a room's members through `GET /rooms/{roomId}/joined_members` once its sender or any user in its namespaces is joined, instead of having to act as a joined user. Shipped by @AnonAlly in (#696).

- Sliding sync does less work per request on connections with many required-state keys or overlapping list ranges. Credit to @basnijholt for (#714) and (#715).

- FreeBSD operators can `pkg install tuwunel` (port `net-im/tuwunel`, `aarch64` and `amd64`), which our docs now name; with appreciation to @nkfilis for (#690). The port's version can trail ours.

- The apt repository is now `https://apt.tuwunel.chat`, under a new signing key. `apt.f.dog` still answers but stops at 1.9.4, so a machine pointed at it sees no error and no 2.x. Replace the keyring and the sources file as [debian/README.md](https://github.com/matrix-construct/tuwunel/blob/v2.1.0/debian/README.md#moving-from-aptfdog) shows under "Moving from apt.f.dog". Packages since 1.9.4 need glibc 2.39 or newer (Debian 13, Ubuntu 24.04); apt offers Debian 12 and Ubuntu 22.04 at most 1.9.3, so use the container image or a static binary there. Thank you @ruka-hamanasu for clearing out the stale instructions (#699).

- The identity provider callback moves to `/_tuwunel/sso/callback/<client_id>` (MSC2324). If a provider has `callback_url` unset and `well_known.client` is set, Tuwunel now sends it `<well_known.client>/_tuwunel/sso/callback/<client_id>`; nothing is logged, and sign-in fails on the provider's own page until that URL is registered there. Before upgrading, register it with the provider, or pin `callback_url` to the old `/_matrix/client/unstable/login/sso/callback/<client_id>` URL. If `well_known.client` has a path, set `callback_url` to the full new URL and register that. An explicit old URL keeps working through 2.x with a startup warning, and so does leaving both settings unset. The reverse proxy must forward `/_tuwunel/`, and a `callback_url` change needs a restart, not a reload (6e7510b88c).

- Access tokens are read only from the `Authorization` header (MSC4127). A token in the query string is refused on client requests, appservice tokens included: `GET /_matrix/client/v3/account/whoami?access_token=...` now gets 401 `M_MISSING_TOKEN`. Clients, bridges and scripts must send the header (552fd81042).

- The stable `GET /_matrix/client/v1/auth_issuer` is gone (MSC2965) and answers 404 `M_UNRECOGNIZED`; use `/_matrix/client/v1/auth_metadata`. The unstable path is still served (6bc306ff88).

- Dependencies advance to ruma 0.18.0, sentry-tracing 0.49.5, lettre 0.11.24, url 2.5.9 and cookie 0.18.3. The unstable mutual rooms answer (MSC2666) no longer carries `count`; the stable one keeps it (de1008cfc9, 9d82a10661).

- Release artifacts, the book and the Nix flake build on current stable Rust; the minimum supported version is unchanged at 1.95.0 (42011fb05e, 6a3a487a82).

- Support links now lead to `#tuwunel:tuwunel.love` (8264bc46ea, 71270ebc75).

### Bug Fixes

- **The server links on macOS again.** Building 2.0.0 there stopped in Apple's linker with `Assertion failed: (name.size() <= maxLength)`, over a symbol name of 1,524,753 bytes against a 1 MiB limit; 1.9.4 linked. Only source builds on macOS were affected. Graciously contributed by @obodnikov in (#692). Sincere apologies to everyone whose build stopped there.

- **Invites sent as room state to users on other servers now reach them.** `PUT /rooms/{roomId}/state/m.room.member/{userId}` with `membership: invite`, the shape bridges use, answered 200 and kept the invite in local history while the invitee saw nothing. `@antoinek:mpi-sws.org` reported it with mautrix-slack; thank you! Such invites, delayed ones (MSC4140) included, now go through the federation invite handshake: if the other server is unreachable or refuses, nothing is stored: an ordinary send gets an error, so the bridge must retry, and a delayed one records the failure on the delayed event, giving up after 30 seconds. On this route, `/invite` and `createRoom` alike, the invitee's server must countersign with a key it publishes. In a room whose federation an admin disabled, a local user's remote invite is now stored once the other server accepts, instead of failing afterwards. Invites stored before the upgrade are not sent again (12f4484eb8, 7343de1055).

- **Sync timelines follow the room's history visibility.** Legacy `/sync` and sliding sync judge each event by the rule `/messages` uses, so a room with `joined` visibility starts its timeline at your join. Where a timeline stops early, scrolling back still reaches the older events you may see. Proposed by @basnijholt in (#728), with thanks (7a2f1b8a53, 718926f3b2).

- @leonitousconforti reported in (#739) that requests, SSO callbacks among them, returned 500 with `send failed SendError(..)` depending on which CPU ran them, on a Proxmox LXC pinned to CPUs 4 and 9 over Ceph RBD, and proposed both parts of the fix. Thank you! A server pinned to a subset of CPUs, on a block device whose queue IDs run past the highest allowed CPU, now gives each of them a working queue (986f447cdd).

- Thanks to @ld-873233, who reported in (#737) that submitting a typed device code did nothing in Chromium when an external identity provider signs users in (`native_auth` off). The page's `form-action 'self'` stopped the redirect to the provider; a "Continue signing in." page now carries the browser there (fe2ffeb0e3).

- Identity provider sign-up skips a username that an appservice reserves, shipped by @basnijholt in (#721); `registration.forbidden_usernames` now matches the localpart, and IDs naming another server are skipped (7d5d21c1be). A provider with `trusted = true` therefore no longer signs anyone in to an existing account with such a name: sign-in moves on to the next claim or a derived name, or is refused when `unique_id_fallbacks` or `registration` is off. Accounts already linked are unchanged.

- Thank you @basnijholt for (#702): with several identity providers, re-authentication goes to one you finished signing in with. In 2.0.0 a chained sign-in's pending session could send the prompt, for deleting a device or resetting cross-signing, to a provider you never used.

- Server notice rooms are recognized by `m.server_notice` alone and created with that tag, courtesy of @dasha-uwu (b07fd6d521, 2a0ae4be1c). With a custom `admin.room.tag`, notice rooms made since 1.8.2 are no longer recognized and the next notice opens a new room; to keep an old one, run `!admin users put-room-tag <user_id> <room_id> m.server_notice` for its recipient.

- @rexbron reported that joining a bare room version 12 ID answered 500 over "a room with no forward extremities". A room unknown here, with no server to ask, now gets 404 `M_NOT_FOUND`, and a room that lost its forward extremities says to join through a server in the room. Thank you! (f11a0795a9, c7dbaeda67)

- Tip of the hat to @pkubaj for (#738): larger FreeBSD hosts no longer stop at startup with `Resource temporarily unavailable (os error 35)`. The thread limit is read from the `kern.threads.max_threads_per_proc` sysctl (1500 by default) instead of `RLIMIT_NPROC`, which counts processes there.

- After declining an invite or withdrawing a knock, the left-room state in legacy `/sync` carries only your own membership event unless the room is world-readable; if you had joined, you still get the room state. Graciously contributed by @basnijholt in (#729, d77ce8c054).

- A remote join keeps only the state the room's own rules allow, credit to @basnijholt for (#723): each event in a `send_join` answer is checked against those already admitted from it, and a create event that fails or names another room version fails the join. A federated event must refer to the room's known create event, and a local user's join must come from that user (#727); state fetched by ID or rebuilt locally must belong to the room (#726).

- `GET /_synapse/admin/v2/users` and `/v3/users` ignore case in the `name` and `user_id` filters, so `name=ALICE` finds `@alice:example.org`; with appreciation to @dasha-uwu (251250f659).

- **Thread summaries and relations show each person what they may see.** In clients that read the bundled summary, such as Element Web and Element X, someone you ignore no longer appears as a thread's latest reply or in its count, nor does a latest reply hidden from you. The new `rooms.relations.thread_summary_search_limit` (default 1024, reloadable, zero refused at startup) caps the rows read to rebuild one. The latest reply names its room, and its sender gets it with their `transaction_id`. `/relations` hides ignored senders' and redacted children, judges the parent event too, and answers every refusal with 404. Federation sends no unsigned data, and state, notification and appservice responses no stored summary (3451270536, 0dbe242381, 4121ace12b, 563c670b52, a602a05b81, 7752984d26, 609143eb31).

- `rooms.invites.block_non_admin` (default `false`) covers invites sent as room state, as it does `/invite` and `createRoom`. With it on, bridges and other non-admins get 403 `M_FORBIDDEN` for them (d7a7985a44).

- A soft-failed state event that state resolution keeps now reaches room state and clients once a later message builds on it. Scrolling back no longer stops at a withheld event, and a server whose backfill request references a soft-failed event gets it (0de9dee62a, 8bd40b6638, e1e7ea2008, ed867531d3).

- When a policy server rate-limits us and names a retry time, every request to that server waits it out, at most a day, and unrelated traffic from it no longer ends the wait. The Synapse-compatible `reset_connection` admin endpoint clears it early (26fffd5dcc, 6c472118db).

- A request refused before it is sent (an address in `ip_range_denylist`, or this server itself) or cut off by shutdown no longer counts as the peer's failure. A destination behind a denied address stays queued and is rechecked every `sending.timeout`, 180 seconds by default (90bf042553).

- One stored event that cannot be read or redacted no longer fails a whole backfill, missing-events or event-auth answer; it is logged and left out (94c6b61f74).

- Remote joins and knocks keep the events this server signed, which were dropped unless a notary had once returned our own key (c798ac4c89). A notary's keys are taken only from documents naming the origin server (2a7fa23606).

- Withdrawing a knock on a room this server is not in asks the room's servers, so the knock leaves its member list instead of clearing only here (29aeb3beaf).

- Someone a history visibility change opens a room to sees the change itself, for example when a room goes from `joined` to `world_readable` (e067651c15).

- `/relations` with `recurse=true` stays within three levels, stops at events the filters exclude and pages in order (cf063b67b8). A state event or a relation's child no longer roots a thread (MSC3440): a thread reply sent here to an event with a custom or unreadable `rel_type` gets 400 `M_UNKNOWN`, and a child's replies count once it is redacted (eca7ad06cc, 6a609d2547).

- Concurrent changes to a room's tags or to `m.direct` by auto-accepted invites no longer drop one another, and a tag write that cannot read the stored tags fails instead of replacing them (3f9620d6a6).

- `POST /_synapse/admin/v1/rooms/{room_id_or_alias}/make_room_admin` no longer hangs for a user neither in nor invited to a private room; it invites them, then grants power (af6597afca).

- Searching for a word that grows when lowercased, such as 25 dotted capital I characters, no longer overflows a fixed buffer in the query; 1.9.4 has this too (db49225a93).

- With `profile.enable_set_displayname = false` (default `true`), the account page shows the display name read-only and refuses the form, except for admins (4106e79174). Braces in a display name appear there as typed (1df6f93741).

- Invite routing hints are read by exact room ID, and an empty row no longer panics (facb261dc0). A remote server's `unsigned` data is dropped from join, knock and leave templates (d4a8aae0c9). Fetches still queued at shutdown are cancelled (10268d2ec2).

- The TURN guide now says that a `service` or `ttl` parameter already in `turn.endpoint.url` is replaced by Tuwunel's own, as in 2.0.0 (e9df7b3cda).

- @basnijholt landed several more fixes: an event ID repeated in a peer's `/state_ids` answer no longer rejects consistent state (#701); a replayed `send_join` is answered only while its user is still joined (#718); a restricted join refused here stays refused when the remote fallback names one of our own users as authoriser (#719); a refresh token works only for refreshing and revoking, not as the bearer token of a request such as `/sync` (#720); and tests cover the room identity of create events (#700) and steady a timeline probe (#703).
