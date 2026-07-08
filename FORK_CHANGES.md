# MindRoom Tuwunel Fork - Changes vs Upstream

This document describes the current MindRoom fork behavior on top of upstream
`tuwunel`. The fork is rebased directly onto upstream commits; see
`docs/rebase-*.md` for the per-rebase log. As of the 2026-07-21 rebase the base
is upstream `v1.8.2`. Four features this fork originally carried were merged
upstream and dropped from the fork: the room-creation default power-level
override and the SSO grant-cookie path hardening (2026-07-07 rebase), and the
drained/zero one-time-key counts fix and public MatrixRTC transport discovery
(2026-07-21 rebase, upstream `164b8da61`/`007033cd5` and `a1776c368`).

## How To Inspect
- Fork commits: `git log --reverse --oneline <upstream-base>..HEAD`
- Files changed in the fork: `git diff --stat <upstream-base>..HEAD`
- Per-commit patch: `git show <sha>`

## Runtime Changes

### 1) `mindroom/edits: compact /sync, purge superseded edits, bundle the survivor`
Files:
- `src/api/client/sync/mod.rs`, `src/api/client/sync/mindroom_edits.rs`
- `src/core/config/mod.rs`, `src/core/config/check/mindroom.rs`, `src/core/config/check.rs`
- `src/core/matrix/event.rs`, `src/core/matrix/event/relation.rs`
- `src/database/map/remove.rs`
- `src/service/edit_purge/mod.rs`, `src/service/mod.rs`, `src/service/services.rs`
- `src/mindroom-tests/tests/edit_purge_bundle_compose.rs`
- `tuwunel-example.toml`

Behavior:
- Adds `/sync` timeline compaction for superseded `m.replace` events.
- Adds a background purge worker that deletes old superseded edit events from
  storage and indexes, keeping exactly one edit per (target, sender).
- Adds the MindRoom edit-lifecycle configuration surface and purge validation.
- **Turns on upstream's edit bundling by default** (`bundle_edit_relations`,
  MSC3925; upstream ships it off). The purge deletes superseded edits, so
  without the bundle a history endpoint (`/messages`, `/context`, `/event`, ...)
  would serve an original with its stale pre-edit body and no way for the client
  to find the surviving edit. Upstream bundles the newest surviving edit onto
  the original at `unsigned.m.relations.m.replace` via its `relatesto_typed`
  typed index; the purge composes with that index (it tolerates the dangling
  rows the purge leaves behind and always selects the surviving edit), and
  upstream's startup `rebuild_relatesto_typed` migration indexes pre-existing
  edits. `edit_purge::purge_cycle` is `pub` so operators/tests can trigger a
  cycle; a composition test drives a real purge and asserts the survivor is
  still bundled and that a dangling newest index row is skipped.

### 2) `auth/sso: SSO-origin UIAA hardening and self-reactivation`
Files:
- `src/api/router/auth/uiaa.rs`, `src/api/client/session/sso.rs`
- `src/api/client/account/deactivate.rs`, `src/api/client/admin/mas/delete_user.rs`
- `src/api/client/admin/users/deactivate_account.rs`, `src/api/client/admin/users/create_or_modify.rs`
- `src/api/client/membership/mod.rs`, `src/api/oidc/account/account_deactivate.rs`
- `src/admin/user/mod.rs`, `src/database/maps.rs`
- `src/service/deactivate/mod.rs`, `src/service/emergency/mod.rs`
- `src/service/users/mod.rs`, `src/service/users/sso.rs`

Behavior:
- Upstream already ships the strict-CSP-safe SSO UIAA fallback itself (MSC2454:
  server-redirect flow, `m.login.sso/fallback/web` completion, bound-IdP
  routing). This fork hardens how UIAA flows are advertised for SSO-origin
  users: no `m.login.password` for passwordless SSO accounts (even with LDAP
  enabled), `m.login.sso` only for SSO-origin accounts and only when the exact
  IdP is unambiguous (the device's own IdP or the single configured provider),
  JWT UIAA rejected for SSO-origin users and no longer advertised (its
  fallback/web page is not implemented), and legacy SSO-origin account
  metadata repaired on the fly.
- Reactivates a deactivated local SSO account on re-login, but only when the
  account was self-deactivated (a persisted deactivation reason distinguishes
  self-service from administrative deactivation).
- Upstream's Synapse admin deactivation endpoints (the v1 deactivate route and
  the v2 create-or-modify `deactivated` flag, new in v1.8.1) record
  `DeactivationReason::Admin`, so accounts they deactivate stay deactivated on
  SSO re-login.

Design note — why deactivation takes a reason (vs Synapse/upstream):
- Synapse models deactivation as a bare `users.deactivated` flag (plus the
  separate, admin-imposed MSC3823 `suspended` flag); it stores no reason,
  hard-blocks SSO login for deactivated accounts (the
  `sso_account_deactivated_template` 403 page in `auth.py`), and its
  admin-only `activate_account` expects a password hash to be set afterwards,
  which passwordless SSO accounts don't have.
- Upstream Tuwunel likewise stores no reason; since v1.8.1 an admin can
  reactivate a passwordless user via the password sentinel, but neither
  server has a self-service path.
- The fork persists the initiator (`self`/`admin`) so *self*-deactivated SSO
  accounts can safely self-reactivate when the same IdP identity returns,
  while admin deactivations stay final. The reason is a required parameter of
  `deactivate_account`, so each new upstream call site must classify itself
  at compile time — this is the recurring (and intentional) rebase seam; the
  v1.8.1 rebase adapted two new Synapse-admin endpoints exactly this way,
  and the v1.8.2 window added no new callers.
- Deliberately not upstreamed: reversible deactivation diverges from
  Synapse-established semantics and would effectively need an MSC, so we
  carry it as a fork feature. The full comparison lives in
  `src/service/users/sso.rs` above `DeactivationReason`.

Note: the SSO grant-cookie path hardening this fork originally carried (matching
set and removal cookie paths) was merged upstream, so it is no longer a fork
delta.

### 3) `auth/apple: native iOS Apple login exchange`
Files:
- `src/api/client/session/mod.rs`, `src/api/client/session/sso.rs`
- `src/api/client/session/sso/native_apple.rs`
- `src/api/router.rs`, `src/core/config/mod.rs`, `tuwunel-example.toml`

Behavior:
- Adds `POST /_matrix/client/unstable/org.mindroom.login/apple`.
- Verifies native Sign in with Apple identity tokens against Apple's JWKS,
  issuer, audience, expiration, and nonce (with a brief in-memory JWKS cache
  that refreshes on an unknown key ID).
- Accepts configured native app bundle IDs via
  `global.identity_provider.native_client_ids` while keeping the web Services ID
  valid; reuses the normal SSO mapping/registration/reactivation/loginToken
  path.

Note: the Apple `id_token` userinfo fallback that this fork originally carried
was merged upstream, so it is no longer a fork delta.

### 4) `Notify once when MindRoom streams finish (#9)`
Files:
- `src/service/pusher/mod.rs`, `src/service/pusher/send.rs`
- `src/service/pusher/tests.rs`

Behavior:
- Recognizes MindRoom streamed events by the `io.mindroom.stream_status`
  content key (an event-level protocol signal: senders opt their own streamed
  events in; no account privilege involved).
- Suppresses push notifications for stream updates in non-terminal states, so
  a streaming agent reply does not notify once per chunk. Unknown stream
  statuses fail closed (suppressed) so a new producer state cannot
  reintroduce push spam.
- When the stream reaches a terminal status (`completed`, `cancelled`,
  `interrupted`, `error`) via an `m.replace` edit, evaluates the surviving
  message as the final content it represents: for `m.room.message` the
  `m.new_content` body, for `m.room.encrypted` the payload without
  `m.relates_to`. Ordinary room/DM/mention/mute push rules then decide the
  single notification instead of the generic edit-suppression rule swallowing
  the terminal update.
- The `push_everything` debug mode honors the same suppression.

### 5) `fix(e2ee): reject replacement of existing device identity keys (#10)`
Files:
- `src/api/client/keys/upload_keys.rs`, `src/service/users/device.rs`

Behavior:
- Treats identity keys for an existing device as immutable in `/keys/upload`:
  the first upload is accepted, an exact-copy re-upload is ignored (upstream's
  nheko workaround, preserving cross-signing signatures), and differing key
  material is rejected with 403 `M_FORBIDDEN` so a client that lost its crypto
  store fails loudly and re-logins instead of becoming a zombie session
  (root cause of the 2026-07-14 mindroom.chat silent-call incident; unpatched
  upstream accepts the rotated upload).
- A stored value that fails to deserialize propagates a Database error rather
  than being treated as absent (absence would bypass the immutability check).
- `remove_device` also purges the device's uploaded identity keys and one-time
  keys (resolving upstream's standing TODO); leftover keys would otherwise
  collide with the immutability check when a later login re-uses the device
  id.
- Submitted upstream (`fix/reject-device-key-replacement` and follow-ups);
  drop this section when it merges, as happened with the one-time-key and
  MatrixRTC fixes at v1.8.2.

## Operational Changes

### 6) `ci: fork release automation, container publishing, and GitHub checks`
Files:
- `.github/workflows/mindroom-release.yml`, `.github/workflows/auto-mindroom-release.yml`
- `.github/workflows/mindroom-container-release.yml`, `.github/workflows/mindroom-ci.yml`
- `scripts/fork_release_tag.py`, `docker/bake.sh`

Behavior:
- Computes `v<base_version>-mindroom.<n>` tags on `main`, creates/reuses the
  matching GitHub Release, publishes Linux `x86_64`/`aarch64` binaries, and
  dispatches container publication. Runs the fork's own GitHub-hosted checks.

## Tests

Fork integration tests live in the `mindroom-tests` crate
(`src/mindroom-tests/`), plus `default_test` database-path isolation in
`src/main/args.rs`. They pin the rebase-sensitive behaviors (SSO/UIAA, native
Apple, deactivation/erase, Synapse-admin deactivation reason, edit-purge ↔
bundling composition) so future rebases catch regressions. The stream-push
classification (#9) is pinned by unit tests in `src/service/pusher/tests.rs`.

## Runtime Configuration

### Edit compaction, purge, and bundling
```toml
[global]
mindroom_compact_edits_enabled = true
mindroom_edit_purge_enabled = true
mindroom_edit_purge_min_age_secs = 86400
mindroom_edit_purge_interval_secs = 3600
mindroom_edit_purge_batch_size = 1000
mindroom_edit_purge_scan_limit = 100000
mindroom_edit_purge_dry_run = false
# bundle_edit_relations defaults to true in the fork; set false only to opt out.
```

### Native Sign in with Apple
```toml
[[global.identity_provider]]
brand = "AppleOIDC"
client_id = "chat.mindroom.matrix.apple"
native_client_ids = ["chat.mindroom.app"]
```

## Compatibility Notes
- Matrix event formats remain standard. With edit bundling on, served events
  (including `/sync`) may carry `unsigned.m.relations.m.replace` (the newest
  surviving edit, as a sync-shaped event without `room_id`); the fork's `/sync`
  compaction still delivers the surviving edit event itself.
- Superseded edits can be permanently removed when purge is enabled; the bundle
  compensates so history endpoints never serve stale pre-edit bodies.
- Admin-deactivated SSO accounts stay deactivated on future login attempts.
- Native Apple login requires the app bundle ID in `native_client_ids`.
- A client that lost its crypto store but kept its access token receives 403
  `M_FORBIDDEN` on `/keys/upload` until it logs in again (device identity keys
  are immutable per device id).
- Events carrying `io.mindroom.stream_status` push at most once, when the
  stream reaches a terminal status.
