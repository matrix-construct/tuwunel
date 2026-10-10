# Tuwunel 1.9.4

October 10, 2026

### New Features & Enhancements

- **Google Cloud Storage is now a supported media provider**, graciously contributed by @asafarian in (#644). On GCE or GKE, metadata-server credentials replace the exported key the S3 interoperability route needed. Set `bucket` (prefix in `base_path`, not a `gs://` URL) under `[global.storage_provider.<ID>.gcs]` and list that ID in `media_storage_providers` unless it is `media`. The startup check needs `storage.objects.list`, aborts startup on failure and proves no write or delete rights; signing without a private key also needs the IAM API and `iam.serviceAccounts.signBlob`.

- **Native OIDC adds provider choice**, thanks to @tototomate123 (#615) and @utop-top's request (#570). Authorization, account and device-login pages offer local passwords or configured SSO with responsive, light/dark cards; @utop-top reported in (#639) that Element X's Manage Account skipped straight to SSO. Device approval stays bound to the signing-in provider. Requires `oidc_native_auth = true` (default `false`) and `well_known.client`. Account management excludes registration; registration policy still applies (da50af461, 2607b7cde).

- **Servers gain profile-read, display-name and room-alias controls** courtesy of @arnolicious (#608), (#609), (#610). Reloadable `limit_profile_requests_to_users_who_share_rooms` (default `false`) requires `require_auth_for_profile_requests = true`; unrelated users get 403. Reloadable `enable_set_displayname` and `allow_room_alias_creation` default `true`; documented admin/appservice exemptions remain. @arnolicious's (#607) is partial: avatar controls and federation profile reads remain unchanged.

- Password limits arrive thanks to @obodnikov (#597). Reloadable `global.rate_limiting.login.account` counts verified sign-ins (default burst five, refill 0.003 tokens per second); `global.rate_limiting.login.failed` counts wrong passwords (default burst three, refill 0.17 tokens per second). Zero rate/burst disables each. Password login and native sign-in share both; UIAA uses the failed-password limit only. Exhaustion returns 429 `M_LIMIT_EXCEEDED`. LDAP/SSO/JWT/token login remain excluded.

- Thanks to @x86pup for refreshing dependencies and advancing OpenTelemetry to 0.33 and tracing-opentelemetry to 0.34 (e9eca62b0, 659bf3efe); yoke-derive also advances to 0.8.4 (bdcba0b48).

- `!admin users redact-recent <user> <room> <count>` redacts a local user's newest messages in a room as that user; the count covers unredacted plaintext and encrypted messages, not rows scanned. Partial failure reports progress without rollback (653ac3ef3).

- Inspect predecessor fetching with `!admin debug prev-walk-metrics`, `backoff-metrics`, `prev-walk-rooms` and `!admin federation incoming-federation`: fetch/upgrade timing, in-flight work, room history and locked rooms without walks. Stock builds warn on capped, failed or cancelled endings; admin diagnostics retain successful walks (f64de3d0a, 82aa52a1b, 91358e256).

- Presence scans seek the requested counter window instead of the entire column. Federation encoding skips avatar/display-name loading; presence payloads gain a dedicated cache (a196906a9, 07761c573).

- Database helpers gain indexed column handles and four-byte integer decoding. RocksDB column options follow table-factory installation; capacity-derived caches round up to MiB. Identical state hashes skip snapshot loading (6ed002c8f, d81fe55f9, 54fccbb28, 8f12fad7b, bdbb04953).

- Unset `servername_status_cache_capacity` now defaults to 200,000 plus 15,000 per available CPU (was 100,000 plus 10,000), counting entries, not bytes; no memory or speed effect is measured (a1e5cd2ca).

### Bug Fixes

- Thread redactions update counts and latest replies, thanks to @basnijholt (#617), (#618), (#619). MindRoom Chat repeatedly fetched 13 pages for roughly 1,000 events. Root redactions preserve bundles; root receipts leave replies unread. One-time startup reconciliation has unknown duration, depending on stored roots/relations. Backfilled counts remain where replies are absent. Failed roots log errors yet completion is recorded. Latest-reply replacement scans at most `thread_latest_reply_search_limit` relations (default 1024, reloadable); a miss or failed read drops the stale preview and omits that summary from served bundles. The startup pass ignores the limit (20feec81f, 281becc72).

- Startup repairs short-id residue behind (#586)'s refusal. Thank you @dogarrowtype and @mav96 for the refusal reports. Repair reconciles identities, reconstructs orphan states, cleans proven purge residue, preserves uncertain data and verifies more. Fresh databases skip scans; settled ones check identities once. Attempts lacking recognized durable outcomes can retry (0341e0f30, a47f17093, 3d2abb9eb).

- Federated key queries and claims keep only the answering server's users, shipped by @basnijholt in (#624); another server can no longer overwrite device, cross-signing or one-time keys of local or third-server users. Stored keys are not revisited (113a83d5b).

- Thanks to @basnijholt, knocking stops opening rooms early: Sliding Sync withholds invited and knocked rooms' timelines (#625), knock-response member events stay out of room state (#632), and a pending knock no longer grants federation read access (#634). Knock bump stamps come from the membership event (607b1040f).

- Deactivating the last active local admin refuses before removing devices or clearing the password, courtesy of @obodnikov in (#604). Users must appoint another admin; passwordless or deactivated admin-room members cannot qualify.

- JWT login and interactive authentication reject deactivated accounts, thanks to @obodnikov for (#611). Locked-account refusals cover refresh and affected OIDC sign-in/approval/token paths. Deactivation returns 403 `M_USER_DEACTIVATED`; locks return 401 `M_USER_LOCKED` with soft logout. Appservice passwordless-puppet exceptions remain (eb655bc57, e85f4dc30, 45800bd79).

- @basnijholt made email password resets refuse deactivated accounts at the final step in (#628), rather than setting a fresh password. Password writes, creation and deactivation share a per-user lock (7f8b85e59).

- Removing/emptying `emergency_password` clears server-user sessions and password next startup, as documented. Credit to @obodnikov for (#612). Cleanup includes LDAP, clearing the password last for interruption recovery. Admin user APIs refuse server-user access changes but allow profile edits (fa3eefe45).

- JWT `B64HMAC` works, graciously fixed by @obodnikov in (#613); legacy `HMACB64` remains accepted case-insensitively. Enabled configurations reject unknown formats at startup/reload, before login. Accepted: `HMAC`, `B64HMAC`, `HMACB64`, `ECDSA`, `EDDSA`. Validation checks format names, not keys (95f973365).

- Admin-banned rooms refuse non-admin member-state joins, knocks and invites, contributed by @basnijholt in (#627). Joins recheck the ban under the join locks, and leaving a banned local room appends a real leave event (f40de1537).

- Tip of the hat to @basnijholt for (#636): `ip_range_denylist` now matches IPv4-mapped IPv6, so `[::ffff:10.1.2.3]` fails `10.0.0.0/8`, and the default adds `0.0.0.0/8` and `::/128`; explicit lists are not augmented. Literal federation destinations get the same check (b8234c9d9).

- With thanks to @basnijholt, federation rejects create events contradicting their room, including room version 12's derived ID (73e24ae0d), and stored predecessor events from another room, walking predecessors at the room's first timestamp too (324520029, fd13f974d). Loaded predecessor state must match the incoming room, and `send_join` auth-chain events pass the same room check before storage, skipping invalid ones (94198c340, d3db90b1d).

- @az4521 taught URL previews to fetch media from the page's final redirected URL, rechecked against host policy, in (#623), fixing kkinstagram.com and kkclip.com embeds whose crawler redirect reaches an Instagram CDN `.mp4` while other agents get a landing page.

- Thank you @az4521 for reporting missing-`room_id` in (#616). Admin `force-set-room-state-from-server` handles room-version-12 create events omitting it; state, auth-chain and incoming knocks normalize before storage (0428ba712, 0a707a38b, 1c6dd680f).

- Mistyped `!admin users reset-password` reports absent local accounts instead of creating state. Thanks to @morteng for (#599). Other user-writing commands/admin grants reject absent accounts. `!admin users revoke-admin` clears accountless staged grants but refuses room-command self-revocation and server-user revocation (10fc18ff5, 0a6eef544, 3492880f1).

- Credit to @basnijholt for (#635): failed appservice requests no longer log the request URL carrying the `hs_token`; the appservice and its registered URL are still named.

- Pending interactive-authentication sessions retain at most 1,024 request bodies of up to 4,096 bytes each, shipped by @basnijholt (fcc07b1bd), with keys counted (0b43956f5); evicted or oversized ones must be resent in full. A session-only cross-signing retry without retained keys gets `M_MISSING_PARAM` asking for the full body (cdde2f41c).

- With appreciation to @basnijholt for formatting and closure-borrow changes for October 3 nightly Clippy compatibility in (#621) and a steadier auto-accept-invites test in (#630), and to @SatvikMishra08 for fixing `federate_created_rooms` documentation in (#643), reported by @edenworky in (#642).

- After four consecutive federation failures, a content-related error triggers transaction splitting by room. A failing room can be parked after another succeeds, with backoff from one hour to 24 hours, doubling each time. Connection failures and rate limits do not trigger splitting. Admin queries expose parked rooms (cffd2951f, bc0f9a06a, a60622de8, b49208277, 9af7242b9, c449ad24a).

- Signed federation requests stop following HTTP redirects, so peers must serve them at the selected destination, and legacy media downloads and thumbnails ask remote origins for bytes. URL preview fetches and local object-store redirects are unaffected (021841f4c, 344d2c5f1).

- SSO sign-in asks before sending its login token to an unlisted destination: anything outside the configured client origin and `oidc_registration_allowed_redirect_hosts` (which accepts private-use schemes) gets a Continue sign-in page naming user and destination. Provider chains carry the account internally instead of as a redirect login token, checking the browser cookie at every step even where `check_cookie` is off (7d83687ca).

- Federation preserves sibling changes sharing preceding snapshots through state-event fork identities. Resolution loads complete fork state, prepares resolved state before installation and batches forward-extremity replacements under the room state lock (9fcefaebd, 241ece9aa, bb8f3c13d, 0b403d7e8, 5f048d8f8).

- Shutdown-interrupted federation work avoids ordinary event-failure recording. Incoming transactions warn on missing PDUs; auth-chain failures remain distinct from fallback-eligible fork-state failures (14239a1a1, 92e536392, cb3915a3c, 8d3f6a903).

- Federation SRV routing preserves request names while resolving target hosts/ports and stripping DNS trailing dots. Malformed remote names get quieter diagnostics; local resolver timeouts and connection exhaustion still warn/error (a8e41c308, 5045ab927).

- `/events` respects requester visibility; previews expose history world-readable at the event. Own join/invite events use their resulting membership. Missing `from` starts at the current stream position; malformed cursors return `M_INVALID_PARAM`. The 50-event cap does not bound hidden-event scans (18039076c, 23ef4fdaf, 73d60b13b, 80e45ced8). Read receipts must target a visible same-room event; `m.fully_read` is not covered (0d7794fab).

- Remote profile refreshes must fit `max_remote_profile_fields` (default 100) and 64 KiB whole, or the cache stays unchanged; filter and sync field selections are capped by `max_profile_fields_per_request` (default 64). Both are reloadable; larger inputs accepted before are now refused. Refreshes keep stored null fields, which count toward the limits and can be replaced, and incremental Sliding Sync profile logs read only joined rooms (0933f37c3, 5490f37ac, 2059b2e23).

- Multi-room search limits merged pages to 10 results by default, capped at 100. Pagination uses the last returned event's count instead of skip offsets. Results are bounded; total index work is not (2431c6346).

- Concurrent claims cannot reuse one-time keys or OAuth authorization codes within one running server. Codes are consumed before validation, including invalid exchanges (25dbbb5a3, 210297c3c).

- Registration reserves names of accountless former admin-room members, returning `M_USER_IN_USE` before side effects. Existing accounts and the server user remain exempt (cc6dfd4a4, 9993cb5f8).

- Admin-room commands require `m.text`, excluding notices/emotes/captions. Outside-room escaped commands now default off (was on); reloadable `admin_escape_commands = true` retains them. Ordinary admin-room `!admin` commands remain unchanged. A shutdown interrupt before worker startup leaves the queue closed (a7ac11f1c, aa34a666c, 08c68eee1).

- Required email verification plus configured SMTP satisfies open-registration guards without tokens. Config display conceals `smtp.connection_uri` credentials and hides unparsable/hostless URIs entirely (e8f6903e4, 239870887).

- LDAP-origin interactive authentication skips local-password lookup; the LDAP build feature and `ldap.enable` remain required (6527b269f).

- Space hierarchy cache entries record local or remote provenance, so remote answers no longer serve as local authority, and participation lookup errors no longer read as absence (5b2d53487).

- A federation destination already sending returns busy before peer-status work, sender workers return to tokio's cooperative budgeting, and presence scans hand their upper counter bound to RocksDB (48487fab0, 436be8af8).

- Database seek commands destroy iterator state before releasing map and engine owners, including queued cancellation before execution (0b144693c).

- Debian packages link the system glibc on a Debian trixie baseline, declaring that dependency through cargo-deb; C++ and other libraries stay static. RocksDB moves to ThinLTO; packaging waits for passing tests (002f585e8, abd983717, 3e991df93).

- Element Web checks reject empty first passes and out-of-test errors, clear stale reports and preserve exit status. Browser/pnpm dependencies align; legacy-crypto known-failure coverage expands without fixing client behavior. Docker unit/integration binaries stop after ten minutes plus 30-second grace; benchmarks/Valgrind are uncapped. Debian checks debhelper markers; startup tests cover disabled federation (29bd18df6, a3eb7347d, 0d3cf15d0, 4ac1a2c34, 97a637828).

- @basnijholt landed several more fixes: appservices reach other users' account data only within their own namespace and the delete routes (MSC3391) refuse `m.fully_read` and `m.push_rules` (#626, 7e9466e91); per-user room scans stop at the user ID boundary, so `@alice:example.org` no longer matches `@alice:example.org.other` (#633); a leave without membership records no departure (#631); and Sliding Sync caps each room's `timeline_limit` at 100 (#637).
