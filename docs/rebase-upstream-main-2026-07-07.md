# Rebase onto upstream `main` - 2026-07-07

## Goal

Rebase the MindRoom fork onto the latest upstream `main` tip and **drop or
reconcile the fork deltas that upstream has since merged**. Two fork PRs landed
upstream after the 2026-07-05 rebase:

- **#496** — the room-creation default power-level override
  (upstream `c4837cfc` + `730fbcbc`, merged via `5166c531` from the fork branch
  `mindroom/upstream-pr-power-level-defaults`).
- **#497** — the grant-session cookie removal with a matching path attribute
  (upstream `ec740593`, merged via `38911216` from the fork branch
  `mindroom/upstream-pr-grant-cookie-removal-path`).

So the power-level commit is dropped wholesale, and the Apple commit is updated
to drop its grant-cookie hardening hunk (adopting upstream's exact-path
implementation) while keeping the native Apple login. History rewrite is
explicitly allowed for this fork; the story stays a small set of logical
commits.

## Baseline

- Fork tip of record: `origin/main` = `c0146fcf`
  (`docs: fork runtime and release overview`).
- Fork base before this rebase: upstream `main` `59f13052`
  (the 2026-07-05 rebase target).
- Upstream target: `upstream/main` tip `2323a8ec`.
- Upstream delta: `59f13052..2323a8ec` = 63 commits.

Notable upstream commits in that delta:
- `c4837cfc` client/room: Apply a configurable default power-level override —
  **this is the fork's own PR #496**; supersedes fork commit `09cf1668`.
- `730fbcbc` client/room: Inline the power-level override merge helper.
- `ec740593` oauth: Remove the grant-session cookie with a matching path
  attribute — **this is the fork's own PR #497**; supersedes the fork's
  grant-cookie hardening.
- `824fcded` oidc: Promote the login-token and SSO-redirect helpers to the
  module root (SSO callback restructure).
- `34ac2a7f` service/users: Add the MSC4025 erasure marker with admin surfacing
  — adds `userid_erased` to the `users` Data struct (source of the sole
  `service/users/mod.rs` conflict).
- MSC3856 threaded-read and MSC3440 threading series.
- Rust toolchain stays `1.95.0` (`rust-version = "1.95.0"`).

## Dropped / reconciled fork commits

The fork carried 7 commits (`59f13052..c0146fcf`). Result: 6 commits.

- **DROPPED** `09cf1668` `config/rooms: default room power-level override` —
  merged upstream as #496 (`c4837cfc`). Dropping it cleanly leaves the edit
  commit's purge validations in `src/core/config/check.rs` (the power-level
  commit had moved them into `check/mindroom.rs`; that move is gone with it).
- **UPDATED** `3d7a9e43` → `0af10c1a` `auth/apple: native iOS Apple login
  exchange` — the grant-cookie path hardening was merged upstream as #497
  (`ec740593`), so the fork's broaden variant (`GRANT_SESSION_COOKIE_PATH`
  const + the `/_matrix/client/`-widening function + its two unit tests) is
  dropped and upstream's exact-path `grant_session_cookie_path` is kept as-is.
  The native Apple login endpoint, JWKS validation, `native_client_ids`, and the
  `complete_sso_session` extraction are all kept. Commit subject/body updated to
  drop the "SSO cookie hardening" claim.
- **UPDATED** `e6516960` → `2ef63bf1` `tests: integration coverage pinning fork
  behaviors` — dropped `src/mindroom-tests/tests/power_level_override.rs`
  (146 lines): power-level is no longer a fork behavior, so it no longer belongs
  in the fork's behavior-pinning suite. Commit message's power-level mention
  removed.
- KEPT (replayed) `c4e57cc7` → `5ad18572` edits, `9545283c` → `64f89adb` sso,
  `b8eed4f2` → `5758e864` ci, `c0146fcf` → docs (FORK_CHANGES/README updated).

## Conflicts resolved

- `src/service/users/mod.rs` — additive: upstream added `userid_erased`
  (MSC4025) at the same spot the fork adds `userid_deactivation_reason`.
  Resolved as a union (both maps, in struct + `build`).
- `src/api/client/session/sso.rs` — the grant-cookie reconciliation. The
  auto-merge was a garbled function-boundary interleave, so the file was reset
  to upstream's clean version (`:2`, which already carries #497's exact
  `grant_session_cookie_path` and, from #495, the local
  `decode_apple_userinfo_from_id_token`) and the native-Apple deltas were
  re-applied surgically:
  - add `mod native_apple;` and `native_apple_login_route` re-export;
  - move the id_token decode into `native_apple::decode_userinfo_from_id_token`
    (remove the local copy + its `serde_json::Value` import + its four unit
    tests, all now living in `native_apple.rs`);
  - extract `complete_sso_session` from the callback so the native endpoint can
    reuse it — **rebuilt from upstream's *current* callback**, preserving the
    new upstream `origin`/`ldap_user_exists` registration logic that postdates
    the fork's original extraction.
  Upstream's exact grant-cookie handling and its `grant_session_cookie_path`
  tests are kept unchanged.

## Safety refs

- `backup/origin-main-before-rebase-20260707` = `c0146fcf` (published fork tip).
- `backup/upstream-main-target-20260707` = `2323a8ec`.
- `backup/rebased-upstream-main-20260707` = the first rebased tip before the
  test-removal / message-reword cleanups.

Local `main` was reset back to `origin/main` (`c0146fcf`) after `--update-refs`
dragged it onto the rebased tip; the rebase lives on branch
`mindroom/rebase-upstream-main-20260707` and is fast-forwarded onto `main` only
after validation.

## Validation

- Toolchain: sanitized Rust `1.95.0` wrapper (nix-store cargo/rustc/clippy 1.95,
  nightly rustfmt), `RUSTC_WRAPPER=` unset, separate
  `CARGO_TARGET_DIR=target-ci195`. (liburing re-pinned 2.12→2.14 and clang
  21.1.2→21.1.8 after a nix GC.)
- `cargo check --workspace --all-targets` — clean.
- `cargo clippy --workspace --all-targets` and `cargo test` — see session log.

## Post-rebase amendments (same session)

Intermediate commit shas above drifted with these rewrites; the commit
*subjects* are the stable identifiers.

- Added `src/mindroom-tests/tests/sso_callback_completion.rs`, pinning the
  `complete_sso_session` branches the new-user happy path does not reach
  (identity reuse + previous-session deletion, admin-deactivated rejection,
  self-deactivated reactivation); folded into the tests commit.
- Reworded the sso commit to `auth/sso: SSO-origin UIAA hardening and
  self-reactivation`: upstream ships the strict-CSP SSO UIAA fallback itself
  (MSC2454 — `11309062`, `a87b8bad`, `3127eca6`, `78e6af7b`, `bdad6af8`), so
  the fork commit now claims only its real deltas — UIAA flow-advertising
  hardening for SSO-origin users plus self-reactivation with a persisted
  deactivation reason. FORK_CHANGES and README were updated to match (tree
  unchanged; message/docs only).
