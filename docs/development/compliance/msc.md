# Tuwunel MSC Implementation Status

## Columns

- **Inv**: inventory status (matrix-spec-proposals state). One of
  `merged`, `open`, `closed`, `unknown`.
- **Status**: ✅ `yes` / 🟨 `partial` / ❌ `no` / ⬛ `n/a` /
  ❔ `?` (never assessed; a question not yet asked, which is
  not the same claim as `no`),
  followed by a confidence glyph (● high / ◐ med / ○ low /
  · unknown) that reflects confidence in the assessment, not
  in the implementation.
- **Impl**: two percentages, `correct/impl`, e.g. `70/80`.
  - **correct**: the share Tuwunel adheres to correctly.
  - **impl**: the share with any code path attempting adherence,
    so `correct` never exceeds `impl`. Either may be `?`.
  - **denominator**: the MSC's homeserver-applicable scope, the
    requirements a homeserver itself must satisfy.
  - **requirement**: any requirement-shaped statement, not just
    MUST/SHOULD. Proposals are loosely normative, so "the server
    returns X" and "this field is added to Y" both count.
- **Spec**: the Matrix spec version that first published the MSC,
  in the [Spec compliance gaps](#spec-compliance-gaps) section
  only. `next` is queued for the release after `1.19`. Blank
  means no changelog from `1.1` on cites the MSC, so it was either
  adopted into the pre-`1.1` spec or has not landed in a release.

## Counts

| ✅ `yes` | 🟨 `part` | ❌ `no` | ⬛ `n/a` | total |
|---:|---:|---:|---:|---:|
| 253 | 56 | 436 | 330 | 1075 |

### Status by inventory bucket

| Inv | yes | part | no | n/a | total |
|---|---|---|---|---|---|
| merged | 181 | 15 | 12 | 60 | 268 |
| open | 41 | 29 | 337 | 198 | 605 |
| closed | 31 | 12 | 87 | 72 | 202 |

## Merged

Sorted by MSC number, highest first. Out-of-scope rows are listed
in the [Out of scope](#out-of-scope) section.

| MSC | Status | Impl | Title | Note |
|---|---|---:|---|---|
| MSC4436 | ✅ ● | 100/100 | Make server ACLs case insensitive | acl_check.rs:39 uses ruma is_allowed, which matches case insensitively |
| MSC4423 | ✅ ● | 100/100 | Undefine order of room directory | directory.rs:331 member-count order stays valid once ordering is undefined |
| MSC4402 | ✅ ◐ | 100/100 | Consistent redirects for .well-known-files | [→ MSC2499?] Global CORS layer covers any 30x; well-known client returns 200 ... |
| MSC4380 | ✅ ● | 100/100 | Invite blocking | src/api/client/sync/{v3.rs,v5/selector.rs} suppress stored invites; createRoo... |
| MSC4376 | ✅ ● | 100/100 | Remove /v1/send_join and /v1/send_leave | v1 send_join and v1 send_leave routes are not registered |
| MSC4341 | ✅ ● | 95/100 | Support for RFC 8628 Device Authorization Grant | src/api/oidc/device.rs:62; RFC 8628 device grant advertised, minted, polled |
| MSC4335 | ❌ ● | 0/0 | M_USER_LIMIT_EXCEEDED error code | M_USER_LIMIT_EXCEEDED error code not used |
| MSC4326 | ✅ ● | 100/100 | Device masquerading for appservices | appservice query device_id asserted; M_UNKNOWN_DEVICE-equivalent on missing |
| MSC4323 | ✅ ● | 100/100 | User suspension &amp; locking endpoints | src/api/client/admin.rs four routes at stable v1 paths; m.account_moderation ... |
| MSC4319 | 🟨 ● | 50/50 | Room member events for invite and knock rooms in the `/sync` response | state/mod.rs:464 puts inviter member in stripped state; no `state` key |
| MSC4312 | ✅ ● | 85/85 | Resetting cross-signing keys in the OAuth world | cross-signing reset via OAuth account deep-link; dual-stage advertise deferred |
| MSC4311 | ✅ ● | 85/95 | Ensuring the create event is available on invites | full-PDU invite/knock state; create-event validation + M_MISSING_PARAM + 5xx |
| MSC4307 | ✅ ● | 100/100 | Validate that `auth_events` are in the correct room | auth_event room_id mismatch rejected |
| MSC4304 | ✅ ● | 90/100 | Room Version 12 | V12 supported as stable; default is V11 |
| MSC4297 | ✅ ● | 100/100 | State Resolution v2.1 | src/service/rooms/state_res/resolve.rs:257 conflicted state subgraph; tests pass |
| MSC4291 | ✅ ● | 100/100 | Room IDs as hashes of the create event | v12 upgrade create event omits deprecated predecessor.event_id |
| MSC4289 | ✅ ● | 100/100 | Explicitly privilege room creators | Room v12 creator privilege; complement 22 pass, 0 fail across six named tests |
| MSC4284 | 🟨 ● | 70/90 | Policy Servers | outbound /sign, inbound verify, fetch-on-missing, reversible soft-fail |
| MSC4277 | ✅ ● | 100/100 | Harmonizing the reporting endpoints | all 3 wired; score removed; user report 200 regardless to deter enumeration |
| MSC4267 | ✅ ● | 90/100 | Automatically forgetting rooms on leave | auto-forget on Leave/Ban; stable + unstable capability advertised |
| MSC4260 | ✅ ● | 85/90 | Reporting users (Client-Server API) | src/api/client/report.rs:63; admin notification, 404 M_NOT_FOUND on unknown u... |
| MSC4254 | ✅ ● | 95/100 | Usage of [RFC7009] Token Revocation for Matrix client logout | src/api/oidc/revoke.rs:37; RFC7009 form-urlencoded; revokes both tokens; 200 ... |
| MSC4239 | ✅ ● | 100/100 | Room version 11 as the default room version | default_default_room_version = V11 |
| MSC4225 | ✅ ● | 100/100 | Specification of an order in which one-time-keys should be issued | OTKs issued in upload order via count_be prefix; src/service/users/keys.rs:99 |
| MSC4222 | ✅ ● | 95/100 | Adding `state_after` to `/sync` | dialects honored; lazy-loaded state_after carries changed members |
| MSC4213 | ✅ ● | 90/90 | Remove `server_name` parameter | join/knock use via; server_name still accepted via Ruma fallback |
| MSC4210 | ✅ ● | 100/100 | Remove legacy mentions | deprecated mention push rules removed at /pushrules read time |
| MSC4191 | ✅ ● | 100/100 | Account management for OAuth 2.0 API | all six actions advertised and handled; stable names alias prototypes |
| MSC4190 | ✅ ● | 100/100 | Device management for application services | 201 on create; M_APPSERVICE_LOGIN_UNSUPPORTED on AS login+register |
| MSC4189 | ✅ ◐ | 80/100 | Allowing guests to access uploaded media | guest tokens accepted on authenticated media routes |
| MSC4186 | ✅ ● | 85/88 | Simplified Sliding Sync | sync v5 served with lists, filters, extensions; no expanded_timeline or stubs |
| MSC4183 | ❌ ● | 0/0 | Additional Error Codes for submitToken endpoints | mixed MSC; the submit_url half is homeserver scope and we emit no submit_url |
| MSC4180 | ✅ ● | 100/100 | Add a stable flag to MSC3916 | stable feature flag for MSC3916 advertised |
| MSC4178 | 🟨 ● | 75/90 | Error codes for requestToken | msisdn returns M_THREEPID_MEDIUM_NOT_SUPPORTED; bad email returns M_INVALID_P... |
| MSC4175 | ✅ ● | 100/100 | Profile field for user time zone | timezone PUT/DELETE/GET routes; m.tz aliased in profile and over federation |
| MSC4170 | ✅ ◐ | 100/100 | 403 error responses for profile APIs | profile lookup unrestricted; MUST minimum satisfied |
| MSC4169 | ✅ ● | 100/100 | Backwards-compatible redaction sending using `/send` | src/api/client/send.rs:42; lifts content.redacts into PduBuilder.redacts; adv... |
| MSC4163 | ✅ ● | 100/100 | Make ACLs apply to EDUs | ACLs applied on receipt and typing EDUs |
| MSC4156 | ✅ ● | 100/100 | Migrate `server_name` to `via` | via parameter handled via Ruma |
| MSC4151 | ✅ ● | 100/100 | Reporting rooms (Client-Server API) | POST /rooms/{roomId}/report implemented and routed |
| MSC4138 | ✅ ● | 100/100 | Update allowed HTTP methods in CORS responses | CORS METHODS list includes HEAD and PATCH; excludes CONNECT/TRACE |
| MSC4133 | ✅ ● | 85/95 | Extending User Profile API with Custom Key:Value Pairs | endpoints + caps/errcodes; unset legacy 200 omit by choice, 404 key-scoped |
| MSC4127 | ❌ ● | 0/0 | Removal of query string auth | src/api/router/auth.rs:78 still falls back to request.query.access_token |
| MSC4126 | ✅ ● | 100/100 | Deprecation of query string auth | deprecation of query string auth; server still accepts both |
| MSC4115 | ✅ ● | 95/85 | membership metadata on events | src/core/matrix/pdu/unsigned.rs add_membership; src/service/rooms/state_acces... |
| MSC4041 | ✅ ◐ | 90/90 | Use http header Retry-After to enable library-assisted retry handling | Ruma error type emits Retry-After header for LimitExceeded responses. |
| MSC4040 | ✅ ● | 100/100 | Update SRV service name to IANA registration | Tuwunel queries _matrix-fed first then falls back to _matrix. |
| MSC4026 | ✅ ◐ | 80/90 | Allow /versions to optionally accept authentication | versions endpoint accepts optional auth via Ruma |
| MSC4025 | ✅ ● | 85/85 | Local user erasure requests | phase B landed: erased senders serve pruned to clients and federation |
| MSC4010 | ✅ ● | 100/100 | Push rules and account data | m.push_rules and m.fully_read rejected on /account_data |
| MSC4009 | ✅ ● | 100/100 | Expanding the Matrix ID grammar to enable E.164 IDs | E.164 + character allowed via Ruma localpart validation |
| MSC3989 | ✅ ● | 100/100 | Redact `origin` property on events | V11 redaction drops origin via Ruma RedactionRules |
| MSC3987 | ✅ ● | 90/90 | Push actions clean-up | unknown push actions ignored as no-ops |
| MSC3981 | ✅ ● | 100/100 | `/relations` recursion | /relations recurse parameter implemented with depth 3 |
| MSC3980 | ✅ ● | 90/90 | Dotted Field Consistency | event_fields trim on /sync; dotted paths use the MSC3873 escape grammar |
| MSC3970 | 🟨 ◐ | 70/80 | Scope transaction IDs to devices | txn key is user, device, txn with no path; redact untracked; echo user scoped |
| MSC3967 | ✅ ● | 100/100 | Do not require UIA when first uploading cross signing keys | keys/device_signing/upload skips UIA when user has no existing cross-signing ... |
| MSC3966 | ✅ ● | 100/100 | `event_property_contains` push rule condition | event_property_contains supported via Ruma push conditions |
| MSC3958 | ✅ ● | 100/100 | Suppress notifications from message edits | SuppressEdits push rule provided via Ruma server_default ruleset |
| MSC3952 | ✅ ◐ | 80/90 | Intentional Mentions | Intentional mentions push rules ride on Ruma server_default; flag advertised. |
| MSC3943 | ✅ ● | 100/100 | Partial joins to nameless rooms should include heroes' memberships. | send_join partial-state response includes hero memberships and their auth chains |
| MSC3939 | ✅ ● | 90/90 | Account locking | Login gate landed; /refresh and OIDC token paths still mint for locked accounts |
| MSC3938 | ✅ ● | 100/100 | Remove deprecated `keyId` parameters from `/keys` endpoints | only the keyless /key/v2/server is routed; no notary /query left to change |
| MSC3930 | ✅ ● | 100/100 | Polls push rules/notifications | Default poll push rules via ruma server_default; seeded at registration |
| MSC3925 | 🟨 ◐ | 75/85 | m.replace aggregation with full event | Full m.replace bundle; gated default-off; typed index, ts-ordered selection |
| MSC3916 | ✅ ● | 90/100 | Authentication for media access, and new endpoint names | New /client/v1/media and /federation/v1/media auth endpoints implemented. |
| MSC3905 | ✅ ● | 100/100 | Application services should only be interested in local users | src/service/appservice/append.rs:66; local-user guard at the three event-inte... |
| MSC3904 | ✅ ● | 100/100 | Room version 10 as the default room version | default_room_version is v11, at or above the v10 the MSC suggests |
| MSC3882 | ✅ ● | 90/100 | Allow an existing session to sign in a new session | POST /login/get_token implemented with UIA |
| MSC3873 | ✅ ● | 100/100 | event_match dotted keys | dotted-key escape semantics handled in ruma flattened JSON |
| MSC3861 | ✅ ● | 90/95 | Next-generation auth for Matrix, based on OAuth 2.0/OIDC | Advertised as org.matrix.msc3861; cores 2964-2967 + merged periphery all yes |
| MSC3860 | ✅ ◐ | 95/100 | Media Download Redirects | Emits 307 to presigned object-store URL on allow_redirect; default-off gate |
| MSC3856 | ✅ ● | 85/100 | Threads List API | Latest-activity order, participated, ignored views, 403, next_batch all served |
| MSC3844 | ✅ ● | 100/100 | Remove "Mjolnir" (policy room) sharing mechanism | removal of unused Mjolnir share endpoint; Tuwunel never implemented it |
| MSC3828 | ✅ ● | 100/100 | Content Repository Cross Origin Resource Policy (CORP) Headers | media endpoints return Cross-Origin-Resource-Policy: cross-origin |
| MSC3827 | ✅ ● | 100/100 | Filtering of `/publicRooms` by room type | /publicRooms supports room_types filter and returns room_type |
| MSC3824 | ✅ ◐ | 90/100 | OAuth 2.0 API aware clients | oauth_aware_preferred set; SSO redirect action forwards as OIDC prompt (opt-in) |
| MSC3823 | ✅ ● | 80/100 | Account Suspension | src/service/rooms/timeline/build.rs check_pdu_for_suspended_sender + auth.rs ... |
| MSC3821 | ✅ ● | 90/100 | Update redaction rules, again | redact_in_place uses Ruma RedactionRules.V11 with keep third_party_invite.signed |
| MSC3820 | ✅ ● | 90/100 | Room Version 11 | v11 stable; redaction and auth rules dispatch via Ruma RoomVersionRules |
| MSC3818 | ✅ ● | 100/100 | Copy room type on upgrade | upgrade reuses old m.room.create content; type preserved by default |
| MSC3816 | ✅ ● | 100/100 | Clarify Thread Participation | current_user_participated recomputed per requester at read time on all paths |
| MSC3787 | ✅ ● | 100/100 | Allowing knocks to restricted rooms | Knock-restricted fully wired; remote re-knock reconciles the room server |
| MSC3786 | ✅ ● | 100/100 | Add a default push rule to ignore `m.room.server_acl` events | server_acl predefined push rule via Ruma defaults |
| MSC3773 | ✅ ● | 100/100 | Notifications for threads | src/service/pusher/notification.rs:143 per-thread counts; src/api/client/sync... |
| MSC3771 | ✅ ● | 100/100 | Read receipts for threads | src/api/client/read_marker.rs validates+routes thread; receipt and private_re... |
| MSC3765 | ✅ ● | 90/100 | Rich text in room topics | reader prefers m.topic text/plain; topic text now indexed for search |
| MSC3758 | ✅ ● | 90/100 | Add `event_property_is` push rule condition kind | event_property_is dispatched via Ruma Ruleset::get_actions |
| MSC3743 | ✅ ● | 90/100 | Standardized error response for unknown endpoints | M_UNRECOGNIZED 404/405 fallback wired in router |
| MSC3715 | ✅ ● | 100/100 | Add a pagination direction parameter to `/relations` | dir parameter on /relations is parsed and used |
| MSC3706 | ✅ ● | 90/100 | Extensions to `/_matrix/federation/v2/send_join/{roomId}/{eventId}` for parti... | send_join supports omit_members, members_omitted, servers_in_room |
| MSC3667 | ✅ ● | 100/100 | Enforce integer power levels | integer_power_levels enforced via RoomVersionRules from V10+ |
| MSC3666 | ✅ ● | 90/100 | Bundled aggregations for server side search | result + context events bundled; thread always, edit/ref gated default-off |
| MSC3604 | ✅ ● | 100/100 | Room Version 10 | V10 supported; integer_power_levels and knock_restricted enforced |
| MSC3589 | ✅ ● | 100/100 | Room version 9 as a default | default_room_version defaults to V11 (exceeds V9) |
| MSC3582 | ✅ ● | 100/100 | Remove m.room.message.feedback | feedback removal; tuwunel never produces or dispatches on m.room.message.feed... |
| MSC3567 | ✅ ● | 100/100 | Allow requesting events from the start/end of the room history | from is optional; defaults to start/end based on dir |
| MSC3550 | ❌ ◐ | 0/0 | Add HTTP 403 to possible profile lookup responses | CS /profile returns 403 M_FORBIDDEN when outbound profile lookup disabled |
| MSC3442 | ✅ ● | 100/100 | move the `prev_content` key to `unsigned` | prev_content placed under unsigned in created/appended PDUs |
| MSC3440 | ✅ ● | 80/88 | MSC3440 Threading via `m.thread` relation | [→ MSC3856] related_by_* filters on /messages+/context; .stable advert; neste... |
| MSC3419 | ✅ ○ | 100/100 | Guest State Events | no guest-specific gate on state-event send path; PL/auth_check applies unifor... |
| MSC3383 | ✅ ● | 100/100 | Include destination in X-Matrix Auth Header | X-Matrix destination field validated on inbound federation |
| MSC3375 | ✅ ● | 100/100 | Room Version 9 | room v9 stable; redaction keeps join_authorised_via_users_server |
| MSC3316 | ✅ ● | 100/100 | Proposal to add timestamp massaging to the spec | appservice ts honored on /send and /state |
| MSC3289 | ✅ ● | 100/100 | Room Version 8 | room v8 listed stable; restricted join rule auth implemented |
| MSC3288 | ❌ ● | 0/0 | Add room type to `/_matrix/identity/v2/store-invite` API | sole clause is addressed to homeservers, so not n/a; no 3PID invite at all |
| MSC3283 | ✅ ● | 100/100 | Expose enable_set_displayname, enable_set_avatar_url and enable_3pid_changes ... | src/api/client/capabilities.rs explicitly emits m.set_displayname, m.set_avat... |
| MSC3267 | 🟨 ◐ | 60/80 | reference relationships | m.reference bundle added; gated default-off; no ignore/visibility filter |
| MSC3266 | ✅ ● | 100/100 | Room Summary API | summary endpoint routed at unstable and (via Ruma) stable paths |
| MSC3231 | ✅ ● | 100/100 | Token Authenticated Registration | registration token UIA + validity endpoint implemented |
| MSC3173 | ✅ ● | 100/100 | Expose stripped state events to any potential joiner | summary_stripped includes recommended events incl create |
| MSC3083 | ✅ ● | 100/100 | Restricting room membership based on membership in other rooms | restricted_join_rule auth via RoomVersionRules; v8/v9 |
| MSC3069 | ✅ ◐ | 80/100 | Allow guests to use /account/whoami | whoami returns is_guest; uses is_deactivated heuristic |
| MSC3030 | ✅ ● | 90/100 | Jump to date API endpoint | client+fed handlers, outbound fallback + same-ts order; 3 leaves AS-blocked |
| MSC2998 | ✅ ● | 100/100 | Room Version 7 | V7 listed in STABLE_ROOM_VERSIONS; full knock support present |
| MSC2967 | ✅ ● | 80/90 | API scopes | src/api/oidc/token.rs:96; stable+unstable device scope honored, surfaced, req... |
| MSC2966 | ✅ ● | 85/95 | Usage of OAuth 2.0 Dynamic Client Registration in Matrix | Full RFC 7591/8252 metadata validation; drops unknown grant/response |
| MSC2965 | ✅ ● | 90/100 | OAuth 2.0 Authorization Server Metadata discovery | auth_issuer and auth_metadata routes return OAuth provider metadata |
| MSC2964 | ✅ ● | 88/100 | Usage of OAuth 2.0 authorization code grant and refresh token grant | src/api/oidc/authorize.rs:57; S256 PKCE now required on the auth-code grant |
| MSC2946 | ✅ ● | 90/100 | Spaces Summary | client and federation hierarchy endpoints implemented |
| MSC2918 | ✅ ● | 90/100 | Refresh tokens | /refresh, expires_in_ms, refresh_token in /login and /register |
| MSC2870 | ✅ ◐ | 100/100 | Protect server ACLs from redaction | redaction dispatches on RoomVersionRules.redaction; ruma MSC2870 enabled |
| MSC2867 | ✅ ◐ | 100/100 | Marking rooms as unread | client convention; account data type stored generically |
| MSC2858 | ✅ ● | 100/100 | Multiple SSO Identity Providers | identity_providers in /login flows; /login/sso/redirect/{idpId} routed |
| MSC2844 | ✅ ● | 90/90 | Using a global version number for the entire specification | src/api/client/versions.rs advertises v1.1 through v1.15 |
| MSC2832 | ✅ ● | 100/100 | Homeserver -&gt; Application Service authorization header | src/service/appservice/request.rs sends Bearer header and query |
| MSC2788 | ✅ ● | 100/100 | Room version 6 as a default | default_default_room_version is V11 in src/core/config/mod.rs:3842 |
| MSC2778 | ✅ ● | 100/100 | Providing authentication method for appservice users | src/api/client/session/appservice.rs implements m.login.application_service |
| MSC2732 | ✅ ● | 100/100 | Olm fallback keys | src/api/client/keys/claim_keys.rs:86; upload, claim-fallback, sync-unused-lis... |
| MSC2705 | ❌ ◐ | 0/10 | Animated thumbnails | animated param accepted; thumbnails always PNG static |
| MSC2702 | ✅ ● | 100/100 | `Content-Disposition` usage in the media repo | Content-Disposition and inline allowlist enforced for media downloads, thumbn... |
| MSC2701 | ✅ ◐ | 80/90 | Media and the `Content-Type` relationship | Optional Content-Type accepted; stored and returned |
| MSC2689 | ✅ ◐ | 100/100 | Allow guests to operate in encrypted rooms | Auth treats guests like users; /members open |
| MSC2677 | ✅ ● | 80/90 | Annotations and Reactions | Duplicate annotation rejected; reactions plumbed |
| MSC2676 | 🟨 ● | 50/60 | Message editing | edits accepted/relayed; no m.replace bundle or new_content apply |
| MSC2675 | 🟨 ◐ | 60/75 | Serverside aggregations of message relationships | thread+edit+reference bundled (edit/ref gated); reactions unbundled |
| MSC2674 | ✅ ● | 90/100 | Event relationships | relates_to handled in append; rel_type tracked |
| MSC2666 | ✅ ● | 92/100 | Get rooms in common with another user | v1 stable path + count + cursor pagination; self-query/non-compliant 400 |
| MSC2663 | ✅ ● | 100/100 | Errors for dealing with non-existent push rules | src/api/client/push.rs all 7 endpoints return NotFound |
| MSC2659 | ✅ ● | 95/100 | Application service ping endpoint | ping surfaces M_CONNECTION_TIMEOUT/FAILED and M_BAD_STATUS per spec |
| MSC2611 | ✅ ● | 100/100 | Remove `m.login.token` User-Interactive Authentication type from the specific... | AuthType::Token UIAA not advertised; m.login.token login is unrelated |
| MSC2610 | ✅ ● | 100/100 | Remove `m.login.oauth2` User-Interactive Authentication type from the specifi... | AuthType::OAuth2 not advertised; only Password/Sso/Jwt flows |
| MSC2604 | ❌ ● | 0/0 | Parameters for Login Fallback | login fallback page is homeserver served, so not n/a; Tuwunel ships no page |
| MSC2540 | ✅ ◐ | 80/80 | Stricter event validation: JSON compliance | canonical JSON enforced at the parse boundary, not keyed off room version |
| MSC2526 | ✅ ● | 100/100 | Add ability to delete key backups | src/api/client/backup.rs:134 delete_backup_version_route |
| MSC2457 | ✅ ● | 100/100 | Invalidating devices during password modification | src/api/client/account.rs:41 honors body.logout_devices |
| MSC2454 | ✅ ● | 90/90 | User-Interactive Authentication for SSO-backed homeserver | src/api/router/auth/uiaa.rs:53 sso_flow; sso/uiaa.rs serves fallback |
| MSC2451 | ✅ ● | 100/100 | Remove the `query_auth` federation endpoint | No /query_auth route registered in src/api/router.rs |
| MSC2432 | ✅ ◐ | 80/90 | Updated semantics for publishing room aliases | alt_aliases wired; canonical_alias resolve check; rooms/{}/aliases route present |
| MSC2414 | ✅ ● | 100/100 | Make `reason` and `score` optional for reporting content | reason and score are Option in ruma report types; route accepts both |
| MSC2409 | 🟨 ● | 70/70 | Proposal to send typing, presence and receipts to appservices | typing+receipt EDUs sent to AS; presence not forwarded |
| MSC2403 | ✅ ● | 90/90 | Add "knock" feature | Knock CS+SS endpoints, sync key, public-rooms join_rule all wired |
| MSC2367 | ✅ ● | 100/100 | Allowing Reasons in all Membership Events | reason field handled in invite/leave/kick/ban/unban/join membership routes |
| MSC2334 | ✅ ● | 100/100 | [MSC2334](https://github.com/matrix-org/matrix-doc/pull/2334) - Change defaul... | Default room version is V11, well past V5 |
| MSC2290 | 🟨 ● | 65/55 | Separate Endpoints for Binding Threepids | add endpoint (UIA+dupe) + HS email validation; IS-bind half out of scope |
| MSC2285 | ✅ ● | 90/100 | Private read receipts | src/api/client/read_marker.rs handles ReadPrivate via private_read_set |
| MSC2265 | 🟨 ● | 75/100 | Proposal for mandating case folding when processing e-mail addresses | HS case-folds whole email (ss-fold) before storage; IS migration out of scope |
| MSC2264 | ✅ ● | 90/100 | Add an unstable feature flag to MSC2140 for clients to detect support | flag not advertised; r0.6.0 and later advertised instead, as Synapse does |
| MSC2263 | ✅ ● | 80/80 | Give homeservers the ability to handle their own 3PID registrations/password ... | HS email registration; binds the address in any UIA stage order |
| MSC2249 | ✅ ● | 90/100 | Require users to have visibility on an event when submitting reports | src/api/client/report.rs:173 verifies sender is room member; PDU lookup gated |
| MSC2246 | ✅ ● | 100/100 | Asynchronous media uploads | async media routes wired; create_pending, upload_pending, error codes present |
| MSC2244 | ❌ ● | 0/0 | Mass redactions | Single-target redactions only; no array redacts handling |
| MSC2240 | ✅ ● | 100/100 | Room Version 6 | V6 in STABLE_ROOM_VERSIONS; v6 auth rules and rules engine implemented |
| MSC2229 | 🟨 ● | 0/20 | Allowing 3PID Owners to Rebind | [→ MSC2290] owner exemption at 3pid/add only; requestToken still rejects the ... |
| MSC2209 | ✅ ● | 100/100 | Update auth rules to check notifications key in m.room.power_levels | limit_notifications_power_levels enforced for v6+ |
| MSC2197 | ✅ ● | 100/100 | Search Filtering in Public Room Directory over Federation | POST /_matrix/federation/v1/publicRooms with filter implemented |
| MSC2181 | ✅ ● | 100/100 | Add an Error Code for Signaling a Deactivated User | M_USER_DEACTIVATED returned by login paths |
| MSC2176 | ✅ ● | 100/100 | Update the redaction rules | redact_in_place uses room_version_rules.redaction |
| MSC2175 | ✅ ● | 100/100 | Remove the `creator` field from `m.room.create` events | creator() falls back to sender when use_room_create_sender |
| MSC2174 | ✅ ● | 100/100 | move the `redacts` property to `content` | src/core/matrix/event/redact.rs handles redacts move per room rules |
| MSC2140 | 🟨 ● | 50/60 | Terms of Service API for Identity Servers and Integration Managers | no 3pid/unbind or IS relay; accepted_terms and register cleanup present |
| MSC2134 | ❌ ● | 0/0 | Identity Hash Lookups | no IS lookup client; no hash_details, pepper or v1 fallback code |
| MSC2078 | ✅ ● | 90/100 | Sending Third-Party Request Tokens via the Homeserver | HS sends and validates email request tokens itself via magic-link, no id server |
| MSC2077 | ✅ ● | 100/100 | Room version 5 | src/core/config/room_version.rs:7; v5 unstable but supported |
| MSC2076 | ❌ ◐ | 0/10 | Enforce key-validity periods when validating event signatures | minimum_valid_until_ts passed for fetches; per-event ts check absent |
| MSC2033 | ✅ ● | 100/100 | Proposal to include device IDs in `/account/whoami` | src/api/client/account.rs:74 returns device_id in whoami response |
| MSC2002 | ✅ ● | 100/100 | MSC 2002 - Rooms V4 | v4 in supported_room_versions; ruma rules implement v4 |
| MSC1983 | ✅ ● | 100/100 | Proposal to add reasons for leaving a room | src/api/client/membership/leave.rs:21 passes body.reason to leave |
| MSC1957 | 🟨 ● | 50/50 | Integration manager discovery | account-data passthrough only; no m.integrations key in well-known client |
| MSC1954 | ✅ ● | 100/100 | Remove prev_content from the essential keys list | merged; identical to MSC1953; ruma redact omits prev_content |
| MSC1946 | ✅ ◐ | 80/90 | Secure Secret Storage and Sharing | generic account_data + to-device pipe carry secret storage/sharing |
| MSC1930 | ✅ ● | 100/100 | Proposal to add a default push rule for m.room.tombstone events | ruma Ruleset::server_default includes ConditionalPushRule::tombstone() |
| MSC1929 | ✅ ● | 100/100 | MSC1929 Homeserver Admin Contact and Support page | src/api/client/well_known.rs:42; multiple contacts via support_contact map |
| MSC1915 | 🟨 ● | 60/55 | MSC 1915 - Add unbind 3PID APIs | delete + deactivate return id_server_unbind_result; IS unbind out of HS scope |
| MSC1884 | ✅ ● | 100/100 | Proposal to replace slashes in event IDs | room v4 supported via ruma EventIdFormatVersion::V3 (URL-safe base64) |
| MSC1866 | ✅ ◐ | 100/100 | MSC 1866 - Unsupported Room Version Error Code for Invites | client invite maps remote unsupported-version to M_UNSUPPORTED_ROOM_VERSION |
| MSC1831 | ✅ ● | 100/100 | Proposal to do SRV lookups after .well-known to discover homeservers | src/service/resolver/actual.rs:79 well-known before SRV |
| MSC1819 | ✅ ● | 100/100 | Remove references to presence lists | duplicate of MSC1818; presence lists not implemented |
| MSC1812 | ✅ ● | 100/100 | MSC 1813 - Federation Make Membership Room Version | src/api/server/make_leave.rs:34 and make_join.rs:52 set room_version |
| MSC1804 | ✅ ● | 100/100 | Proposal for advertising capable room versions to clients | src/api/client/capabilities.rs sets RoomVersionsCapability |
| MSC1802 | ✅ ● | 100/100 | Remove the '200' value from some federation responses | src/api/server/send_join.rs:30 and send_leave.rs:15 handle v2 |
| MSC1794 | ✅ ● | 100/100 | MSC 1794 - Federation v2 Invite API | src/api/server/invite.rs:28 implements PUT /federation/v2/invite |
| MSC1772 | ✅ ● | 90/90 | Proposal for Matrix "spaces" (formerly known as "groups as rooms (take 2)") | spaces implemented; src/api/client/space.rs hierarchy + room create with type |
| MSC1767 | ❌ ◐ | 0/0 | Extensible events in Matrix | no extensible-events handling; relies on generic event relay |
| MSC1759 | ❌ ◐ | 10/20 | MSC 1759 - Rooms V2 | v2 algorithm in use for v3+; v2 itself not in supported_room_versions |
| MSC1756 | ✅ ● | 90/100 | Cross-signing devices with device signing keys | src/api/client/keys/upload_signing_keys.rs and upload_signatures.rs implement... |
| MSC1753 | ✅ ● | 100/100 | client-server capabilities API | src/api/client/capabilities.rs handles GET /capabilities incl m.change_password |
| MSC1730 | ✅ ● | 100/100 | Mechanism for redirecting to an alternative server during login | src/api/client/session/mod.rs:176 sets well_known on login response |
| MSC1721 | ✅ ● | 100/100 | Rename `m.login.cas` to `m.login.sso` | src/api/client/session/sso.rs and uiaa.rs advertise m.login.sso |
| MSC1711 | ✅ ◐ | 100/100 | X.509 certificate verification for federation connections | reqwest+rustls; tls_fingerprints not exposed; standard CA validation |
| MSC1708 | ✅ ● | 90/100 | .well-known support for server name resolution | src/service/resolver/well_known.rs; resolver/actual.rs ordering matches spec |
| MSC1693 | ✅ ● | 100/100 | Specify how to handle rejected events in new state res | rejected event handling in iterative auth check matches MSC1442 amendment |
| MSC1692 | ✅ ● | 95/100 | Terms of service at registration | m.login.terms stage built from registration_terms config; UIA completes it |
| MSC1659 | ✅ ● | 90/100 | Changing Event IDs to be Hashes | reference_hash event IDs; v3 in UNSTABLE_ROOM_VERSIONS; auth_events as list-o... |
| MSC1501 | ✅ ● | 90/90 | Room version upgrades | upgrade endpoint present; tombstone, predecessor, PL freeze all implemented |
| MSC1466 | ✅ ● | 100/100 | Soft Remote Logout Proposal | soft_logout=true returned for expired tokens in 401 responses |
| MSC1442 | ✅ ● | 90/100 | State Resolution: Reloaded | state res v2 implemented in src/service/rooms/state_res/resolve.rs |
| MSC1219 | ✅ ● | 82/100 | Storing megolm keys serverside | better-key compare in add_key (all PUT routes); 403 M_WRONG_ROOM_KEYS_VERSION |

## Spec compliance gaps

Merged MSCs (in the live Matrix spec) that Tuwunel does not
fully implement. These are the highest-priority items to fix
for spec compliance.

| MSC | Status | Impl | Spec | Title | Note |
|---|---|---:|---:|---|---|
| MSC2265 | 🟨 ● | 75/100 | 1.1 | Proposal for mandating case folding when processing e-mail addresses | HS case-folds whole email (ss-fold) before storage; IS migration out of scope |
| MSC3925 | 🟨 ◐ | 75/85 | 1.7 | m.replace aggregation with full event | Full m.replace bundle; gated default-off; typed index, ts-ordered selection |
| MSC4178 | 🟨 ● | 75/90 | 1.13 | Error codes for requestToken | msisdn returns M_THREEPID_MEDIUM_NOT_SUPPORTED; bad email returns M_INVALID_P... |
| MSC2409 | 🟨 ● | 70/70 | 1.13 | Proposal to send typing, presence and receipts to appservices | typing+receipt EDUs sent to AS; presence not forwarded |
| MSC3970 | 🟨 ◐ | 70/80 | 1.7 | Scope transaction IDs to devices | txn key is user, device, txn with no path; redact untracked; echo user scoped |
| MSC4284 | 🟨 ● | 70/90 | 1.18 | Policy Servers | outbound /sign, inbound verify, fetch-on-missing, reversible soft-fail |
| MSC2290 | 🟨 ● | 65/55 |  | Separate Endpoints for Binding Threepids | add endpoint (UIA+dupe) + HS email validation; IS-bind half out of scope |
| MSC1915 | 🟨 ● | 60/55 |  | MSC 1915 - Add unbind 3PID APIs | delete + deactivate return id_server_unbind_result; IS unbind out of HS scope |
| MSC2675 | 🟨 ◐ | 60/75 | 1.3 | Serverside aggregations of message relationships | thread+edit+reference bundled (edit/ref gated); reactions unbundled |
| MSC3267 | 🟨 ◐ | 60/80 | 1.5 | reference relationships | m.reference bundle added; gated default-off; no ignore/visibility filter |
| MSC1957 | 🟨 ● | 50/50 |  | Integration manager discovery | account-data passthrough only; no m.integrations key in well-known client |
| MSC2140 | 🟨 ● | 50/60 |  | Terms of Service API for Identity Servers and Integration Managers | no 3pid/unbind or IS relay; accepted_terms and register cleanup present |
| MSC2676 | 🟨 ● | 50/60 | 1.4 | Message editing | edits accepted/relayed; no m.replace bundle or new_content apply |
| MSC4319 | 🟨 ● | 50/50 |  | Room member events for invite and knock rooms in the `/sync` response | state/mod.rs:464 puts inviter member in stripped state; no `state` key |
| MSC2229 | 🟨 ● | 0/20 |  | Allowing 3PID Owners to Rebind | [→ MSC2290] owner exemption at 3pid/add only; requestToken still rejects the ... |
| MSC1759 | ❌ ◐ | 10/20 |  | MSC 1759 - Rooms V2 | v2 algorithm in use for v3+; v2 itself not in supported_room_versions |
| MSC1767 | ❌ ◐ | 0/0 |  | Extensible events in Matrix | no extensible-events handling; relies on generic event relay |
| MSC2076 | ❌ ◐ | 0/10 |  | Enforce key-validity periods when validating event signatures | minimum_valid_until_ts passed for fetches; per-event ts check absent |
| MSC2134 | ❌ ● | 0/0 |  | Identity Hash Lookups | no IS lookup client; no hash_details, pepper or v1 fallback code |
| MSC2244 | ❌ ● | 0/0 |  | Mass redactions | Single-target redactions only; no array redacts handling |
| MSC2604 | ❌ ● | 0/0 | 1.1 | Parameters for Login Fallback | login fallback page is homeserver served, so not n/a; Tuwunel ships no page |
| MSC2705 | ❌ ◐ | 0/10 | 1.11 | Animated thumbnails | animated param accepted; thumbnails always PNG static |
| MSC3288 | ❌ ● | 0/0 | 1.2 | Add room type to `/_matrix/identity/v2/store-invite` API | sole clause is addressed to homeservers, so not n/a; no 3PID invite at all |
| MSC3550 | ❌ ◐ | 0/0 | 1.2 | Add HTTP 403 to possible profile lookup responses | CS /profile returns 403 M_FORBIDDEN when outbound profile lookup disabled |
| MSC4127 | ❌ ● | 0/0 |  | Removal of query string auth | src/api/router/auth.rs:78 still falls back to request.query.access_token |
| MSC4183 | ❌ ● | 0/0 | 1.18 | Additional Error Codes for submitToken endpoints | mixed MSC; the submit_url half is homeserver scope and we emit no submit_url |
| MSC4335 | ❌ ● | 0/0 | 1.18 | M_USER_LIMIT_EXCEEDED error code | M_USER_LIMIT_EXCEEDED error code not used |

## Open

Sorted by MSC number, highest first. Out-of-scope rows are listed
in the [Out of scope](#out-of-scope) section.

| MSC | Status | Impl | Title | Note |
|---|---|---:|---|---|
| MSC4532 | ❌ ● | 0/0 | Revised Social Presence | no m.presence.persistent or active/idle states; tuwunel still auto-idles |
| MSC4528 | ❌ ● | 0/0 | `M_INCOMPATIBLE_SERVER` error code | M_INCOMPATIBLE_SERVER absent from Tuwunel and from the ruma fork |
| MSC4527 | ❌ ● | 0/0 | State event power levels | no state map in power levels and no MSC4527 room version |
| MSC4526 | ✅ ◐ | 80/100 | Improve OAuth 2.0 refresh token grant error handling | invalid_grant on refresh failures, but every 4xx maps to it, bad params too |
| MSC4525 | ❌ ● | 0/0 | Paginated Sync | no paginated sync endpoint; only MSC4186 simplified sliding sync exists |
| MSC4524 | ❌ ● | 0/0 | Federation Bi-directional Ping | no federation ping or pong route; the cited commit is a Continuwuity branch |
| MSC4521 | ❌ ● | 0/0 | Adaptive set reconciliation | no PinSketch, minisketch or algebraic_v1 digest anywhere in the tree |
| MSC4519 | 🟨 ● | 60/80 | MatrixRTC Transports Registry | src/api/client/rtc.rs:7 serves the MSC4143-prefixed unstable transports route |
| MSC4517 | ❌ ● | 0/0 | Federated thirdparty lookups | no server= parameter and no federation thirdparty routes; local lookups only |
| MSC4514 | ❌ ● | 0/0 | EDU size limit | no 65536 byte EDU cap; only the 24 MiB global request body limit applies |
| MSC4513 | ✅ ◐ | 90/90 | Standard HTTP status codes for all endpoints, current and future | central kind to status map emits 413, 429, 502, 504 with Matrix error bodies |
| MSC4512 | ❌ ● | 0/0 | Delegating parts of the Client-Server and Server-Server API to application se... | no proxy_prefix/proxy_url in AS registration; no appservice/fed_proxy route |
| MSC4511 | ❌ ● | 0/0 | Topological query API with sparse fieldsets and Merkleized metadata | no topological query endpoint, sparse fieldsets or Merkleized metadata |
| MSC4509 | ❌ ● | 0/0 | Auto join on accepted knock | no auto-join on accepted knock; prev_sender emission is pre-existing |
| MSC4508 | 🟨 ◐ | 50/70 | Sliding Sync Extensions: Common format and Typing | typing extension present, but in the pre-MSC m.typing shape and unadvertised |
| MSC4507 | ❌ ● | 0/0 | Return additional room state from `/_matrix/federation/v1/hierarchy/{roomId}` | federation /hierarchy has no additional_state param or m.public_state |
| MSC4506 | ❌ ● | 0/0 | Push Rules for Knocking | no recipient_permission condition and no .m.rule.knock default rule |
| MSC4505 | ❌ ● | 0/0 | Push Rules for Live Location Sharing | no beacon push rules; ruma's default ruleset defines none |
| MSC4503 | ❌ ● | 0/0 | External Protocol Handles | m.external_handle unrecognized; generic MSC4133 passthrough only |
| MSC4502 | ❌ ● | 0/0 | Targeted and unrestricted room member queries | no is_joined route, no scope token, no AS registration scopes |
| MSC4501 | ❌ ◐ | 0/0 | Social Media on Matrix | no m.social.* handling; the plumbing it rides on is generic and pre-existing |
| MSC4500 | ❌ ● | 0/0 | State accumulator endpoint and transaction digests | no LtHash accumulator, state_hashes payload or reconciliation endpoint |
| MSC4499 | 🟨 ◐ | 40/50 | Strict server signing key caching and key ID uniqueness | keys cached and persisted, but extend() overwrites so last seen wins |
| MSC4498 | ❌ ● | 0/0 | Holding messages pending moderation (via policy servers) | policy server client handles 200 and 400 only; no 202 M_HELD path |
| MSC4497 | ❌ ● | 0/0 | Filter state events by type in /state endpoint | /state returns full room state; no types query parameter |
| MSC4496 | ❌ ● | 0/0 | Calendar Events, Invites, and Availability in Matrix | no calendar rooms, RSVP handling or availability query endpoints |
| MSC4495 | ❌ ● | 0/0 | Selective Presence | no sharing config, no EDU recipients or stream_id, no query endpoint |
| MSC4494 | ❌ ● | 0/0 | Membership-based invite blocking | no deny_public action; msc4380 filter has no mutual-room evaluation |
| MSC4492 | ✅ ◐ | 90/100 | Clarify that pagination tokens are directionless | PduCount tokens already directionless and shared across the named endpoints |
| MSC4491 | ❌ ● | 0/0 | Invite reasons in room creation | createRoom passes reason None to invite; no invite_reason field |
| MSC4490 | ❌ ● | 0/0 | Surfacing room contacts when a join or knock does not succeed | no contact field on join or knock rejections; MSC4489 also absent |
| MSC4486 | ❌ ● | 0/0 | Listing delayed events with filtering &amp; pagination | MSC4140 delayed events absent entirely, so the list endpoint has no base |
| MSC4485 | ❌ ● | 0/0 | Homeserver feature controls | no .well-known/matrix/featured route |
| MSC4484 | ❌ ● | 0/0 | Server Administration OAuth Scope | no OAuth scope gating on admin endpoints; no msc4484 flag |
| MSC4480 | ❌ ● | 0/0 | MSC4480 Sliding Sync Extension: Sticky Events | no sticky events; ruma unstable-msc4480 feature not enabled |
| MSC4473 | ❌ ● | 0/0 | Proxied room alias resolution | No federation v2 query/directory; no signing/proxy logic. |
| MSC4472 | ❌ ● | 0/0 | Deprecated room version kind | No `deprecated` stability kind in room_versions capability. |
| MSC4470 | ❌ ● | 0/0 | Routing reports to non-local destinations | `/report` does not accept `must_send_to`; no fan-out. |
| MSC4469 | ❌ ● | 0/0 | Reporting to remote servers (EDU approach) | `m.report` EDU not defined or routed. |
| MSC4467 | ❌ ● | 0/0 | Improved Room Upgrade API | v3 upgrade only; no v4 endpoint, capability, or migration_schema. |
| MSC4466 | ✅ ● | 90/100 | Altering profile change propagation | profile.rs:20 resolve_propagation; key-copy rules met via build.rs:184 |
| MSC4464 | ❌ ● | 0/0 | verifiable links in profile | No /verify_profile_connection endpoint or verification backend |
| MSC4462 | ❌ ◐ | 10/10 | Links in Profile | Incidental MSC4133 passthrough; no m.connections parsing. |
| MSC4461 | ✅ ◐ | 100/100 | Storing per-message profiles for users | Pure account data passthrough; generic CS account-data covers it |
| MSC4458 | ✅ ◐ | 90/100 | Handling incoming JSON in the server-server API | Federation JSON parsed first; signatures checked on the parsed object |
| MSC4457 | ❌ ● | 0/0 | Generic reporting API | No /_matrix/client/v1/safety/report endpoint or harm handling |
| MSC4453 | ❌ ● | 0/0 | Deprecate old room versions | v3-v5 marked unstable; v6-v9 still stable; create/upgrade not gated |
| MSC4452 | ✅ ● | 100/100 | Preview URL capabilities API | capabilities.rs:89 advertises io.element.msc4452.preview_url |
| MSC4450 | ❌ ● | 0/0 | Identity Provider selection for User-Interactive Authentication with Legacy S... | UIAA SSO fallback derives idp from session, not idp_id query |
| MSC4449 | ❌ ● | 0/0 | Updated /members filtering | Single membership filter only; no array support, no mutual-exclusion error |
| MSC4448 | ❌ ● | 0/0 | Preview URL Site Logos | No matrix:site_logo or msc4448:site_logo in preview_url response |
| MSC4447 | ❌ ● | 0/0 | Move OpenID userinfo endpoint out of `/_matrix/federation` | Old /federation/v1/openid/userinfo present; new /_matrix/openid/v1/userinfo n... |
| MSC4446 | ❌ ● | 0/0 | Allow moving the fully read marker to older events | No allow_backward parsing; m.fully_read written with no order check |
| MSC4445 | ❌ ◐ | 0/0 | Clarify `/sync` timeline order | No msc4445 unstable_features flags advertised |
| MSC4440 | ❌ ● | 0/0 | Profile Biography via Global Profiles | Generic MSC4133 passthrough only; no m.biography validation |
| MSC4439 | 🟨 ● | 70/85 | Encryption key URIs in `/.well-known/matrix/support` | check.rs:780 validates URI and rejects raw keys; new https MUST unenforced |
| MSC4438 | ✅ ● | 100/100 | Message bookmarks via account data | Pure account-data convention; existing endpoints store arbitrary types |
| MSC4437 | ❌ ● | 0/0 | Endpoint to replace entire profile | No PUT /_matrix/client/v3/profile/{userId} replace-all endpoint |
| MSC4433 | ❌ ● | 0/0 | Image Packs and Room Upgrades | Room upgrade does not transfer m.room.image_pack or update m.image_pack.rooms |
| MSC4432 | ❌ ● | 0/0 | Server-wide room name overrides | No m.room.name.server_wide propagation; no capability |
| MSC4431 | ❌ ● | 0/0 | Personalised room name overrides | Server side passively allows m.room.name.private as account data |
| MSC4430 | ❌ ● | 0/0 | Member Keys | No member-key room version, no /member_key federation endpoint |
| MSC4429 | ❌ ● | 0/0 | Profile Updates for Legacy Sync | No users key in /sync v3 and no profile_fields filter; only v5 MSC4262 |
| MSC4428 | ❌ ● | 0/0 | Stable identifiers for Room Members | No member_info or unsigned.stable_id added to events or sync |
| MSC4427 | ❌ ● | 10/10 | Custom banners for user profiles | No m.banner_url handling; MSC4133 plumbing accepts it opaquely |
| MSC4426 | ✅ ● | 90/90 | User Status Profile Fields | Both budgets enforced with 400 M_TOO_LARGE; no rate limiting anywhere in the ... |
| MSC4425 | ❌ ● | 0/0 | Ephemeral media | no ephemeral query param; no DELETE on /_matrix/client/v1/media/.../.... |
| MSC4420 | ❌ ● | 0/0 | Duplicate one-time key error response for /keys/upload | add_one_time_key silently overwrites; no M_DUPLICATE_ONE_TIME_KEY emitted. |
| MSC4417 | ❌ ● | 0/0 | URL Previews via Appservices | client preview_url exists; no appservice fan-out or namespace check. |
| MSC4416 | ❌ ● | 0/0 | Optionally requiring policy server signatures in a room | depends on MSC4284; no policy-server signature checks anywhere. |
| MSC4413 | ✅ ◐ | 100/100 | Remove `private` join_rule | private join_rule treated as unknown; effective semantics already aligned. |
| MSC4406 | 🟨 ● | 50/55 | `M_SENDER_IGNORED` error code | src/api/client/{room/event.rs:74,context.rs:86,relations.rs:175}; M_SENDER_IG... |
| MSC4401 | ❌ ◐ | 0/0 | Publishing client capabilities via profiles | generic profile keys exist; logout cleanup of client_capability missing. |
| MSC4396 | ❌ ● | 0/0 | Inline linked media | no multipart/mixed event-with-media; no m.media mixin or M_GONE wired. |
| MSC4388 | ✅ ● | 90/100 | Secure out-of-band channel for sign in with QR | router.rs:471 five MSC4388 routes; TTL default 600s exceeds SHOULD 300s |
| MSC4387 | ❌ ● | 0/0 | `M_SAFETY` error code | M_SAFETY errcode not used anywhere in src/; no harms field handling. |
| MSC4383 | ✅ ● | 100/100 | Client-Server Discovery of Server Version | versions.rs:30 server object; :149 advertises net.zemos.msc4383 |
| MSC4382 | ❌ ● | 0/0 | Peppered hash verification for E2EE content moderation | No verification_hash check on report endpoint |
| MSC4371 | ❌ ● | 0/0 | On the elimination of federation transactions. | No PUT /_matrix/federation/v2/send/{eventId\|eduId} endpoint |
| MSC4370 | ❌ ● | 0/0 | Federation endpoint for retrieving current extremities | No /_matrix/federation/v1/extremities endpoint |
| MSC4369 | ❌ ● | 0/10 | M_CAPABILITY_NOT_ENABLED error code for when capability is not enabled on an ... | Endpoints exist but return M_FORBIDDEN/Unknown not M_CAPABILITY_NOT_ENABLED |
| MSC4368 | ❌ ● | 0/0 | Combine definitions of M_RESOURCE_LIMIT_EXCEEDED error code and m.server_noti... | M_RESOURCE_LIMIT_EXCEEDED unused; no limit_type field |
| MSC4367 | ❌ ● | 0/0 | via routes in the published room directory | PublishedRoomsChunk has no via field |
| MSC4366 | ❌ ● | 0/0 | Resident servers in and around the room directory | publicRooms not filtered to rooms with joined members |
| MSC4365 | ❌ ● | 0/0 | Canonical ignore list rooms | No ignored_user_list_rooms server-side filtering |
| MSC4363 | ❌ ● | 0/0 | OAuth step up authentication | No M_INSUFFICIENT_USER_AUTHENTICATION error or acr_values |
| MSC4361 | ✅ ● | 100/100 | Non-federating Membership Authorization Rule Amendments | src/service/rooms/state_res/event_auth/room_member.rs:56; reject m.room.membe... |
| MSC4360 | ❌ ● | 0/0 | Threads extension to Sliding Sync | No /thread_updates endpoint or threads sliding sync extension |
| MSC4359 | 🟨 ● | 50/50 | "Do not Disturb" notification settings | DnD account data passes through; device removal leaves the per device key |
| MSC4358 | ❌ ● | 0/0 | Out of room server discovery | No /discover_common_rooms federation endpoint |
| MSC4354 | ❌ ● | 0/0 | Sticky Events | No sticky object, sticky_duration_ms param, or sync sticky section |
| MSC4353 | ❌ ● | 0/0 | Per-origin linear chain | No origin_predecessor field or per-origin chain validation |
| MSC4352 | ❌ ● | 0/0 | Customizable HTTPS permalink base URLs via server discovery | No permalink_base_url in /.well-known/matrix/client output |
| MSC4351 | ✅ ● | 100/100 | Odd Context Limits | Context handler biases remainder to events_after via div_ceil(2) |
| MSC4350 | ❌ ● | 0/0 | Permitting encryption impersonation for appservices | No impersonator field in device keys, no /keys/query handling |
| MSC4348 | ❌ ● | 0/0 | Portable and serverless accounts in rooms | portable accounts (account keys); not implemented |
| MSC4345 | ❌ ● | 0/0 | Server key identity and room membership | server key as room identity; massive auth-rule changes; not implemented |
| MSC4344 | ❌ ● | 0/0 | Strike deprecated SRV service name. | deprecated _matrix._tcp SRV still queried |
| MSC4343 | ❌ ● | 0/0 | Making mass redactions use a new event type | m.room.redactions (mass redactions) event not used; depends on MSC2244 |
| MSC4342 | ❌ ● | 0/0 | Limiting the number of devices per user ID | 30-device limit and M_TOO_MANY_DEVICES not enforced |
| MSC4339 | ❌ ● | 0/0 | Allow the user directory to return full profiles | user_directory v4 with profile_fields not implemented |
| MSC4337 | ❌ ● | 0/0 | Appservice API to supplement user profiles | appservice profile supplement endpoint not queried |
| MSC4331 | ❌ ● | 0/0 | Device Account Data | per-device account data routes not implemented |
| MSC4330 | 🟨 ◐ | 50/50 | specify HTTP and TLS versions which must be supported | HTTP/2 via axum/hyper available; TLS 1.2+ via rustls; not enforced as MUST |
| MSC4325 | ❌ ● | 0/0 | Presence privacy | presence privacy filtering by m.presence_sharing_config not implemented |
| MSC4322 | ❌ ● | 0/0 | Simple Media Self-Redaction | [→ MSC3911?] media self-redaction; no /media/redact endpoint or EDU |
| MSC4320 | ❌ ● | 0/0 | Rich Presence | Rich Presence m.rpc; no support for activity/media profile field |
| MSC4309 | ❌ ● | 0/0 | Finalised delayed events on sync | finalised delayed events on /sync; depends on MSC4140; no impl |
| MSC4308 | ❌ ● | 0/0 | Thread Subscriptions extension to Sliding Sync | No thread_subscriptions sliding sync extension; MSC4306 store absent |
| MSC4306 | ❌ ● | 0/0 | Thread Subscriptions | No thread subscription endpoints, push condition or postcontent kind |
| MSC4305 | ❌ ● | 0/0 | Pushed Authorization Requests (PARs) for OAuth authentication | OIDC auth_metadata lacks PAR endpoint fields |
| MSC4303 | ❌ ● | 0/0 | Disallowing non-compliant user IDs in rooms | no future room version banning non-compliant user IDs |
| MSC4298 | ❌ ● | 0/0 | Room version components for 'Redact on ban' | no future room version protecting redact_events from redaction |
| MSC4293 | ❌ ● | 0/0 | Redact on kick/ban | MSC4293 commit lives only on Continuwuity branches; current tree has no redac... |
| MSC4282 | ❌ ● | 0/0 | Hint that a /rooms/{room_id}/messages request is interactive | no interactive query parameter on /messages |
| MSC4279 | ❌ ● | 0/0 | Server notice rooms | no notice room presets, no leave_rules, no server_notice room type filter |
| MSC4271 | ❌ ◐ | 0/0 | Recommended enabled-ness for default push rules | no admin override knob; uses Ruma defaults verbatim |
| MSC4266 | 🟨 ● | 75/75 | Policies in /.well-known/matrix/support | multi-lang policies reachable from config; wire key not unstable-prefixed |
| MSC4265 | ❌ ◐ | 10/10 | Data Protection Officer contact in /.well-known/matrix/support | support_role configurable; MSC role string accepted as Custom |
| MSC4264 | ❌ ● | 0/0 | Tokens for Contacting Accounts or Joining Semi-Public Rooms | Tokens for contact / semi-public-room joins not implemented |
| MSC4263 | ❌ ◐ | 10/10 | Preventing MXID enumeration via key queries | MUST floor met implicitly; MAY restriction unused |
| MSC4262 | 🟨 ● | 60/60 | Sliding Sync Extension: Profile Updates | Change-log-backed extension; no lists/rooms scoping, drop signal or lazy_members |
| MSC4261 | ✅ ◐ | 80/100 | "Do not encrypt for device" flag | DeviceKeys stored and served raw, so do_not_encrypt survives upload and query |
| MSC4259 | ❌ ● | 0/0 | Profile Update EDUs for Federation | m.profile EDU broadcast not implemented |
| MSC4258 | ❌ ● | 0/0 | Federated User Directory | Federated user_directory/search not implemented |
| MSC4257 | ❌ ● | 0/0 | Profiles Arent Auth: Move profile contents to a separate event | m.room.member.profile separate event not supported |
| MSC4256 | ❌ ● | 0/0 | RFC 9420 MLS mode Matrix | MLS mode rooms not implemented |
| MSC4255 | ❌ ● | 0/0 | Bulk Profile Updates | Bulk PUT/PATCH /profile not implemented |
| MSC4250 | ❌ ● | 0/0 | Authenticated media v2 (Cookie authentication for Client-Server API) | set_auth_cookie media auth not implemented |
| MSC4249 | ✅ ● | 100/100 | Removal of legacy media endpoints | allow_legacy_media defaults to false; legacy disabled |
| MSC4247 | ❌ ◐ | 10/10 | User Pronouns | MSC4133 generic profile fields cover m.pronouns transparently |
| MSC4246 | ❌ ● | 0/0 | Sending to-device messages as/to a server | Empty-localpart server addressing for to-device absent |
| MSC4245 | ❌ ● | 0/0 | Immutable encryption algorithm | encryption_algorithm in m.room.create not honored |
| MSC4244 | ❌ ● | 0/0 | RFC 9420 MLS for Matrix | MLS for Matrix not implemented |
| MSC4243 | ❌ ● | 0/0 | User ID localparts as Account Keys | Account keys / federation query/accounts not implemented |
| MSC4242 | ❌ ● | 0/0 | State DAGs | State DAGs not implemented; uses standard auth chain |
| MSC4235 | ❌ ● | 0/0 | `via` query param for hierarchy endpoint | hierarchy endpoint lacks via query parameter |
| MSC4234 | ❌ ● | 0/0 | Update app badge counts when rooms are read | cleared_notifs read-receipt flag not handled |
| MSC4233 | ❌ ● | 0/0 | Remembering which server a user knocked through | Knocked-through server not remembered; leave and rejection paths unchanged |
| MSC4232 | ❌ ● | 0/0 | Attribute-Based Access Control (ABAC) | ABAC permissions model; no room version implements it |
| MSC4228 | ❌ ● | 0/0 | Search Redirection | optional 403 search redirection not used |
| MSC4226 | ❌ ● | 0/0 | Reports as rooms | reports-as-rooms (m.report room type) not implemented |
| MSC4224 | ❌ ● | 0/0 | CBOR Serialization | application/cbor content negotiation not implemented |
| MSC4223 | ❌ ● | 0/0 | Error code for disallowing threepid unbinding | 3pid unbind/delete endpoints not implemented at all |
| MSC4218 | ❌ ● | 0/0 | Improving performance of profile changes | synthetic events / m.room.user_profile not implemented |
| MSC4208 | 🟨 ◐ | 40/50 | Adding User-Defined Custom Fields to User Global Profiles | MSC4133 fields carry u.* generically; no 512B cap, no disallowed list |
| MSC4207 | ❌ ● | 0/0 | Media identifier moderation policy | m.policy.rule.mxc not interpreted |
| MSC4204 | ❌ ● | 0/0 | `m.takedown` moderation policy recommendation | no m.takedown recommendation handling |
| MSC4203 | ✅ ● | 95/100 | Sending to-device events to appservices | to_device in AS txns with to_user_id/to_device_id; retain (dual /sync) |
| MSC4201 | ❌ ● | 0/10 | Profiles as Rooms v2 | only generic /profile/{user} exists; no roomID profile lookup |
| MSC4198 | ❌ ● | 0/0 | Usage of OIDC login_hint | login_hint not parsed on authorization, device or account-management paths |
| MSC4196 | ❌ ● | 0/0 | MatrixRTC voice and video calling application `m.call` | m.rtc.slot not replicated on room upgrade; slot and intent are client scope |
| MSC4195 | 🟨 ● | 10/10 | MatrixRTC Transport using LiveKit Backend | MSC4143 transport advertised; no get_token, derivation, salt or delayed leave |
| MSC4194 | ❌ ● | 0/0 | Batch redaction of events by sender within a room (including soft failed events) | POST /rooms/{}/redact/user/{} not wired |
| MSC4188 | ❌ ● | 0/0 | Handling HTTP 410 Gone Status in Matrix Server Discovery | 410 Gone not specially handled in well-known resolver |
| MSC4185 | ❌ ● | 0/0 | Event Visibility API | no can_user_see_event endpoint |
| MSC4179 | ❌ ● | 0/0 | Moderation event hiding | No m.moderation_hidden; edit fold demands same sender and type as the parent |
| MSC4177 | ❌ ● | 0/0 | Add upload location hints proposal | no m.upload.locations or location query param |
| MSC4176 | ❌ ● | 0/0 | Translatable Errors | no localized error messages map |
| MSC4174 | ❌ ● | 0/0 | Web push | No webpush pusher kind, no /pushers/ack endpoint, no VAPID capability |
| MSC4173 | ❌ ● | 0/0 | test pusher | no /pushers/push test endpoint |
| MSC4168 | 🟨 ● | 30/40 | Update `m.space.*` state on room upgrade | src/api/client/room/upgrade.rs:447; copies m.space.parent always plus m.space... |
| MSC4167 | ❌ ● | 0/0 | Copy bans on room upgrade | bans not copied during room upgrade |
| MSC4166 | ✅ ● | 100/100 | Specify `/turnServer` response when no TURN servers are available | turnServer returns 404 M_NOT_FOUND when no TURN URIs configured |
| MSC4165 | ✅ ● | 100/100 | Remove own power level on deactivation | power level entry removed for self on deactivation |
| MSC4164 | ✅ ● | 100/100 | Leave all rooms on deactivation | deactivation leaves all joined/invited/knocked rooms |
| MSC4162 | ❌ ◐ | 10/10 | One-Time Key Reset Endpoint | no /keys/reset; claim ordering is implicit via key prefix iter |
| MSC4155 | ✅ ● | 90/100 | Invite filtering | Six glob lists evaluated per sender; blanket block outranks the ignore list |
| MSC4154 | ✅ ● | 100/100 | Request max body size | max_request_size default 24MB, M_TOO_LARGE returns 413 |
| MSC4152 | ❌ ● | 0/0 | Room labeling and filtering | room labels and /rooms/{roomId}/labels not implemented |
| MSC4149 | 🟨 ◐ | 80/80 | Update CSP Directives for Media Repository | global CSP aligns with MSC; missing font-src and script-src 'none' |
| MSC4145 | ❌ ● | 0/0 | Simple verified accounts | m.verified profile field and endpoint not implemented |
| MSC4143 | 🟨 ◐ | 50/60 | MatrixRTC | GET rtc/transports served; slots and sticky membership are client and MSC4354 |
| MSC4141 | ❌ ● | 0/0 | Time based notification filtering | time_and_day push rule condition not supported |
| MSC4140 | ❌ ● | 0/0 | Cancellable delayed events | No delayed_event routes, no m.delayed_events capability, no delay_id anywhere |
| MSC4136 | ❌ ● | 0/0 | Shared retry hints between servers | retry_hints in /send_join response not implemented |
| MSC4128 | ✅ ● | 100/100 | Error on invalid auth where it is optional | invalid token returns error even on optional auth endpoints |
| MSC4125 | ✅ ● | 90/100 | Specify servers to join via for federated invites | federation invite via field used both inbound and outbound |
| MSC4121 | ✅ ● | 100/100 | `m.role.moderator` `/.well-known/matrix/support` role. | m.role.moderator served via Ruma ContactRole alias and config |
| MSC4120 | ❌ ● | 0/0 | Allow `HEAD` on `/download` | HEAD on /download not wired; routes mounted via Ruma metadata GET only |
| MSC4117 | ❌ ● | 0/0 | Reinstating Events (Reversible Redactions) | m.room.reinstate (reversible redactions) not implemented |
| MSC4109 | ❌ ● | 0/0 | Appservices &amp; soft-failed events | appservice v2/transactions endpoint with soft-failed events absent |
| MSC4108 | ✅ ● | 90/90 | Mechanism to allow OAuth 2.0 API sign in and E2EE set up via QR code | Device-authorization login served; secure channel now delegated to MSC4388 |
| MSC4106 | ❌ ● | 0/0 | Join as Muted | join-as-muted default_membership not implemented |
| MSC4104 | ❌ ● | 0/0 | Auth Lock: Soft-failure-be-gone! | m.auth_lock event and auth-rule not implemented |
| MSC4103 | ❌ ◐ | 0/0 | Make threaded read receipts opt-in in /sync | threaded_read_receipts sync filter not implemented |
| MSC4102 | ❌ ◐ | 0/0 | Clarifying precedence in threaded and unthreaded read receipts in EDUs | unthreaded-takes-precedence aggregation rule not enforced |
| MSC4100 | ❌ ● | 0/0 | Scoped signing keys | scoped signing keys / X-Matrix-Scoped not implemented |
| MSC4096 | ❌ ● | 0/0 | Proposal to make forceTurn option configurable server-side | forceTurn not advertised in well-known |
| MSC4094 | ❌ ● | 0/0 | Sync Server and Client Times with endpoint | GET /_matrix/client/v3/get_server_now endpoint missing |
| MSC4089 | ❌ ● | 0/0 | Delivery Receipts | m.delivery receipts not implemented |
| MSC4086 | ❌ ● | 0/0 | Event media reference counting | event-media reference counting not implemented |
| MSC4084 | ❌ ● | 0/0 | Improving security of MSC2244 | v4 send endpoint with UIA for redactions not implemented |
| MSC4083 | ❌ ● | 0/0 | Delta-compressed E2EE file transfers | delta-compressed media transfers not implemented |
| MSC4081 | ❌ ● | 0/0 | Eagerly sharing fallback keys with federated servers | eager fallback key sharing not implemented |
| MSC4080 | ❌ ● | 0/0 | Cryptographic Identities (Client-Owned Identities) | cryptographic identities/send_pdus endpoint not implemented |
| MSC4079 | ❌ ● | 0/0 | Server-Defined Client Landing Pages | landing_page in well-known not implemented |
| MSC4078 | ❌ ● | 0/0 | Registering pushers against push notification services should forward back fa... | upstream_errcode/upstream_error not surfaced from /pushers/set |
| MSC4076 | 🟨 ● | 60/100 | Let E2EE clients calculate app badge counts themselves (disable_badge_count) | disable_badge_count honored when sending push notifications |
| MSC4074 | ❌ ● | 0/0 | Server side annotation aggregation | server-side annotation aggregation not implemented |
| MSC4071 | ❌ ● | 0/0 | Pagination Token Headers | No X-Matrix-Pagination-* header handling. |
| MSC4062 | 🟨 ◐ | 50/50 | Add a push rule tweak to disable email notification | Custom tweak round trips through ruma; no email pusher to honour dont_email |
| MSC4060 | ❌ ● | 0/0 | Accept room rules before speaking | No m.room.rules state event or acceptance gating. |
| MSC4057 | ❌ ● | 0/0 | Static Room Aliases | No .well-known/matrix/rooms lookup before federation directory. |
| MSC4056 | ❌ ● | 0/0 | Role-Based Access Control (mk II) | No m.role / m.role_map RBAC support. |
| MSC4053 | ❌ ◐ | 0/0 | Extensible Events - Mentions mixin | No mixin push rules with room_version_supports condition. |
| MSC4049 | ❌ ● | 0/0 | Sending events as a server or room | No room version permitting non-user-ID senders. |
| MSC4048 | ❌ ● | 0/0 | Authenticated key backup | No m.backup.v2.curve25519-aes-sha2 algorithm or backup_mac handling. |
| MSC4047 | ❌ ● | 0/0 | Send Keys | No m.room.send_key state event or send-key auth path. |
| MSC4046 | ❌ ● | 0/0 | Make &amp; send PDU endpoints | None of the four make_pdu/send_pdu endpoints implemented. |
| MSC4045 | ❌ ● | 0/0 | Deprecating the use of IP addresses in server names | No room version banning IP-literal server names. |
| MSC4044 | ❌ ● | 0/0 | Enforcing user ID grammar in rooms | No room version enforcing strict user ID grammar. |
| MSC4043 | ❌ ● | 0/0 | Presence Override API | No /presence/{userId}/override endpoint. |
| MSC4042 | ❌ ● | 0/0 | Disabled Presence State | No fourth presence state; Tuwunel serves online, offline, unavailable, busy |
| MSC4038 | ❌ ● | 0/0 | Key backup for MLS | No MLS or m.dmls_backup.v1.aes-hmac-sha2 backup algorithm support. |
| MSC4034 | ❌ ● | 0/0 | Media limits | No /usage endpoint and no m.storage.* fields in /config. |
| MSC4033 | ❌ ● | 0/0 | Explicit ordering of events for receipts | No order field on events or receipts. |
| MSC4031 | ❌ ● | 0/0 | Pre-generating invites and room invite codes | pre-generated invites and m.room.invite state event not implemented |
| MSC4029 | 🟨 ◐ | 40/50 | Fixing `X-Matrix` request authentication | X-Matrix verification covers basics; canonicalization rules not fully specified |
| MSC4028 | ❌ ● | 0/0 | Push all encrypted events except for muted rooms | No .m.rule.encrypted_event override in the default ruleset Tuwunel serves |
| MSC4027 | ❌ ● | 0/0 | Custom Images in Reactions | shortcode 100-byte limit unenforced on send; rendering itself is client scope |
| MSC4023 | ❌ ● | 0/0 | Thread ID for second-order relation | unsigned.thread_id not added to events |
| MSC4021 | ❌ ● | 0/0 | Archive client controls | m.room.archive_controls not relayed in /publicRooms |
| MSC4020 | ❌ ● | 0/0 | Room model configuration | m.room.create model object flagging not supported |
| MSC4019 | ❌ ● | 0/0 | Encrypted event relationships | m.room.relationship_encryption flag not handled by server |
| MSC4016 | ❌ ● | 0/0 | Streaming and resumable E2EE file transfer with random access | No tus, no Range, no incremental serve; MSC2246 async upload still buffers |
| MSC4014 | ❌ ● | 0/0 | Pseudonymous Identities | pseudonymous identities (sender_key, mxid_mapping) not implemented |
| MSC4011 | ❌ ● | 0/0 | Thumbnail media negotiation | thumbnail Accept header negotiation not implemented |
| MSC4005 | ❌ ◐ | 0/0 | Explicit read receipts for sent events | Server does not auto-generate read receipt on send |
| MSC4002 | ❌ ● | 0/0 | Walkie talkie | No server side speaker lock; walkie, ptt, floor lock absent from src and ruma |
| MSC4001 | ❌ ● | 0/0 | Return start of room state at context endpoint | context returns state at LAST event, MSC asks for state at FIRST |
| MSC4000 | ❌ ● | 0/0 | Forwards fill (`/backfill` forwards) | forwards_fill federation endpoint not implemented |
| MSC3999 | ❌ ● | 0/0 | Add causal parameter to `/timestamp_to_event` | timestamp_to_event causal event_id parameter not supported |
| MSC3998 | ❌ ● | 0/0 | Add timestamp massaging to `/join` and `/knock` | join/knock ts query param not honored |
| MSC3997 | ❌ ● | 0/0 | Add timestamp massaging to `/createRoom` | createRoom ts query param not honored (always timestamp: None) |
| MSC3996 | ❌ ● | 0/0 | Encrypted mentions-only rooms | m.has_mentions cleartext flag and is_encrypted_mention rule not present |
| MSC3995 | ❌ ● | 0/0 | Linearized Matrix | Linearized Matrix hub/participant architecture not implemented |
| MSC3994 | ❌ ● | 0/0 | Display why an event caused a notification | rule_kind/rule_id not added to /notifications |
| MSC3993 | ❌ ● | 0/0 | Room takeover | room takeover variants not implemented |
| MSC3991 | ❌ ● | 0/0 | Power level up! Taking the room to new heights | raise own power level above max not allowed |
| MSC3984 | 🟨 ● | 75/80 | Sending key queries to appservices | src/api/client/keys/get_keys.rs:178 appservice device overlay; no cross-signi... |
| MSC3983 | ✅ ● | 85/100 | Sending One-Time Key (OTK) claims to appservices | src/api/client/keys/claim_keys.rs:92 local OTK, appservice, fallback; interes... |
| MSC3982 | ❌ ● | 0/0 | Limit maximum number of events sent to an AS | no 100-event cap on appservice transactions |
| MSC3964 | ❌ ● | 0/0 | Notifications for room tags | room_tag push condition not implemented |
| MSC3963 | ❌ ● | 0/0 | Oblivious Matrix over HTTPS | Oblivious MoH endpoints absent |
| MSC3960 | ✅ ● | 90/100 | Sliding Sync Extension: Receipts | sliding sync receipts extension implemented |
| MSC3959 | ✅ ● | 90/100 | Sliding Sync Extension: Account Data | sliding sync account_data extension implemented |
| MSC3947 | ❌ ● | 0/0 | Allow Clients to Request Searching the User Directory Constrained to Only Hom... | exclude_sources parameter on user_directory/search not implemented. |
| MSC3946 | ❌ ● | 0/0 | Dynamic room predecessor | m.room.predecessor state event not handled. |
| MSC3944 | ❌ ● | 0/0 | Dropping stale send-to-device messages | Stale-to-device cancellation/dedup logic not implemented. |
| MSC3934 | ❌ ● | 0/0 | Bulk push rules change endpoint | PUT /pushrules_bulk/.../actions and /enabled endpoints not implemented. |
| MSC3933 | ❌ ● | 0/0 | Core push rules for Extensible Events | Extensible-event default underride push rules not added. |
| MSC3932 | ❌ ● | 0/0 | Extensible events room version push rule feature flag | Extensible-event room version push rule gating not enabled. |
| MSC3931 | ❌ ● | 0/0 | Push rule condition for room version features | room_version_supports push condition not enabled in tuwunel. |
| MSC3926 | ❌ ● | 0/0 | Disable server-default notifications for bot users by default | enable_predefined_push_rules registration body field not implemented. |
| MSC3922 | ❌ ● | 0/0 | Removing SRV records from homeserver discovery | SRV record discovery still active; would need code removal. |
| MSC3917 | ❌ ● | 0/0 | Cryptographically Constrained Room Membership | Cryptographic membership (RRK / RSK / signed memberships) not implemented. |
| MSC3914 | ❌ ● | 0/0 | Matrix native group call push rule | .m.rule.room.call push rule + call_started condition not implemented. |
| MSC3911 | ❌ ● | 0/0 | Linking media to events | attach_media query, /media/copy, restrictions block in federation media not p... |
| MSC3909 | ❌ ● | 0/0 | Membership based mutes | Membership-based mutes via new mute/leave-mute states; not implemented. |
| MSC3901 | ❌ ◐ | 0/0 | Deleting State | meta-MSC of sub-proposals; obsolete-state cleanup not implemented |
| MSC3896 | ❌ ● | 0/0 | Appservice media | appservice media namespace not implemented |
| MSC3895 | ❌ ● | 0/0 | Federation API Behaviour of Partial-State Resident Servers | M_UNABLE_DUE_TO_PARTIAL_STATE error code not implemented |
| MSC3890 | ✅ ● | 100/100 | Remotely silence local notifications | device removal deletes local_notification_settings via MSC3391 tombstone |
| MSC3885 | 🟨 ● | 70/80 | Sliding Sync Extension: To-Device | to_device extension uses its own opaque since token in v5 sync |
| MSC3884 | ✅ ● | 90/100 | Sliding Sync Extension: E2EE | sliding sync e2ee extension implemented |
| MSC3883 | ❌ ● | 0/0 | Fundamental state changes | draft proposal, no concrete API; would require new room version |
| MSC3881 | ❌ ● | 0/0 | Remotely toggling push notifications for another client | pusher enabled and device_id fields not exposed |
| MSC3874 | ❌ ● | 0/0 | MSC3874 Loading Messages excluding Threads | rel_types and not_rel_types filters absent; org.matrix.msc3874 not advertised |
| MSC3872 | ❌ ◐ | 0/0 | Order of rooms in Spaces | manual room ordering in spaces; vague proposal, no API defined |
| MSC3871 | ❌ ● | 0/0 | Gappy timeline | No gaps field on /messages and no org.matrix.msc3871 versions flag |
| MSC3870 | ❌ ● | 0/0 | Async media upload extension: upload to URL | upload_url field and /complete endpoint not implemented |
| MSC3865 | ✅ ● | 100/100 | User-given attributes for users | client-side; uses generic account_data endpoints already implemented |
| MSC3864 | ✅ ● | 100/100 | User-given attributes for rooms | client-side; uses generic account_data endpoints already implemented |
| MSC3851 | ❌ ● | 0/0 | Allow custom room presets when creating a room | only standard RoomPreset variants accepted; no custom string presets |
| MSC3848 | ❌ ● | 0/0 | Introduce errcodes for specific event sending failures. | no M_INSUFFICIENT_POWER/M_NOT_JOINED/M_ALREADY_JOINED errcodes emitted |
| MSC3847 | ❌ ● | 0/0 | Ignoring invites with policy rooms | no policy room handling for m.policies account data |
| MSC3843 | ❌ ● | 0/0 | Reporting content over federation | federation /rooms/{}/report/{} endpoint not implemented |
| MSC3840 | ❌ ◐ | 0/0 | Ignore invites | client-side ignored invites account data; no server behavior required |
| MSC3839 | ❌ ● | 0/0 | primary-identity-as-key | No public-key identity or challenge login; five login types, none key-based |
| MSC3837 | ❌ ● | 0/0 | Cascading profile tags for push rules | no profile_tags array; only single profile_tag handled |
| MSC3834 | ❌ ● | 0/0 | Opportunistic user key pinning (TOFU) | TOFU signing key is client-side; no server hooks |
| MSC3814 | 🟨 ● | 75/90 | Dehydrated devices with SSSS | All four routes wired; events endpoint is the deprecated POST, not the GET |
| MSC3767 | ❌ ● | 0/0 | Time based notification filtering | time_and_day push condition not present |
| MSC3759 | ❌ ● | 0/0 | Leave event metadata for deactivated users | deactivation leaves omit m.deactivated metadata |
| MSC3744 | ❌ ● | 0/0 | Support for flexible authentication | no flexible-auth /register or /account/authenticator endpoints |
| MSC3726 | ❌ ● | 0/0 | Safer Password-based Authentication with BS-SPEKE | open MSC; no BS-SPEKE login/register/password flows |
| MSC3723 | ❌ ● | 0/0 | Federation `/versions` | open MSC; no /_matrix/federation/versions endpoint |
| MSC3713 | ❌ ● | 0/0 | Alleviating ACL exhaustion with ACL Slots | open MSC; no ACL slot state-key handling |
| MSC3682 | ❌ ● | 0/0 | Sending Account Data to Application Services | AS transactions do not include account_data field |
| MSC3673 | ❌ ● | 0/0 | Encrypting ephemeral data units | branch MSC; no encrypted EDU envelope support |
| MSC3672 | ❌ ● | 0/0 | Sharing ephemeral streams of location data | branch MSC; no m.beacon EDU support or location streaming |
| MSC3664 | ✅ ● | 80/88 | Pushrules for relations | reply default, relation eval, rule repair; no insert-time lone-key reject |
| MSC3644 | 🟨 ● | 50/100 | Extensible Events: Edits and replies | Edits fold with no m.new_content, but a type-changing edit is dropped |
| MSC3618 | ❌ ◐ | 0/0 | Simplify federation `/send` response | branch MSC; tuwunel returns full pdus map per current spec |
| MSC3593 | ❌ ● | 0/0 | Safety Controls through a generic Administration API | none of the proposed /admin/* endpoints exist; tuwunel uses admin room |
| MSC3571 | ❌ ● | 0/0 | Aggregation pagination | no /aggregations endpoint; no aggregation pagination |
| MSC3570 | ❌ ◐ | 0/0 | Relation history visibility changes | no special history visibility for relations; new room version needed |
| MSC3547 | ❌ ● | 0/0 | Allow appservice bot user to read any rooms the appservice is part of | members.rs:77 TODO stands; an appservice must still masquerade or be joined |
| MSC3488 | ❌ ◐ | 10/10 | m.location: Extending events with location data | location event types pass through; no m.tile_server in well-known |
| MSC3480 | ❌ ◐ | 10/20 | Make device names private | allow_device_name_federation config gates device name exposure |
| MSC3469 | ❌ ○ | 0/0 | Mandate HTTP Range on Content Repository Endpoints | depends on object_store / hyper response writer for ranges |
| MSC3468 | ❌ ● | 0/0 | MXC to Hashes | no MXC-to-hash endpoints; no /clone or /hash routes |
| MSC3414 | ❌ ● | 0/0 | Encrypted state events | no encrypted state event handling or encrypted_state in publicRooms |
| MSC3395 | ❌ ● | 0/0 | Synthetic Appservice Events | no synthetic appservice events emitted on register/login/logout |
| MSC3394 | ❌ ● | 0/0 | New auth rule that only allows someone to post a message in relation to anoth... | no auth rule restricting top-level vs threaded messages |
| MSC3389 | ❌ ● | 0/0 | Redaction changes for events with a relation | no m.relates_to preservation in redactions |
| MSC3386 | ❌ ● | 0/0 | Unified Join Rules | no unified allow_join/allow_knock; no new room version |
| MSC3385 | 🟨 ◐ | 30/40 | Bulk small improvements to room upgrades | upgrade copies fixed list of state, not all m.* state nor account_data |
| MSC3361 | ❌ ● | 0/0 | Opportunistic Direct Push | no direct pusher kind or notifications in sync |
| MSC3360 | ❌ ● | 0/0 | Server Status | no /server/status endpoint or m.server.status event |
| MSC3359 | ❌ ● | 0/0 | Delayed Push | no jitter pusher field; not advertised in versions |
| MSC3356 | ❌ ● | 0/0 | Add additional OpenID user info fields | openid userinfo returns only sub |
| MSC3338 | ❌ ● | 0/0 | Adding iframe specifics to preview json | url preview has no iframe/oEmbed support |
| MSC3325 | ❌ ● | 0/0 | Upgrading invite-only rooms | upgrade does not switch invite-only rooms to restricted |
| MSC3277 | ❌ ● | 0/0 | Scheduled messages | no scheduled-message at= query param support |
| MSC3269 | ❌ ● | 0/0 | An error code for busy servers | no M_SERVER_BUSY error code |
| MSC3262 | ❌ ● | 0/0 | aPAKE authentication | SRP6a aPAKE login/registration not implemented |
| MSC3219 | ❌ ● | 0/0 | Space Flair | space flair events and member flag not implemented |
| MSC3216 | ❌ ● | 0/0 | Synchronized access control for Spaces | space-level synchronized PL replication absent |
| MSC3214 | ✅ ◐ | 90/100 | Allow overriding `m.room.power_levels` using `initial_state` | initial_state PL effectively replaces default via later append |
| MSC3202 | ✅ ● | 88/95 | Encrypted Appservices | device_lists.changed, OTK counts, fallback types under org.matrix.msc3202 |
| MSC3192 | ❌ ● | 0/0 | Batch state endpoint | batch_state endpoint not implemented |
| MSC3189 | ❌ ● | 0/0 | Per-room/per-space profiles | per-room/space scoped profile API not implemented |
| MSC3144 | ❌ ● | 0/0 | Allow Widgets By Default in Private Rooms | private_chat preset does not lower widgets PL |
| MSC3079 | ❌ ● | 0/0 | Low Bandwidth Client-Server API | branch; no CoAP/CBOR/DTLS support |
| MSC3051 | ❌ ◐ | 0/0 | A scalable relation format | open; m.relations array not handled |
| MSC3038 | ❌ ● | 0/0 | Typed Typing Notifications | branch; no events field on typing |
| MSC3032 | ❌ ◐ | 20/20 | Thoughts on updating presence | effective presence; busy supported, profile-as-rooms absent |
| MSC3020 | ❌ ◐ | 0/0 | Support for private federation networks | branch; same proposal as MSC3018, not implemented |
| MSC3018 | ❌ ◐ | 0/0 | Support for private federation networks | branch; no m.networks capability or network query |
| MSC3015 | ✅ ● | 100/100 | Room state personal overrides | override.* room account data rides the generic per-room endpoint and sync |
| MSC3014 | ❌ ● | 0/0 | HTTP Pushers for the full event with extra rooms information | open; no full_event_with_rooms pusher format |
| MSC2970 | 🟨 ◐ | 40/50 | Remove pusher path requirement | path/scheme constraints relaxed; lacks fragment/userinfo/8000-char checks |
| MSC2962 | ❌ ● | 0/0 | Managing power levels via Spaces | no auto_users or m.room.power_level_mappings handling |
| MSC2961 | ❌ ◐ | 0/10 | External Signatures | endpoint accepts arbitrary signature keys; object form discarded |
| MSC2943 | ❌ ● | 0/0 | Return an event ID for membership endpoints | membership endpoint responses lack event_id |
| MSC2938 | ❌ ● | 0/10 | Report content to moderators | target field and room_moderators routing not implemented |
| MSC2923 | ❌ ◐ | 0/0 | Matrix to Matrix connections | speculative idea-stage; no concrete API |
| MSC2895 | 🟨 ● | 20/30 | Improving the way membership lists are queried | membership filter present but single valued; no /rooms, joined_* not deprecated |
| MSC2883 | ❌ ● | 0/0 | [WIP] Matrix-flavoured MLS | WIP MLS; no DMLS support |
| MSC2882 | ❌ ◐ | 0/0 | [WIP] Tempered Transitive Trust | WIP; new public_user_signing key, m.device.signature EDU not implemented |
| MSC2855 | ❌ ◐ | 0/0 | Server-Initiated Client Clear-Cache &amp; Reload | no clear-cache signal mechanism |
| MSC2848 | ❌ ● | 0/10 | Globally unique event IDs | only legacy GET /event/:eventId; new room-scoped path absent |
| MSC2846 | ❌ ● | 0/0 | Decentralizing media through CIDs | open; CID-based MXC URLs not implemented |
| MSC2845 | ❌ ◐ | 0/5 | Thirdparty Lookup API for Telephone Numbers | src/api/client/thirdparty.rs returns empty protocols TODO |
| MSC2836 | ❌ ● | 0/0 | Threading | advertises org.matrix.msc2836 in /versions but no event_relationships |
| MSC2828 | ❌ ◐ | 0/0 | Proposal to restrict allowed user IDs over federation | no extended_user_id_char auth rule restriction |
| MSC2815 | ✅ ◐ | 90/100 | Proposal to allow room moderators to view redacted event content | include_unredacted_content honored; admin or redact PL gates access |
| MSC2812 | ❌ ● | 0/0 | Role-based power structures | role-based power proposal still draft; no m.role events |
| MSC2802 | ❌ ● | 0/0 | Full Room Abstraction | open meta proposal to redesign spec; not implementable as-is |
| MSC2787 | ❌ ● | 0/0 | Portable Identities | no UPK/UDK/attestation infrastructure |
| MSC2785 | ❌ ● | 0/0 | Event notification attributes and actions | no notification_attribute_data or notifications_profile endpoints |
| MSC2782 | ✅ ● | 90/90 | Pushers with the full event content | full_event opt-in works; membership absent, user_is_target now correct |
| MSC2772 | ❌ ◐ | 0/0 | Notifications for Jitsi Calls | no .m.jitsi default underride push rules |
| MSC2757 | ❌ ● | 0/0 | Sign Events | No event_signing key type; no client signature plumbing |
| MSC2755 | ❌ ● | 0/0 | Lazy load rooms | No room_limit_by_complexity filter handling |
| MSC2753 | ❌ ● | 0/0 | Peeking via Sync (Take 2) | No /peek or /unpeek; no peek section in sync |
| MSC2749 | ❌ ● | 0/0 | Per-user E2EE on/off setting | No m.encryption capability; no force/preference logic |
| MSC2730 | ❌ ● | 0/0 | Verifiable forwarded events | No /forward/{targetRoomId}; no signature validation |
| MSC2716 | ❌ ● | 0/0 | Incrementally importing history into existing rooms | No /batch_send; no m.room.insertion/batch/marker handling |
| MSC2695 | 🟨 ● | 40/40 | Get event by ID over federation | Federation /event exists; no client /events/{eventId} revival |
| MSC2673 | ❌ ● | 0/0 | Notification Levels | No notification_levels concept; push rules used |
| MSC2596 | ❌ ◐ | 0/0 | Proposal to always allow rescinding invites | Vendor room version net.maunium.msc2596 not registered; no rescind exception ... |
| MSC2513 | ❌ ◐ | 0/10 | Allow clients to specify content for membership events | Membership endpoints accept reason only; no content body param |
| MSC2499 | 🟨 ◐ | 10/30 | Fixes for Well-known URIs | src/service/resolver/well_known.rs follows redirects; 12288B cap; uses /versions |
| MSC2487 | ❌ ◐ | 0/0 | Filtering for Appservices | No filter field on appservice registration |
| MSC2477 | ❌ ◐ | 0/0 | User-defined ephemeral events in rooms | No PUT /rooms/{roomId}/ephemeral/{type}/{txnId} route |
| MSC2448 | 🟨 ● | 70/80 | Using BlurHash as a Placeholder for Matrix Media | blurhash on profile, federation query, media upload, member events |
| MSC2444 | 🟨 ● | 30/30 | Proposal for implementing peeking over federation (peek API) | world_readable allowed on some federation reads; no /peek subscription API |
| MSC2438 | ❌ ● | 0/10 | Local and Federated User Erasure Requests | deactivate present but no erase param, no fed/AS erase endpoints |
| MSC2437 | ✅ ◐ | 100/100 | Store tagged events in Room Account Data | m.tagged_events stored via existing room account_data routes |
| MSC2413 | ❌ ● | 0/0 | Remove client_secret | requestToken routes served, but client_secret stays required by the ruma type |
| MSC2391 | ❌ ● | 0/0 | Federation point-queries. | No federation point-query state endpoint |
| MSC2388 | ❌ ● | 0/0 | Toward the EDU-to-PDU transition: Read Receipts. | Receipts are EDU only; no m.receipt PDU and no m.read rel_type in either tree |
| MSC2380 | ❌ ● | 0/0 | Matrix Media Information API | No /media/r0/info/{origin}/{media_id} endpoint |
| MSC2379 | ❌ ● | 0/0 | MSC 2379: Add /versions endpoint to Appservice API. | No /_matrix/app/versions probe code |
| MSC2375 | ❌ ◐ | 0/0 | Appservice Invite States | Appservice transactions send raw PDU JSON without invite_room_state injection |
| MSC2370 | ❌ ● | 0/0 | Resolve URL API | No /resolve_url endpoint in source |
| MSC2326 | ❌ ● | 0/0 | Label based filtering | No labels/not_labels EventFilter support; no m.label handling |
| MSC2316 | ❌ ● | 0/0 | Federation queries to aid with database recovery | No /_matrix/federation/v1/query/members route |
| MSC2301 | ❌ ● | 0/0 | Proposal for an /info endpoint on the CS API | No /info merger of /versions; no branding fields exposed |
| MSC2300 | ❌ ● | 0/0 | Proposal for a /ping endpoint on the CS API | No GET /_matrix/client/r0/ping route |
| MSC2278 | ❌ ◐ | 0/10 | Proposal for deleting content for expired and redacted messages | No DELETE /media client API; only admin-only delete helper |
| MSC2270 | 🟨 ● | 30/30 | Proposal for ignoring invites | m.ignored_room_list absent; generic account-data store only, no sync filter |
| MSC2228 | ❌ ● | 0/0 | Proposal for self-destructing messages | self_destruct fields not honored |
| MSC2214 | ❌ ● | 0/0 | Joining upgraded private rooms | m.room.previous_member event not implemented |
| MSC2213 | ❌ ● | 0/0 | Rejoinability of private rooms | rejoin_rule field not implemented |
| MSC2212 | ❌ ● | 0/0 | Third party user power levels | third_party_users not present in PL handling or auth rules |
| MSC2199 | ❌ ● | 0/0 | Canonical DMs (server-side middle ground edition) | no m.kind in sync summary; uses legacy m.direct account data |
| MSC2190 | ✅ ◐ | 80/80 | Allow appservice bots to use /sync | appservice token defaults to sender_localpart user |
| MSC2162 | ❌ ● | 0/0 | Signaling Errors at Bridges | no m.bridge_error on appservice delivery failure; absent in src and ruma |
| MSC2127 | ❌ ● | 0/0 | Proposal for a federation capabilities API | federation /capabilities and per-room capabilities not present |
| MSC2108 | ❌ ● | 0/0 | Sync over Server Sent Events | no /sync/sse or text/event-stream paths |
| MSC2102 | ❌ ◐ | 0/0 | Enforce Canonical JSON on the wire for the S2S API | no canonical-JSON wire enforcement on inbound S2S |
| MSC1974 | ❌ ● | 0/0 | Crypto Puzzle Challenge | open; hashcash-style proof-of-work never adopted |
| MSC1973 | ❌ ● | 0/0 | Hash Key User ID | open; speculative scheme never adopted |
| MSC1921 | ❌ ◐ | 0/0 | Cancellation of 3pid validation tokens | 3pid cancelToken endpoints not implemented; 3pid stack stubbed |
| MSC1862 | ❌ ◐ | 20/20 | Presence flag for capabilities API | presence on/off enforced; m.presence not in /capabilities response |
| MSC1797 | ❌ ● | 0/0 | Proposal for more granular profile error codes | branch; M_USER_NOT_FOUND/M_PROFILE_* error codes not used |
| MSC1780 | ❌ ● | 0/0 | Add DIDs and DID names as admin accounts to HS | open; m.did medium not supported in 3pid endpoints |
| MSC1768 | ❌ ● | 0/0 | Proposal to authenticate with public keys | open; m.login.proof.* not implemented |
| MSC1763 | ❌ ● | ?/0 | Proposal for specifying configurable per-room message retention periods. | no m.room.retention support; /retention/configuration endpoint absent |
| MSC1762 | ❌ ● | 0/0 | Support user-owned identifiers as new 3PID type | no m.did mediums; ruma Medium is email or msisdn; 3PID invites rejected |
| MSC1740 | ❌ ◐ | ?/0 | Using the Accept header to select an encoding | no Accept-based content negotiation; only application/json supported |
| MSC1607 | ❌ ◐ | 0/0 | Proposal for room alias grammar | alias parsing delegated to Ruma RoomAliasId; no NFKC/punycode/blacklist logic |
| MSC1228 | ❌ ● | ?/0 | Removing MXIDs from events | removing mxids never merged; no user_room_key or pseudo IDs in src |

## Closed

Sorted by MSC number, highest first. Out-of-scope rows are listed
in the [Out of scope](#out-of-scope) section.

| MSC | Status | Impl | Title | Note |
|---|---|---:|---|---|
| MSC4465 | ❌ ● | 0/0 | On-Demand Fetch for Missing Events | GET /event/{eventId} returns M_NOT_FOUND; no federation fallback. |
| MSC4463 | ❌ ◐ | 0/0 | Backfilling Pinned Events | No pinned-events backfill on join or on pinned_events update. |
| MSC4435 | ❌ ● | 0/0 | Room slowmode | No m.room.slowmode handling |
| MSC4418 | ✅ ● | 100/100 | Make `destination` a required server authentication field | destination required on inbound and outbound; cited verbatim in MSC. |
| MSC4415 | 🟨 ● | 0/50 | Make `/_matrix/client/v3/admin/whois/{userId}` only available to admins | whois is served, but a non admin may still look up their own user |
| MSC4403 | ❌ ● | 0/0 | Forbid `event_id` on PDUs received over federation | new room version forbidding event_id on PDUs; com.nhjkl.msc4403.opt2 absent. |
| MSC4400 | ❌ ● | 0/0 | Remove the depth field from PDUs | new room version removing depth field; com.nhjkl.msc4400.opt1 absent. |
| MSC4390 | ❌ ● | 0/0 | Room Blocking API | [→ MSC4375?] no client admin endpoints for room block/delete; only federation... |
| MSC4384 | 🟨 ◐ | ?/50 | Supporting alternative room directory sorting | Largest-first sort is hardcoded; no alt-sort hook |
| MSC4375 | ❌ ● | 0/0 | Admin Room Management | No /_matrix/client/v1/admin/rooms/* endpoints |
| MSC4373 | ❌ ● | 0/0 | Server opt-out of specific EDU types | No federation EDU-type preference endpoint is registered |
| MSC4329 | ❌ ● | 0/0 | Inviting with authorization | federation /v3/invite with create event in `state` not implemented |
| MSC4324 | ✅ ◐ | 80/80 | Fixing MSC4289's power level for tombstones | tombstone PL=150 set; matches highest-anchored intent for default config |
| MSC4317 | ❌ ● | 0/0 | Signed profile data | signed profile data; no `m.signed` profile field handling |
| MSC4316 | ❌ ● | 0/0 | External cross-signing signatures with X.509 certificates and (semi-)automate... | X.509 cross-signing; no `external` signature support |
| MSC4299 | ✅ ◐ | 100/100 | trusted users | Generic account data stores and syncs m.trusted_users; no trust logic to break |
| MSC4294 | ❌ ● | 0/0 | Ignore and mass ignore invites | no ignored_inviters list, no auto invite cleanup |
| MSC4276 | ❌ ● | 0/0 | Soft unfailure for self redactions | no self-redaction soft-fail bypass |
| MSC4214 | ❌ ● | 0/0 | Embedding Widgets in Messages | closed MSC; m.widget event/capability not implemented |
| MSC4202 | ❌ ◐ | 20/20 | Reporting User Profiles | client report endpoint exists; federation forwarding absent |
| MSC4184 | ❌ ● | 0/0 | Dynamic Notification Suppression | no m.push_rules_executed field on events |
| MSC4158 | ✅ ◐ | 80/100 | MatrixRTC focus information in .well-known | rtc_foci exposed in .well-known/matrix/client |
| MSC4148 | ❌ ● | 0/0 | Permitting HTTP(S) URLs for SSO IdP icons | SSO IdP icon limited to mxc URIs in config; HTTP(S) not allowed |
| MSC4124 | ❌ ● | 0/0 | Simple Server Authorization | m.server.knock/participation auth events not implemented |
| MSC4123 | ❌ ● | 0/0 | Allow `knock` -&gt; `join` transition | new room version with knock to join transition not implemented |
| MSC4107 | ❌ ● | 0/0 | Feature-focused versioning | features key on /versions not added |
| MSC4098 | ❌ ● | 0/0 | Use the SCIM protocol for provisioning | SCIM user provisioning endpoints absent (closed MSC) |
| MSC4097 | ❌ ● | 0/0 | Interactions between media redirection and authentication | media redirect symmetric encryption not implemented |
| MSC4072 | ❌ ● | 0/0 | Handling devices with no one-time keys in `/keys/claim` | Missing/exhausted devices are filtered out, not returned as empty objects. |
| MSC4069 | ❌ ● | 0/0 | Inhibit profile propagation | No ?propagate query parameter on profile endpoints. |
| MSC4059 | ❌ ● | 0/0 | Mutable event content | No mutable-event EDU or hashes-omitted detection. |
| MSC4058 | ❌ ● | 0/0 | Additive Events | No m.additive EDU or unsigned.m.additive metadata pipeline. |
| MSC4051 | ✅ ◐ | 80/80 | Using the create event as the room ID | V12 RoomVersionRules.room_create_event_id_as_room_id dispatched. |
| MSC4037 | ✅ ○ | 90/100 | Thread root is not in the thread | Receipts allowed for thread roots; spec wording is mostly client-facing. |
| MSC4018 | ❌ ● | 0/0 | Reliable call membership | Reliable call membership endpoints (PUT/DELETE) not present |
| MSC4015 | 🟨 ◐ | 70/70 | Voluntary Bot indicators | MSC4133 profile serves the bot key; no copy into m.room.member on join |
| MSC3975 | ❌ ● | 0/0 | rel_type for Replies | m.reply rel_type not handled |
| MSC3968 | ❌ ● | 0/0 | Poorer features | m.room.event_features state event not enforced |
| MSC3961 | ✅ ● | 90/100 | Sliding Sync Extension: Typing Notifications | sliding sync typing extension implemented |
| MSC3945 | 🟨 ◐ | 50/50 | Private device names | Federation hides device names by default; CSAPI /keys/query still leaks them ... |
| MSC3915 | ❌ ● | 0/0 | Owner power level | PL150 owner role / creator-defaults-to-150 not implemented. |
| MSC3912 | ❌ ● | 0/0 | Redaction of related events | with_rel_types / with_relations on /redact not implemented. |
| MSC3902 | ❌ ◐ | 20/20 | Faster remote room joins over federation (overview) | sends omit_members but immediately fetches full state |
| MSC3887 | ❌ ◐ | 0/0 | List matching push rules | closed MSC; list-matching in event_match not implemented |
| MSC3866 | ❌ ● | 0/0 | `M_USER_AWAITING_APPROVAL` error code | M_USER_AWAITING_APPROVAL error code not implemented |
| MSC3862 | ❌ ● | 0/0 | event_match (almost) anything | event_match only matches strings; non-string primitives not converted |
| MSC3859 | ❌ ● | 0/0 | Add well known media domain proposal | no m.media_server in well-known responses |
| MSC3852 | ❌ ● | 0/0 | Expose user agent information on `Device` | last_seen_user_agent not exposed on Device |
| MSC3782 | ❌ ● | 0/0 | Matrix public key login spec | m.login.publickey login type not implemented |
| MSC3779 | ❌ ● | 0/0 | "Owned" state events | owned state events require new room version |
| MSC3772 | ❌ ● | 0/0 | Push rule for mutually related events | relation_match push condition not implemented |
| MSC3761 | ❌ ● | 0/0 | State event change control | m.event.acl ACL events for state not implemented |
| MSC3760 | ❌ ● | 0/0 | State sub-keys | state_subkey requires new room version; not present |
| MSC3754 | ✅ ● | 95/100 | Removing profile information | [→ MSC4133?] DELETE profile field route serves avatar_url and displayname; ke... |
| MSC3741 | ❌ ● | 0/0 | Revealing the useful login flows to clients after a soft logout | login does not return per-user flows for soft-logout tokens |
| MSC3720 | ❌ ● | 0/0 | Account status endpoint | branch MSC; no /account_status endpoints (CS or federation) |
| MSC3659 | ❌ ● | 0/0 | Invite Rules | closed MSC; no invite_rules account data dispatch |
| MSC3613 | ❌ ● | 0/0 | Combinatorial join rules | branch MSC; no combinatorial join_rules array logic in tuwunel |
| MSC3588 | ✅ ● | 100/100 | WIP: MSC3588: Encrypted Stories As Rooms | msc3588.stories.block-list rides the generic account-data store and sync |
| MSC3585 | ✅ ● | 100/100 | Allow the base event to be omitted from `/federation/v1/event_auth` response | event_auth handler omits the requested event itself per MSC |
| MSC3572 | ❌ ◐ | 0/0 | Relation aggregation cleanup | no relations rename; m.relations only |
| MSC3523 | ❌ ● | 0/0 | Timeboxed/ranged relations endpoint | no from_target/to_target query params on /relations |
| MSC3429 | ❌ ● | 0/0 | Individual room preview API | no /rooms/{id}/preview endpoint |
| MSC3391 | ✅ ● | 100/100 | API to delete account data | src/api/client/account_data.rs:126; both DELETE routes via Ruma&lt;R&gt;; tombstone... |
| MSC3306 | ❌ ● | 0/0 | How to count unread messages | notification_count uses push-rule Notify actions, not MSC3306 algo |
| MSC3302 | ✅ ● | 100/100 | Stories via To-Device-Messaging | m.stories.blocklist and m.stories.ignore ride the generic account-data path |
| MSC3244 | ❌ ● | 0/10 | Room version capabilities | capabilities lacks room_capabilities knock/restricted info |
| MSC3174 | ❌ ● | 0/0 | An error code for spam rejections | M_ANTISPAM_REJECTION error code not used |
| MSC3137 | ✅ ● | 100/100 | Define space room type, subset of MSC1772 | type:m.space in m.room.create accepted; used in directory and spaces |
| MSC3125 | ❌ ● | 0/0 | Limits API — Part 5: per-Instance limits | per-instance limits admin API absent |
| MSC3105 | ❌ ● | 0/0 | Previewing user-interactive flows | OPTIONS preflight for UIA flows not implemented |
| MSC3073 | ❌ ● | 0/0 | Role based access control | closed; rbac/m.role not implemented |
| MSC3060 | ❌ ● | 0/0 | Room labels | branch; m.room.labels not surfaced in publicRooms |
| MSC3053 | ❌ ● | 0/0 | Limits API — Part 2: per-Room limits | closed; no admin/limits endpoints or m.limits.* events |
| MSC3026 | ✅ ● | 100/100 | `busy` presence state | PresenceState::Busy and msc3026.busy_presence flag |
| MSC3013 | ❌ ● | 0/0 | Encrypted Push | closed; no encrypted-push algorithm support |
| MSC3012 | ❌ ● | 0/0 | Post-registration terms of service API | branched; no /terms endpoint or m.terms account data |
| MSC3007 | ❌ ● | 0/0 | Forced insertion and room blocking by self-banning | closed; no insert_member power or /insert endpoint |
| MSC3006 | ❌ ● | 0/0 | Bot Interactions | closed; bot-interaction event types not implemented |
| MSC3005 | ❌ ● | 0/0 | Streaming Federation Events | closed; no streaming federation transport |
| MSC2957 | ❌ ● | 0/0 | Cryptographically Concealed Credentials | PAKE-style login flow; closed; not implemented |
| MSC2912 | ❌ ● | 0/0 | Setting cross-signing keys during registration | no device_signing field accepted by /register |
| MSC2839 | ❌ ◐ | 0/0 | Dynamic User-Interactive Authentication | closed; UIA flows are static in Tuwunel |
| MSC2835 | ❌ ◐ | 0/10 | Add UIA to the /login endpoint | closed; /login does not consume UIA auth dict |
| MSC2821 | ❌ ● | 0/0 | Test Pusher | POST /pushers/push test endpoint not implemented |
| MSC2810 | 🟨 ● | 70/70 | Consistent globs specification | ACL and push globs go through wildmatch; the backslash escape is unimplemented |
| MSC2773 | ❌ ◐ | 0/0 | Room kinds | closed; no m.kind summary or m.room.kind handling |
| MSC2706 | ❌ ● | 0/0 | IPFS as a media repository | No IPFS support; no m.ipfs capability |
| MSC2704 | ✅ ◐ | 100/100 | Handling duplicate media on `/upload` + clarifying the origin of an MXC URI | Fresh MXC per upload; no dedup |
| MSC2703 | ✅ ● | 100/100 | Media ID grammar | 32-char alphanumeric media IDs; opaque |
| MSC2700 | 🟨 ◐ | 50/50 | Thumbnail requirements for the media repo | image crate handles png/jpeg/gif; no svg/video |
| MSC2697 | 🟨 ● | 40/60 | Device dehydration | [→ MSC3814] Dehydration served at the MSC3814 prefix; no claim and no token r... |
| MSC2654 | ❌ ● | 0/0 | Unread counts | No unread_count in sync; no msc2654 markers |
| MSC2638 | ❌ ● | 0/0 | Ability for clients to request homeservers to resync device lists | No /devices/refresh endpoint; no msc2638 marker in src |
| MSC2631 | ✅ ◐ | 80/80 | Add `default_payload` to PusherData | ruma HttpPusherData flattens custom data; default_payload accepted via passth... |
| MSC2625 | ❌ ◐ | 0/0 | Add `mark_unread` push rule action | No mark_unread action; sync exposes only highlight/notification counts |
| MSC2579 | ✅ ● | 100/100 | Improved tagging support | Generic account-data path carries `m.tag.<tag>`; ordering rules are client-side |
| MSC2463 | ❌ ◐ | 0/0 | Exclusion of MXIDs in push rules content matching | closed MSC; no MXID exclusion in push rule content matching |
| MSC2416 | ✅ ● | 90/100 | Add m.login.jwt authentication type | m.login.jwt fully wired in session module |
| MSC2389 | ❌ ● | 0/0 | Toward the EDU-to-PDU transition: Typing. | Typing is EDU only; no m.typing PDU; a later message does not end typing |
| MSC2356 | ❌ ● | 0/0 | Bulk /joined_members endpoint | No POST /joined_members bulk endpoint in src/api |
| MSC2314 | ❌ ● | 0/40 | Backfilling Current State | src/api/server/state.rs:14 requires event_id; no current-state branch |
| MSC2306 | ✅ ◐ | 100/100 | Removing MSISDN password resets | msisdn pw reset endpoint absent; ThreepidDenied on msisdn |
| MSC2271 | ❌ ● | 0/0 | Proposal for TOTP 2FA | No TOTP endpoints, no m.login.totp UIA stage |
| MSC2261 | ✅ ● | 100/100 | Allow `m.room.aliases` events to be redacted by room admins | Subsumed by MSC2432/v6 redaction rules |
| MSC2260 | 🟨 ● | 50/50 | Update the auth rules for `m.room.aliases` events | Subsumed by MSC2432/v6 auth rules; aliases sender-domain check enforced |
| MSC2233 | ❌ ● | 0/0 | Unauthenticated Capabilities API | no /capabilities/server unauthenticated endpoint |
| MSC2153 | ✅ ● | 100/100 | Add a default push rule to ignore m.reaction events | Ruleset::server_default() includes .m.rule.reaction via Ruma |
| MSC2061 | ✅ ● | 100/100 | make the trailing slash on `GET /_matrix/key/v2/server/` optional | src/api/router.rs:250 routes both /key/v2/server and /server/{key_id} |
| MSC2000 | ❌ ● | 0/0 | MSC 2000: Proposal for server-side password policies | branch; no /password_policy endpoint or password validation |
| MSC1998 | ❌ ● | 0/0 | Two-Factor Authentication Providers | closed; TOTP/recovery 2FA never adopted by spec |
| MSC1953 | ✅ ● | 100/100 | Remove prev_content from the essential keys list | ruma redact() does not retain prev_content |
| MSC1943 | ✅ ● | 100/100 | Set v3 to be the default room version | default room version V11 (&gt;= v3) |
| MSC1935 | 🟨 ● | 30/40 | Key validity enforcement | [→ MSC2076] no valid_until_ts enforcement; v4 and v5 supported but listed uns... |
| MSC1888 | ✅ ● | 90/100 | Proposal to send EDUs to appservices | [→ MSC2409] appservice receive_ephemeral with EDU push; src/service/sending/s... |
| MSC1840 | 🟨 ● | 60/100 | Typed rooms | room_types filter on sliding sync only; v3 filter API unextended |
| MSC1818 | ✅ ● | 100/100 | Remove references to presence lists | presence list endpoints absent (compliant by removal) |
| MSC1796 | ❌ ◐ | 0/0 | Proposal for improving notifications for E2E encrypted rooms | branch; m.mentions on encrypted events not honored server-side |
| MSC1777 | ❌ ● | 0/0 | Proposal for implementing peeking over federation (server pseudousers) | branch; server pseudouser peeking not implemented |
| MSC1776 | ❌ ● | 0/0 | Proposal for implementing peeking via /sync in the CS API | branch; POST /sync with peek not implemented |
| MSC1731 | ❌ ◐ | 0/0 | Mechanism for redirecting to an alternative server during SSO login | branch; homeserver query param on sso loginToken redirect not added |
| MSC1714 | ❌ ● | 0/0 | using the TLS private key to sign federation-signing keys | branch/abandoned 2018; no rsa key id, no TLS-cross-signing in src/api/server/... |
| MSC1687 | ❌ ● | ?/0 | Proposal for storing an encrypted recovery key on the server to aid recovery ... | no PBKDF passphrase backup logic; auth_data passes through opaquely |
| MSC1597 | 🟨 ◐ | 20/30 | Grammars for identifiers in the Matrix protocol | validate_strict is dispatched; ruma admits / and +; ID grammars unenforced |
| MSC1497 | ✅ ● | 100/100 | Advertising support of experimental features in the CS API | unstable_features map present in /_matrix/client/versions |
| MSC1425 | ✅ ● | 100/100 | Room Versioning | room versioning fully present; STABLE_ROOM_VERSIONS in core/config |
| MSC1227 | ✅ ● | 80/90 | Proposal for lazy-loading room members to improve initial sync speed and clie... | lazy_load_members supported via filter; service in rooms/lazy_loading |
| MSC1194 | ❌ ● | 0/0 | A way for HSes to remove bindings from ISes (aka unbind) | no IS unbind; delete_3pid and deactivate always return no-support |
| MSC688 | ✅ ● | 80/100 | Room Summaries (was: Calculate room names server-side) | sync summary with heroes and counts; v3 heroes capped at 5 members |
| MSC441 | 🟨 ● | 20/20 | Support for Reactions / Aggregations | [→ MSC2675/MSC2676] relations paging only; no vote coalescing, counts or QoS ... |

## Out of scope

MSCs marked `n/a`: the homeserver-applicable scope is empty
(addressed to an identity server, integration manager, widget or
client only, or governance/process), or the MSC is unimplemented and
superseded by another MSC. Scope is a property of the proposal, not
of what Tuwunel has chosen to build. Listed here for
audit; the `Inv` column carries each row's inventory bucket in
place of the (uniformly empty) `Impl` cell.

| MSC | Status | Inv | Title | Note |
|---|---|---|---|---|
| MSC4534 | ⬛ ● | open | Simple room rules | client-enforced room rules; the MSC avoids server-side changes by design |
| MSC4533 | ⬛ ● | open | MatrixRTC widget actions for authenticated livekit realted endpoints | widget API actions delegating MSC4195 endpoints; no homeserver scope |
| MSC4531 | ⬛ ◐ | open | Deprecate legacy VoIP | spec deprecation of legacy VoIP; no server behaviour change is required |
| MSC4529 | ⬛ ● | open | Client-side profile overrides | account data read by clients; MSC states servers need no knowledge of it |
| MSC4522 | ⬛ ◐ | open | Username colors | client rendering rules; the server side is generic MSC4133 field storage |
| MSC4518 | ⬛ ● | open | Registries | process MSC for spec registries; no homeserver behaviour is defined |
| MSC4515 | ⬛ ● | open | RTC Transports discovery for widgets | widget API action; the hosting client calls the endpoint, no server scope |
| MSC4504 | ⬛ ● | open | Using a global version number for the entire specification (v2) | process MSC for the Spec Core Team; only the SCT may implement it |
| MSC4489 | ⬛ ● | open | Per-room contact state event | client and bot only; m.room.contact is ordinary room state |
| MSC4487 | ⬛ ● | open | Deprecate internal use of matrix.to navigation | client-only; server sends no matrix.to or matrix: URIs |
| MSC4483 | ⬛ ● | open | Encrypted Account Data | client-only; server already stores account data opaquely |
| MSC4482 | ⬛ ● | open | Ability to bookmark messages | client-only; m.bookmarks room type and m.bookmark event are passthrough |
| MSC4481 | ⬛ ● | open | Non-sigil matrix.to | client-only URL format; server emits no matrix.to links |
| MSC4476 | ⬛ ● | open | Image pack rooms | client-only; m.image_pack room type needs no server-side handling |
| MSC4475 | ⬛ ● | open | Media galleries by inline MXC links | Inline MXC gallery rendering is client-side; link tracking deferred |
| MSC4474 | ⬛ ◐ | open | Clarify usage of content blocks in Extensible Events | Spec clarification for MSC1767 authors; no homeserver requirement |
| MSC4471 | ⬛ ● | open | Streaming ephemeral event updates for room events | MSC states no homeserver work required; to-device transport suffices |
| MSC4468 | ⬛ ◐ | open | Reporting to communities (via to-device) | Pure state-event plus to-device passthrough; no HS-specific work. |
| MSC4460 | ⬛ ● | open | Extensible Events - Alternative unstable support | Client-side hybrid extensible-events rendering rules; no Tuwunel dispatch. |
| MSC4459 | ⬛ ● | open | Image pack references | Client-side image pack reference field; homeserver passes events through tran... |
| MSC4456 | ⬛ ● | open | Harms taxonomy | Harm identifier taxonomy; the server MUST is conditional on other MSCs |
| MSC4455 | ⬛ ● | open | Catch-all property for spaces | Client-only space catch-all state event; MSC says servers not required |
| MSC4454 | ⬛ ● | open | Deprecating Spoiler Fallback In Media Repository | Client spoiler body construction; no homeserver behavior |
| MSC4451 | ⬛ ● | open | Deprecate notifications endpoint | Spec-only deprecation; endpoint still served per MSC |
| MSC4444 | ⬛ ● | closed | Malicious PDUs | April Fools joke MSC, status closed |
| MSC4441 | ⬛ ● | open | Encrypted User Profile Annotations via Account Data | Client-side encrypted account data convention; no server change |
| MSC4421 | ⬛ ● | closed | Standardize the spec on US English | spec house-style proposal (en-US); no protocol surface. |
| MSC4414 | ⬛ ● | open | Design decision - Errors | design-direction proposal with no technical changes. |
| MSC4412 | ⬛ ● | open | Widget Base PostMessage API | Widget postMessage negotiation; no homeserver footprint |
| MSC4411 | ⬛ ● | open | Widget State Event | Widget state event schema; server stores m.widget state opaquely |
| MSC4409 | ⬛ ● | merged | Clarify thumbnailing behavior in E2EE | Client thumbnail guidance in E2EE; no homeserver requirement |
| MSC4407 | ⬛ ● | open | Sticky Events (Widget API) | widget API for sticky events; no homeserver involvement beyond MSC4354. |
| MSC4405 | ⬛ ● | open | Deprecate the emoji method for SAS verification | deprecates emoji SAS in favor of decimal; client-side method choice. |
| MSC4404 | ⬛ ● | closed | Compare emoji by name rather than image | adds accept_languages to to-device verification; client SAS UI guidance. |
| MSC4397 | ⬛ ● | open | Tags as Spaces | account_data key m.tag_space points at a private space; server is opaque. |
| MSC4392 | ⬛ ● | open | Encrypted reactions and replies | client puts m.relates_to inside encrypted payload; server forwards untouched. |
| MSC4391 | ⬛ ● | open | Simplified in-room bot commands | in-room bot command UI; state and message events forwarded opaquely. |
| MSC4389 | ⬛ ● | open | Image ordering within packs | image pack ordering is account-data; server passes through opaque blobs. |
| MSC4386 | ⬛ ● | open | Automatically sharing secrets after device verification | client-to-client to-device verification protocol; server forwards opaque events. |
| MSC4385 | ⬛ ● | open | Pushing secrets to other devices | Client to-device convention; server forwards m.secret.push opaquely |
| MSC4381 | ⬛ ◐ | merged | Remove plaintext sender key | Removal of plaintext sender_key is client-side; server is opaque |
| MSC4377 | ⬛ ● | open | Clarify Image Pack Ordering | Image pack ordering is client-side account/state data convention |
| MSC4362 | ⬛ ● | open | Simplified Encrypted State Events | No encrypt_state_events handling in m.room.encryption |
| MSC4357 | ⬛ ● | open | Live Messages via Event Replacement | Client-only convention reusing m.replace; no server work |
| MSC4356 | ⬛ ● | merged | Recently used emoji | Pure client-side account data convention; no server work |
| MSC4349 | ⬛ ● | open | Causal barriers and enforcement | causal barrier terminology and deferred authorization not adopted |
| MSC4347 | ⬛ ● | closed | Emoji verification images | client-side emoji image rendering for SAS verification; not server |
| MSC4340 | ⬛ ● | open | Prompts and partial commands for in room commands. | bot command prompts; client-side concern, no server changes |
| MSC4334 | ⬛ ● | open | Add `m.room.language` state event. | m.room.language state event; not whitelisted/handled specially |
| MSC4333 | ⬛ ● | open | Room state API for moderation bots | moderation bot state event; client-side concern |
| MSC4332 | ⬛ ● | open | In-room bot commands | in-room bot commands; client-side concern, no server changes |
| MSC4321 | ⬛ ● | open | Policy Room Upgrade Semantics | policy room upgrade `move`/`transition` semantics not handled |
| MSC4313 | ⬛ ● | merged | Require HTML `<ol>` `start` Attribute support | client HTML rendering requirement; not applicable to homeserver |
| MSC4310 | ⬛ ◐ | open | MatrixRTC decline `m.rtc.notification` | event-only MSC; ruma feature enabled, no homeserver-specific behavior |
| MSC4302 | ⬛ ● | open | Exchanging FHIR resources via Matrix events | new event type for FHIR, no server logic |
| MSC4301 | ⬛ ● | closed | Event capability negotiation between clients | client-to-client capability negotiation |
| MSC4300 | ⬛ ● | open | Processing status requests &amp; responses | client-to-client status request/response in events |
| MSC4296 | ⬛ ● | open | Mentions for device IDs | client-side mentions field extension |
| MSC4295 | ⬛ ● | open | Bot bounce limit - a better loop prevention mechanism | bot/client behavior; servers relay events unmodified |
| MSC4292 | ⬛ ● | open | Handling incompatible room versions in clients | [→ MSC4331] |
| MSC4287 | ⬛ ● | merged | Sharing key backup preference between clients | client-side account data for key backup preference |
| MSC4286 | ⬛ ● | open | App store compliant handling of payment links within events | client-side HTML rendering attribute |
| MSC4283 | ⬛ ● | open | Distinction between Ignore and Block | terminology MSC, no implementation surface |
| MSC4281 | ⬛ ● | closed | Mitigating Membership Mistakes, or "Invisible" Cryptography | closed April 1 joke MSC; client-only encryption mode |
| MSC4278 | ⬛ ● | merged | Media preview controls | Client rendering preference in account data; the only server clause is a MAY |
| MSC4274 | ⬛ ● | open | Inline media galleries via msgtypes | new client msgtype m.gallery, no server logic |
| MSC4273 | ⬛ ● | open | Approve and Disapprove ratings for moderation policies | new event type for moderation tools, no server logic |
| MSC4270 | ⬛ ● | open | Matrix Glossary | glossary/spec doc proposal, not an implementation feature |
| MSC4269 | ⬛ ● | open | Unambiguous mentions in body | client-side message body composition |
| MSC4268 | ⬛ ● | merged | Sharing room keys for past messages | client-only E2EE key sharing; server only relays to-device and stores media |
| MSC4253 | ⬛ ● | open | Modifying or rejecting accepted MSCs | Spec process MSC; no implementable behavior |
| MSC4252 | ⬛ ● | open | Extensible Events modification: State event handling | Client-side guidance for extensible state events |
| MSC4238 | ⬛ ● | open | Pinned events read marker | Client-set m.read.pinned_events account data only |
| MSC4231 | ⬛ ● | closed | Backwards compatibility for media captions | Client-side caption fallback rendering; no server work |
| MSC4230 | ⬛ ● | merged | 'Animated' flag for images | event-only; passthrough; merged in spec |
| MSC4229 | ⬛ ● | closed | Pass through `unsigned` data from `/keys/upload` to `/keys-query` | template/example proposal; no real change |
| MSC4227 | ⬛ ● | open | Audio based quick login | no MSC4108 rendezvous support; audio/DTMF login absent |
| MSC4221 | ⬛ ● | open | Room Banners | event-only; passthrough |
| MSC4220 | ⬛ ● | open | Local call rejection (m.call.reject_locally) | event-only; m.call.reject_locally not interpreted |
| MSC4211 | ⬛ ● | open | WebXDC on Matrix | event-only; passthrough |
| MSC4209 | ⬛ ● | open | Updating endpoints in-place | deprecation policy clarification; no code |
| MSC4206 | ⬛ ● | open | Moderation policy auditing and context | m.policy.rule.context not interpreted server-side |
| MSC4205 | ⬛ ● | open | Hashed moderation policy entities | hashed entity policies not interpreted |
| MSC4197 | ⬛ ● | open | Copy-Paste Hints | event content field; passthrough |
| MSC4193 | ⬛ ● | open | Spoilers on Media | event content field; passthrough; nothing for HS to do |
| MSC4192 | ⬛ ● | open | Comparison of proposals for ignoring invites | comparison/research document, not a feature |
| MSC4171 | ⬛ ● | open | Service members | Client-side name and avatar computation; server filtering explicitly rejected |
| MSC4161 | ⬛ ● | open | Crypto terminology for non-technical users | crypto terminology guidance for clients |
| MSC4159 | ⬛ ● | merged | Remove the deprecated name attribute on HTML anchor elements | client-side HTML rendering recommendation |
| MSC4157 | ⬛ ● | open | Delayed Events (widget-api) | widget-api only; not a homeserver concern |
| MSC4153 | ⬛ ● | merged | Exclude non-cross-signed devices | client-side cross-signing enforcement and to-device filtering |
| MSC4150 | ⬛ ● | open | m.allow recommendation for moderation policy lists | m.allow recommendation for policy lists is client-side |
| MSC4147 | ⬛ ● | merged | Including device keys with Olm-encrypted to-device messages | sender_device_keys in Olm plaintext is client-side |
| MSC4146 | ⬛ ● | closed | Shared Message Drafts | shared message drafts via m.drafts rooms is client-side |
| MSC4144 | ⬛ ● | open | Per-message profiles | m.per_message_profile is client-only event content |
| MSC4142 | ⬛ ● | merged | Remove unintentional intentional mentions in replies | client-side guidance for m.mentions in replies |
| MSC4139 | ⬛ ● | open | Bot buttons &amp; conversations | m.prompts mixin is client-only event content |
| MSC4132 | ⬛ ● | merged | Deprecate Linking to an Event Against a Room Alias. | deprecation of event-on-room-alias URIs is client-only |
| MSC4131 | ⬛ ● | open | Handling `m.room.encryption` events | client-side guidance on handling m.room.encryption events |
| MSC4119 | ⬛ ● | open | Voluntary content flagging | client-only m.room.context flagging mixin; server is content-agnostic |
| MSC4114 | ⬛ ● | open | Matrix as a password manager | client-only password manager via rooms; no server-side requirements |
| MSC4113 | ⬛ ● | closed | Image hashes in Policy Lists | m.policy.media_hash unknown to server (closed MSC) |
| MSC4110 | ⬛ ● | open | Fewer Features | m.room.event_features state event has no special server handling |
| MSC4101 | ⬛ ● | open | Hashes for unencrypted media | hashes field on unencrypted media info not consumed by server |
| MSC4095 | ⬛ ● | open | Bundled URL previews | Client-embedded preview data; the proposal asks a homeserver for nothing |
| MSC4092 | ⬛ ● | open | Enforce tests around sensitive parts of the specification | process MSC about test enforcement; no protocol changes |
| MSC4077 | ⬛ ● | merged | Improved process for handling deprecated HTML features | process MSC for HTML feature deprecation; no server work |
| MSC4075 | ⬛ ◐ | open | MatrixRTC Notification Event (call ringing) | Client ring and notify semantics over ordinary timeline and to-device events |
| MSC4073 | ⬛ ● | open | Shepherd teams | process MSC about SCT shepherd teams; not protocol |
| MSC4052 | ⬛ ● | closed | Hiding read receipts UI in certain rooms | Pure client-side hint via m.hide_ui state event. |
| MSC4050 | ⬛ ● | open | MXID verification | Pure client/third-party signaling via custom event types. |
| MSC4039 | ⬛ ● | open | Access the Content repository with the Widget API | Widget API extension; entirely client-to-widget scope. |
| MSC4036 | ⬛ ● | closed | Room organization by promoting threads | Pure client UI behavior toggled by m.promote_threads state event. |
| MSC4032 | ⬛ ● | open | Asset Collections | Asset Collections defines client-side data structures for 3D worlds; no serve... |
| MSC4013 | ⬛ ● | open | Poll history cache | client convention using existing relations API |
| MSC4006 | ⬛ ● | open | Answered Elsewhere for VoIP | VoIP m.call.hangup reason value, client concern |
| MSC4004 | ⬛ ● | open | unified view of identity service | identity service API, not homeserver |
| MSC4003 | ⬛ ● | open | Semantic table attributes | HTML table sanitization is client concern |
| MSC3985 | ⬛ ● | open | Break-out rooms | m.breakout state event not handled |
| MSC3979 | ⬛ ● | open | Revised feature profiles | client feature profiles, not a homeserver concern |
| MSC3978 | ⬛ ● | closed | Deprecate room tagging | room tagging not deprecated; still implemented |
| MSC3977 | ⬛ ● | open | Introduction | IETF MIMI framework draft, not a Matrix MSC |
| MSC3973 | ⬛ ● | open | Search users in the user directory with the Widget API | widget API extension; client/embedder feature |
| MSC3972 | ⬛ ● | closed | Lexicographical strings as an ordering mechanism | client-side ordering algorithm |
| MSC3971 | ⬛ ● | open | Sharing image packs | image pack sharing/links not implemented |
| MSC3969 | ⬛ ● | closed | Size limits | m.room.size_limits state event not enforced |
| MSC3956 | ⬛ ● | open | Extensible Events - Encrypted Events | client-side extensible encrypted event format |
| MSC3955 | ⬛ ● | open | Extensible Events - Automated event mixin (notices) | m.automated mixin for extensible events not implemented |
| MSC3954 | ⬛ ● | open | Extensible Events - Text Emotes | Extensible m.emote event type not specifically handled. |
| MSC3949 | ⬛ ● | open | Power Level Tags | Power-level tag state event is client UX; no server enforcement. |
| MSC3948 | ⬛ ● | open | Repository room for Thirdroom | ThirdRoom 3D-asset repository room type; no homeserver semantics. |
| MSC3935 | ⬛ ● | open | Cute Events against social distancing | Client-side cute event msgtype; no server behavior. |
| MSC3927 | ⬛ ● | open | Extensible Events - Audio | Extensible m.audio event type not specifically dispatched. |
| MSC3923 | ⬛ ● | merged | Bringing Matrix into the IETF process | Spec-process MSC about IETF coordination; no homeserver code. |
| MSC3919 | ⬛ ● | open | Matrix Message Format (IETF/MIMI) | IETF informational draft on Matrix message format; not a server feature. |
| MSC3918 | ⬛ ● | open | Matrix Message Transport (IETF/MIMI) | IETF informational draft about Matrix as MIMI transport; not a server feature. |
| MSC3910 | ⬛ ● | closed | Content tokens for media | [→ MSC3916] |
| MSC3908 | ⬛ ● | open | Expiring Policy List Recommendations | expiring policy field interpreted by clients/bots |
| MSC3907 | ⬛ ● | open | Mute Policy Recommendation | mute policy recommendation enforced by clients/bots |
| MSC3906 | ⬛ ● | closed | Protocol to use an existing Matrix client session to complete login and setup... | [→ MSC4108] QR sign-in protocol depends on unimplemented rendezvous |
| MSC3903 | ⬛ ● | closed | X25519 Elliptic-curve Diffie-Hellman ephemeral for establishing secure channe... | [→ MSC4108] client-to-client X25519 ECDH; no server role |
| MSC3898 | ⬛ ● | open | Native Matrix VoIP signalling for cascaded SFUs | VoIP SFU signalling is opaque events between clients |
| MSC3892 | ⬛ ● | open | Custom Emotes with Encryption | custom emotes are pure client/state-event feature |
| MSC3888 | ⬛ ◐ | open | Voice Broadcast | voice broadcast is opaque events, no server change required |
| MSC3886 | ⬛ ● | closed | Simple client rendezvous capability | [→ MSC4108] |
| MSC3880 | ⬛ ● | open | dummy replies for Olm | client-side Olm dummy event behavior |
| MSC3879 | ⬛ ● | open | Trusted key forwards | E2EE key forwarding flag is client-side |
| MSC3869 | ⬛ ● | open | Read event relations with the Widget API | Widget API extension; homeservers do not implement widget API |
| MSC3868 | ⬛ ◐ | open | Room Contribution | custom state event for room contribution, no server requirements |
| MSC3857 | ⬛ ● | open | Welcome messages/screening | no m.room.welcome state event handling |
| MSC3849 | ⬛ ● | open | Observations and Reinforcement | no observation/reinforcement event handling |
| MSC3846 | ⬛ ● | open | Allowing widgets to access TURN servers | widget TURN access; client-widget API only |
| MSC3845 | ⬛ ● | open | Draft: Expanding policy rooms to reputation | no m.opinion recommendation handling |
| MSC3842 | ⬛ ● | open | Power levels on message (extensible) events | proposal body is TBD; nothing to implement |
| MSC3825 | ⬛ ◐ | open | Obvious relation fallback location | is_falling_back location handled by Ruma types passively |
| MSC3819 | ⬛ ● | open | Allowing widgets to send/receive to-device messages | widget to-device is client-widget API only |
| MSC3817 | ⬛ ● | open | Allow widgets to create rooms | widget API only, no server-side surface |
| MSC3815 | ⬛ ● | open | 3D Worlds | 3D worlds is client-side room type and state events; no server behavior |
| MSC3813 | ⬛ ● | open | Obfuscated events | obfuscated events; client-side dummy traffic |
| MSC3812 | ⬛ ● | open | Hint buttons in messages | hint buttons in messages; client UI |
| MSC3803 | ⬛ ● | open | Matrix Widget API v2 | Widget API v2 issue placeholder |
| MSC3796 | ⬛ ◐ | open | Auth/linking for content repo (and enforcing GDPR erasure) | [→ MSC3916] |
| MSC3790 | ⬛ ● | closed | Register Clients | client launcher registry; client-only |
| MSC3784 | ⬛ ● | merged | Using room type of `m.policy` for policy rooms | Room type string only; the text leaves all handling to clients |
| MSC3783 | ⬛ ● | merged | Fixed base64 for SAS verification | SAS MAC scheme is client-to-client crypto |
| MSC3780 | ⬛ ● | open | Knocking on `action=join` | matrix-uri client UX fallback for knock |
| MSC3775 | ⬛ ● | open | Markup Locations for Audiovisual Media | event content schema for media markup |
| MSC3768 | ⬛ ● | open | Push rule action for in-app notifications | [→ MSC2625] |
| MSC3757 | ⬛ ● | closed | Restricting who can overwrite a state event. | [→ MSC4354] |
| MSC3755 | ⬛ ● | open | Member pronouns | pronouns are client member-content fields |
| MSC3752 | ⬛ ● | open | Markup locations for text | event content schema for text markup locations |
| MSC3751 | ⬛ ● | open | Allowing widgets to read account data | Widget API permission, not a homeserver concern |
| MSC3746 | ⬛ ● | closed | Render image data in reactions | [→ MSC4027] image reactions are client-only event content |
| MSC3735 | ⬛ ◐ | open | Add device information to m.room_key.withheld message | client-side to-device field; server relays unchanged |
| MSC3725 | ⬛ ● | open | Content warnings | client-side content warning event content; no server changes |
| MSC3700 | ⬛ ◐ | merged | Deprecate plaintext sender_key | client-side ignoring of sender_key/device_id; server is transparent |
| MSC3676 | ⬛ ◐ | merged | Transitioning away from reply fallbacks. | client-side reply-fallback transition rules; no server gate |
| MSC3662 | ⬛ ● | open | Allow Widgets to share user MxIds to the client | widget-to-client API; no server involvement |
| MSC3647 | ⬛ ● | open | Bring Your Own Bridge - Decentralising Bridges | WIP bridge negotiation; no spec-level details, no server impl |
| MSC3639 | ⬛ ● | open | Matrix for the social media use case | client-side social media room/event conventions; no server changes |
| MSC3635 | ⬛ ● | open | Early Media for VoIP | client-side VoIP signalling; no server changes required |
| MSC3592 | ⬛ ● | open | Markup locations for PDF documents | client-side PDF markup event types; no server implementation required |
| MSC3575 | ⬛ ● | closed | Sliding Sync (aka Sync v3) | [→ MSC4186] |
| MSC3574 | ⬛ ● | open | Marking up resources | no m.markup.resource or annotation handling |
| MSC3554 | ⬛ ● | open | Extensible Events - Translatable Messages | no lang field handling; ruma feature not enabled |
| MSC3553 | ⬛ ● | open | Extensible Events - Videos | unstable-msc3553 not enabled in ruma features |
| MSC3552 | ⬛ ● | open | Extensible Events - Images and Stickers | unstable-msc3552 not enabled in ruma features |
| MSC3551 | ⬛ ● | open | Extensible Events - Files | unstable-msc3551 not enabled; no extensible m.file event |
| MSC3531 | ⬛ ● | open | Letting moderators hide messages pending moderation | client-only m.visibility event; server explicitly unchanged |
| MSC3517 | ⬛ ● | closed | "Mention" Pushrule | [→ MSC3952] |
| MSC3510 | ⬛ ● | closed | Let users with the same power level kick/ban/demote each other. | [→ MSC3915] |
| MSC3489 | ⬛ ◐ | open | m.beacon: Sharing streams of location data with history | Beacons are plain state and message events; no homeserver clause in the text |
| MSC3464 | ⬛ ● | closed | Allow Users to Post on Behalf of Other Users | no m.on_behalf_of or m.allows_on_behalf_of handling |
| MSC3417 | ⬛ ● | open | Call room room type | creation_content type=m.call passes through createRoom |
| MSC3401 | ⬛ ● | open | Native Group VoIP signalling | only default PL for m.call/m.call.member; no to-device signaling |
| MSC3382 | ⬛ ● | closed | Inline message Attachments | PR-style amendment to MSC2881, not a standalone proposal |
| MSC3381 | ⬛ ● | merged | Chat Polls | Polls are client-side by design; the poll push rules belong to MSC3930 |
| MSC3368 | ⬛ ● | open | Message Content Tags | no message-content tag awareness |
| MSC3309 | ⬛ ● | open | Room Counters | no m.room.counter event handling |
| MSC3291 | ⬛ ● | merged | Muting in VoIP calls | server passes call events opaquely; ruma has the type |
| MSC3286 | ⬛ ● | closed | Media spoilers | server passes events opaquely; no spoiler-aware code |
| MSC3282 | ⬛ ● | closed | Expose enable_set_displayname in capabilities response | [→ MSC3283] |
| MSC3279 | ⬛ ● | closed | Expose enable_set_displayname in capabilities response | [→ MSC3283] |
| MSC3270 | ⬛ ● | closed | Symmetric megolm backup | server stores backup auth_data/session_data opaquely |
| MSC3265 | ⬛ ● | closed | Login and SSSS with a Single Password | client-only construction; explicitly no server-side changes |
| MSC3255 | ⬛ ● | closed | Use SRV record for homeservers discovery by clients | client-side discovery via SRV; closed proposal |
| MSC3246 | ⬛ ● | open | Audio waveforms (extensible events) | client message-content field; no server role |
| MSC3245 | ⬛ ● | open | Voice messages (using extensible events) | client message type; ruma feature enabled but server has no role |
| MSC3230 | ⬛ ● | open | Spaces top level order | m.space_order is account_data; uses generic API |
| MSC3226 | ⬛ ● | merged | Per-room spell check | per-room spellcheck language is account_data; no server logic |
| MSC3217 | ⬛ ● | open | Clientside hints for a soft kick | m.softkick hint on member event not implemented |
| MSC3215 | ⬛ ● | closed | Aristotle - Moderation in all things | decentralized moderation room scheme not implemented |
| MSC3184 | ⬛ ● | open | Challenges Messages | client-only challenge message types |
| MSC3160 | ⬛ ● | open | Attach timezone metadata to time information in messages | client-only HTML &lt;time&gt; markup in messages |
| MSC3131 | ⬛ ● | open | Verifying with QR codes v2 | client-only QR verification v2 method names |
| MSC3124 | ⬛ ● | closed | Handling spoilers in plain-text message fallback | client-only spoiler fallback handling |
| MSC3122 | ⬛ ● | merged | Deprecate starting key verifications without requesting first | client-only deprecation of to-device verification start |
| MSC3089 | ⬛ ● | open | File trees | client-only data trees on m.space; no server change required |
| MSC3088 | ⬛ ● | open | Room subtyping | client-only m.room.purpose state event; no server change required |
| MSC3086 | ⬛ ● | open | Asserted Identity for VoIP Calls | client VoIP event content; server transparent |
| MSC3077 | ⬛ ● | merged | Support for multi-stream VoIP | merged; sdp_stream_metadata is event content |
| MSC3074 | ⬛ ● | closed | Proposal for URIs conforming to RFC 3986 syntax. | client URI scheme; not a server feature |
| MSC3068 | ⬛ ● | closed | Compliance tiers | informational compliance terminology only |
| MSC3067 | ⬛ ● | closed | Prevent/remove legacy groups from being in the spec | meta MSC; spec-process decision to drop legacy groups |
| MSC3062 | ⬛ ● | open | Bot verification | client-only verification method |
| MSC3061 | ⬛ ● | merged | Sharing room keys for past messages | client-only; sender-flagged room key property |
| MSC3009 | ⬛ ● | open | Websocket transport for client &lt;--&gt; widget communications | client to widget transport; not server-side |
| MSC3008 | ⬛ ● | open | Scoped access for widgets | widget client/UA concern; obsoleted by OIDC scopes |
| MSC2997 | ⬛ ● | open | Add t-shirt | joke proposal; t-shirt design |
| MSC2974 | ⬛ ● | open | Widgets: Re-exchange capabilities | widget-side request_capabilities; client-only |
| MSC2949 | ⬛ ● | open | Proposal to clarify "Requires auth" and "Rate-limited" in the spec | spec-text clarification; no homeserver behavior |
| MSC2931 | ⬛ ● | open | Widget navigate permission | widget navigate capability; client-only |
| MSC2881 | ⬛ ● | closed | Message Attachments | new event content schema (m.attachment relation); generic event passthrough |
| MSC2876 | ⬛ ● | closed | Allowing widgets to read events in a room | widget read_events action; client-only |
| MSC2874 | ⬛ ● | merged | Single SSSS | client interpretation of SSSS default key; account data passthrough |
| MSC2873 | ⬛ ● | open | Identifying clients and user settings in widgets | widget URL template variables and theme_change; client-only |
| MSC2872 | ⬛ ● | open | Move the widget `title` to the root | widget definition field reorder; client-only |
| MSC2871 | ⬛ ● | open | Sending approved capabilities back to the widget | widget-only feature; homeserver not involved |
| MSC2813 | ⬛ ● | open | Handling invalid Widget API requests | client/widget error handling rules |
| MSC2801 | ⬛ ● | merged | Make it explicit that event bodies are untrusted data | spec note: clients should treat events as untrusted |
| MSC2790 | ⬛ ● | open | Widgets - Prompting for user input within the client | client-side widget modal API |
| MSC2781 | ⬛ ● | merged | Remove reply fallbacks from the specification | removes client-side reply fallback; client behavior change |
| MSC2779 | ⬛ ● | closed | Clarify that event IDs are globally unique | spec clarification issue; closed; no server behavior change |
| MSC2775 | ⬛ ◐ | closed | Lazy loading room membership over federation | [→ MSC3706/MSC3902] |
| MSC2774 | ⬛ ● | merged | Giving widgets their ID so they can communicate | client widget URL template variable |
| MSC2771 | ⬛ ● | closed | Bookmarks | client-side bookmarks via account_data; closed |
| MSC2765 | ⬛ ● | merged | Widget avatars | client-side widget definition field |
| MSC2762 | ⬛ ● | open | Allowing widgets to send/receive events | client-side widget API; homeserver not involved |
| MSC2758 | ⬛ ● | merged | Common grammar for textual identifiers | meta grammar guideline for future identifiers; not directly implementable |
| MSC2747 | ⬛ ● | open | Transferring VoIP Calls | Client-only m.call.replaces event semantics |
| MSC2746 | ⬛ ● | merged | Improved Signalling for 1:1 VoIP | Client-side VoIP; HS relays m.call.* and serves unsigned.age on all read paths |
| MSC2723 | ⬛ ● | open | Forwarded message metadata | Client-side m.forwarded content field only |
| MSC2713 | ⬛ ● | merged | Remove deprecated Identity Service endpoints | Identity Service endpoints; not a homeserver feature |
| MSC2644 | ⬛ ● | open | `matrix.to` URI syntax v2 | matrix.to URI syntax; client-only |
| MSC2630 | ⬛ ◐ | merged | Checking public keys in SAS verification | Client SAS verification crypto; server transports key.verification events |
| MSC2618 | ⬛ ● | closed | Helping others with mandatory implementation guides | Spec process MSC; no homeserver behavior |
| MSC2589 | ⬛ ◐ | closed | Improve replies | Client reply rendering; closed MSC; server ignores reply_body fields |
| MSC2582 | ⬛ ◐ | merged | Remove `mimetype` from `EncryptedFile` object | Removes mimetype example from spec; pure spec/client cleanup |
| MSC2557 | ⬛ ◐ | merged | Clarifications on spoilers | Client-only spoiler rendering clarification |
| MSC2545 | ⬛ ◐ | merged | Image Packs (Emoticons &amp; Stickers) | Client pack UX; HS clauses are a federation no-reject plus an optional check |
| MSC2530 | ⬛ ◐ | merged | Body field as media caption | Client rendering of body+filename for media msgtypes |
| MSC2529 | ⬛ ◐ | closed | Use existing m.room.message/m.text events as captions for images | [→ MSC2530] Client-only relation/caption rendering; superseded by MSC2530 |
| MSC2516 | ⬛ ◐ | closed | Add a new message type for voice messages | Client-only msgtype; server does no msgtype-specific handling |
| MSC2475 | ⬛ ○ | closed | API versioning | Spec process meta-MSC about API version naming; closed |
| MSC2474 | ⬛ ◐ | open | Add key backup version to SSSS account data | Client-side SSSS field; server stores account_data opaquely |
| MSC2472 | ⬛ ◐ | merged | Symmetric SSSS | Client-side SSSS crypto; server only stores account_data |
| MSC2461 | ⬛ ◐ | closed | Proposal for Authenticated Content Repository API | [→ MSC3916] |
| MSC2427 | ⬛ ◐ | open | Proposal for JSON-based message formatting | Client-only message formatting alternative to HTML |
| MSC2425 | ⬛ ● | closed | Remove Authentication on /submitToken Identity Service API | Identity Server endpoint; not a homeserver concern |
| MSC2422 | ⬛ ◐ | merged | Allow `color` as attribute for `<font>` in messages | Client HTML sanitizer change for &lt;font color&gt; |
| MSC2399 | ⬛ ◐ | merged | Reporting that decryption keys are withheld | Client-only m.room_key.withheld to-device event |
| MSC2398 | ⬛ ◐ | open | proposal to allow mxc:// in the "a" tag within messages | Client HTML rendering policy for &lt;a href=mxc:&gt; |
| MSC2390 | ⬛ ◐ | closed | On the EDU-to-PDU transition. | Process MSC; closed; recommends no further EDU use |
| MSC2385 | ⬛ ◐ | open | Disable URL Previews, alternative method | Client-only url_previews array on m.room.message |
| MSC2376 | ⬛ ◐ | closed | Disable URL Previews | Client-only HTML attribute hint; server has no role |
| MSC2366 | ⬛ ◐ | merged | Key verification flow additions: `m.key.verification.ready` and `m.key.verifi... | Client-side verification flow over to-device; server transports |
| MSC2359 | ⬛ ◐ | closed | E2E Encrypted SFU VoIP conferencing via Matrix | [→ MSC3401] Architectural sketch for client+SFU; no homeserver requirements |
| MSC2354 | ⬛ ◐ | open | Device to device streaming file transfers | Client-only WebRTC signaling over event types; server transports opaquely |
| MSC2346 | ⬛ ● | open | MSC 2346: Bridge information state event | m.bridge state event; bridge/client concern |
| MSC2324 | ⬛ ● | merged | Facilitating early releases of software dependent on spec | Process change about FCP and stable prefixes |
| MSC2320 | ⬛ ● | merged | Versions information for identity servers | Identity server endpoint, not homeserver |
| MSC2315 | ⬛ ● | open | Allow users to select "none" as an integration manager | Client account_data m.integrations toggle |
| MSC2313 | ⬛ ● | merged | Moderation policies as rooms (ban lists) | State events m.policy.rule.*; no homeserver enforcement |
| MSC2312 | ⬛ ● | merged | URI scheme for Matrix | Client-side URI scheme; no homeserver endpoint required |
| MSC2299 | ⬛ ● | open | Proposal to add m.textfile msgtype | Client-only msgtype m.textfile |
| MSC2291 | ⬛ ● | open | Configuration to Control Crawling | Bot-only advisory state event; no homeserver behavior |
| MSC2284 | ⬛ ● | merged | Making the identity server optional during discovery | Client-side .well-known FAIL_PROMPT behavior |
| MSC2241 | ⬛ ◐ | merged | Key verification in DMs | Client-side verification flow over m.room.message; server passes events trans... |
| MSC2232 | ⬛ ● | closed | Expose Homeserver Email Configuration in Registration Parameters | proposal text is the empty MSC template |
| MSC2230 | ⬛ ◐ | merged | Store Identity Server in Account Data | client behavior over generic account data; HS already supports account data |
| MSC2211 | ⬛ ● | open | Identity Servers Storing Threepid Hashes at Rest | identity server storage details; not HS |
| MSC2192 | ⬛ ● | open | Inline widgets | client extensible event m.embed; no server logic |
| MSC2191 | ⬛ ● | merged | Markup for mathematical messages | client formatted_body rendering only |
| MSC2184 | ⬛ ● | merged | Allow the HTML `<details>` tag in messages | client HTML rendering; no server impact |
| MSC2063 | ⬛ ◐ | closed | Add "server information" public API proposal | closed; no real proposal text (template file only) |
| MSC2010 | ⬛ ● | merged | MSC 2010: Proposal to add client-side spoilers | client-side rendering of data-mx-spoiler in formatted_body |
| MSC1961 | ⬛ ● | merged | Integration manager authentication | merged; integration-manager auth API is on the manager, not homeserver |
| MSC1960 | ⬛ ● | merged | OpenID Connect information exchange for widgets | OpenID Connect exchange for widgets; the new flow is widget-to-client, server... |
| MSC1959 | ⬛ ● | open | Sticker picker API | branch; sticker picker API on integration manager, not homeserver |
| MSC1958 | ⬛ ● | closed | Widget architecture changes | client widget account_data shape; servers do not interpret widget content |
| MSC1956 | ⬛ ● | open | Integrations API | branch; integrations API is integration-manager scope, not homeserver |
| MSC1951 | ⬛ ◐ | closed | Custom emoji and sticker packs in Matrix | branch; client/integration manager concept; uses generic rooms |
| MSC1920 | ⬛ ◐ | closed | Alternative texts for stickers | branch; client-side rendering field on m.sticker; no server logic |
| MSC1902 | ⬛ ● | closed | Splitting the media repo into a client-side and server-side component | [→ MSC3916] |
| MSC1849 | ⬛ ◐ | closed | Proposal for aggregations via relations | [→ MSC2674/MSC2675/MSC2676] |
| MSC1781 | ⬛ ● | open | Proposal for associations for DIDs and DID names | identity-server endpoints for DID validation; not a homeserver concern |
| MSC1779 | ⬛ ● | merged | Proposal for Open Governance of Matrix.org | governance/foundation document; not a homeserver feature |
| MSC1769 | ⬛ ● | open | Proposal for extensible profiles as rooms | branch; profile-as-rooms not implemented |
| MSC1722 | ⬛ ● | closed | Support for displaying math(s) in messages | client-side rendering of MathML in formatted_body; servers do not interpret |
| MSC1719 | ⬛ ● | merged | Olm unwedging | client-only behavior (m.dummy, session re-creation rate-limit) |
| MSC1717 | ⬛ ◐ | merged | Key verification mechanisms | to_device transport carries m.key.verification.* events |
| MSC1716 | ⬛ ● | open | Open on device API | client-only m.openondevice event type; nothing server-side to implement |
| MSC1704 | ⬛ ● | merged | matrix.to permalink navigation | server-side requirement is via= on /join; src/api/client/membership/join.rs:79 |
| MSC1703 | ⬛ ● | closed | encrypting recovery keys for online megolm backups | amendment PR to MSC1687; closed without merge |
| MSC1700 | ⬛ ◐ | closed | Improving .well-known discovery of homeservers | well-known client+server discovery served from config |
| MSC1680 | ⬛ ● | closed | cross-signing of devices to simplify key verification | empty Google-doc stub; cross-signing specified in MSC1756 |
| MSC1544 | ⬛ ● | merged | Key verification using QR codes | amendment PR to MSC1543; no separate proposal text |
| MSC1543 | ⬛ ● | merged | Bi-directional Key verification using QR codes | client-only QR verification over send-to-device; server is opaque |
| MSC1318 | ⬛ ● | closed | Proposal for Open Governance of Matrix.org | [→ MSC1779] governance proposal; not a homeserver feature |
| MSC1310 | ⬛ ● | closed | Proposal for a media information API | empty Google-doc stub; media info API never specified |
| MSC1301 | ⬛ ◐ | closed | Proposal for improving authorization for the matrix profile API | legacy 2018 issue (closed) tracked via redirect; profile-share-room limit not... |
| MSC1286 | ⬛ ● | open | Formally spec an API for interacting with integration managers | legacy 2018 issue tracked via cross-repo redirect; integration manager API is... |
| MSC1267 | ⬛ ● | closed | Interactive key verification using short authentication strings | stub Google doc; SAS verification specified later (MSC2241+); client-only fea... |
| MSC1236 | ⬛ ● | open | Matrix Widget API v2 | legacy 2018 issue tracked via redirect; widget API v2 is a client-side concern |
| MSC1229 | ⬛ ◐ | open | Mitigating abuse of the event depth parameter over federation | legacy 2018 issue tracked via redirect; depth-abuse mitigations not implement... |
| MSC1225 | ⬛ ● | closed | Extensible event types &amp; fallback in Matrix | empty Google-doc stub; extensible events specified later in MSC1767 |
| MSC1215 | ⬛ ● | closed | Groups as Rooms | [→ MSC1772] empty Google-doc stub; groups feature dropped in favor of Spaces |
| MSC971 | ⬛ ● | closed | Add groups stuff to spec | [→ MSC1772] groups stuff superseded by Spaces (MSC1772); proposal is doc link... |
| MSC701 | ⬛ ◐ | open | Auth/linking for content repo (and enforcing GDPR erasure) | legacy 2016 issue tracked via redirect; auth/linking for content repo address... |
| MSC455 | ⬛ ● | closed | Do we want to specify a matrix:// URI scheme for rooms? (SPEC-5) | [→ MSC2312] stub Google doc; matrix:// URI scheme superseded by matrix: URI (... |

