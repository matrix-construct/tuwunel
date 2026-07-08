# Rebase on upstream v1.7.1 - 2026-06-12

## Goal

Rebase the MindRoom fork onto upstream `v1.7.1` with explicit overlap review, conflict-by-conflict notes, and full test runs after each conflict resolution before continuing the rebase.

## Baseline

- Fork tip of record: `origin/main` = `d64264da` (`ci: add GitHub-hosted MindRoom checks (#5)`).
- Fork base before this rebase: `v1.7.0` (commit `01f56a9d`, `Bump 1.7.0.`); `origin/main` = `v1.7.0` + 19 fork commits, linear history.
- Upstream target: `v1.7.1` = `ba56af22` (tagged 2026-06-05, latest published GitHub release as of 2026-06-12).
- Upstream delta: `v1.7.0..v1.7.1` = 147 commits.
- Local `main` at session start was `bbb48ce3`, a stale leftover of the v1.6.1 rebase session (2026-05-04) that was superseded by later force-pushes to `origin/main`; it was reset to `origin/main` after backing it up.
- `git cherry` patch-id check: pending (see Overlap Review).

## Safety Refs

- `backup/main-stale-v1.6.1-rebase-20260612` = `bbb48ce3` preserves the stale local branch found at session start.
- `backup/origin-main-before-rebase-v1.7.1-20260612` = `d64264da` preserves the fork tip this rebase starts from.

## Test Command

Current full local test command for conflict gates:

```bash
env -u TUWUNEL_DATABASE_PATH \
  -u LD_PRELOAD \
  -u NIX_CFLAGS_LINK -u NIX_CFLAGS_COMPILE -u NIX_LDFLAGS -u NIX_CC -u NIX_BINTOOLS \
  -u CC -u CXX -u LD -u AR -u AS -u RANLIB \
  -u CARGO_BUILD_TARGET -u CARGO_BUILD_RUSTFLAGS -u CARGO_PROFILE \
  -u JEMALLOC_OVERRIDE -u ROCKSDB_LIB_DIR -u cmakeFlags \
  RUSTC_WRAPPER= \
  PKG_CONFIG_PATH=/nix/store/kic9mkwsq7xgrlxzzarrd7k0k83qnhik-liburing-2.12-dev/lib/pkgconfig \
  CFLAGS=-I/nix/store/kic9mkwsq7xgrlxzzarrd7k0k83qnhik-liburing-2.12-dev/include \
  CXXFLAGS=-I/nix/store/kic9mkwsq7xgrlxzzarrd7k0k83qnhik-liburing-2.12-dev/include \
  LD_LIBRARY_PATH=/nix/store/hczbpz0rrrn86nvphr2zv9fx4qm8isb2-liburing-2.12/lib:$LD_LIBRARY_PATH \
  RUST_TEST_THREADS=1 \
  cargo test --workspace --all-targets
```

This is the v1.6.1-session command (same rationale: the inherited shell exports Nix musl/static cross-compile variables that segfault build scripts; `rust-librocksdb-sys` needs explicit liburing paths; `RUST_TEST_THREADS=1` avoids RocksDB lock contention; `TUWUNEL_DATABASE_PATH` must be unset because Figment env overrides beat test-local TOML database paths) with one new addition:

- `RUSTC_WRAPPER=` (empty) — new this session. The repo's `.cargo/config.toml` sets `build.rustc-wrapper = "rustc-wrapper"`, which resolves to sccache. sccache 0.12.0 in the current dev shell crashes with `GLIBC_ABI_DT_X86_64_PLT not found` (it was built against glibc 2.42 while `LD_LIBRARY_PATH` carries the nix-ld glibc 2.40 directory), failing every crate compile. An empty `RUSTC_WRAPPER` env var overrides the config and disables the wrapper.

The liburing Nix store paths from the v1.6.1 notes still exist and were reused verbatim.

## Running Notes

- 2026-06-12: session start. Confirmed `v1.7.1` is the latest upstream release (`gh release list`). Confirmed linear fork history on top of `v1.7.0`. Created safety refs. Reset local `main` to `origin/main`.

## Overlap Review

Completed before starting the rebase.

Commands used:

```bash
git diff --stat --find-renames 01f56a9d..origin/main      # fork side
git diff --stat --find-renames 01f56a9d..v1.7.1           # upstream side
git diff --name-only 01f56a9d..origin/main | sort > /tmp/fork-files.txt
git diff --name-only 01f56a9d..v1.7.1 | sort > /tmp/upstream-files.txt
comm -12 /tmp/fork-files.txt /tmp/upstream-files.txt
git cherry -v v1.7.1 origin/main
```

Patch-id check:

- `git cherry -v v1.7.1 origin/main` shows all 19 fork commits as `+`; no fork commit was directly included upstream.

Fork diff summary (`v1.7.0..origin/main`):

- 47 files changed, 5797 insertions, 149 deletions.
- Runtime areas: edit compaction/purge, default room power-level override, SSO self-reactivation with deactivation-reason tracking, Apple `id_token` userinfo fallback, native Apple login exchange (`/_matrix/client/unstable/org.mindroom.login/apple`), SSO grant-cookie path hardening, strict-CSP SSO UIAA fallback.
- Test areas: `src/mindroom-tests` rebase regression crate, isolated `default_test` database paths.
- Operational areas: fork release/CI workflows (`mindroom-*.yml`, disabled upstream push/PR triggers in `main.yml`), fork docs.
- Compared to the v1.6.1-era fork, much more code now lives in fork-only modules (`mindroom_power_levels.rs`, `native_apple.rs`, `mindroom_edits.rs`, `src/service/edit_purge/`, `src/service/users/sso.rs`, `src/core/config/check/mindroom.rs`, `src/mindroom-tests/`), which shrinks the conflict surface.

Upstream diff summary (`v1.7.0..v1.7.1`, 147 commits):

- 540 files changed, 23950 insertions, 12394 deletions.
- Dominant theme: large file-splitting refactors — admin command files split into per-handler modules, `api/client` multi-endpoint files split, `sso_callback_route` split into helpers (`validate_session_cookie`, `apply_token_response`, `existing_identity_session`, `chain_next_idp_url`, `finalize_login_redirect`), `create_room_route` split into helpers (`apply_power_levels_pdu`, `build_power_levels_users`, ...).
- Other areas: new `service/fetcher` (10 commits), OIDC token-endpoint hardening and refresh-token lifecycle (6), multiple access tokens per device, sync v5, MSC4380 invite suppression, membership/event_handler/federation changes, CI and Complement work.

Same-file overlap (18 files):

- `Cargo.lock` (regenerate, do not hand-merge)
- `.github/workflows/main.yml`
- `src/admin/user/commands.rs`
- `src/api/client/account.rs`
- `src/api/client/room/create.rs`
- `src/api/client/session/mod.rs`
- `src/api/client/session/sso.rs`
- `src/api/router.rs`
- `src/core/config/check.rs`
- `src/core/config/mod.rs`
- `src/core/matrix/event.rs`
- `src/database/maps.rs`
- `src/main/args.rs`
- `src/service/media/mod.rs`
- `src/service/mod.rs`
- `src/service/services.rs`
- `src/service/users/mod.rs`
- `tuwunel-example.toml`

Functional overlap and expected resolution:

- `src/api/client/session/sso.rs` is the highest-risk overlap. Upstream split `sso_callback_route` into helpers covering the same region the fork refactored into its own `complete_sso_session` helper (identity-session dedup, registration, activation check). Resolution: adopt upstream's helper structure and weave fork behavior in — Apple userinfo `id_token` fallback around `request_userinfo`, `id_token` persisted on the session, `grant_session_cookie_path` hardening on both set and removal cookies, legacy SSO-origin repair, SSO self-reactivation, and the `native_apple` module hookup/re-export.
- `src/admin/user/commands.rs`: upstream split every admin command file into per-handler modules (-933 lines). The fork's `DeactivationReason::Admin` arguments must be re-applied in whichever new per-handler module now holds `deactivate_user`/`deactivate_all`.
- `src/api/client/account.rs`: upstream split multi-endpoint units (-162 lines); the fork's `DeactivationReason::SelfService` argument and doc tweak must follow `deactivate_route` to its new home.
- `src/api/client/room/create.rs`: upstream split `create_room_route` into helpers; `default_power_levels_content` is now called from `apply_power_levels_pdu`. The fork's `mindroom_power_levels` submodule and the default-override merge parameter must be re-threaded through the new helper chain.
- `src/service/users/mod.rs`: keep upstream OIDC refresh-token/multi-access-token changes plus fork `sso` submodule, `userid_deactivation_reason` map, `deactivate_account(reason)`, `set_origin`, and sentinel-password origin-preservation logic in `set_password`.
- `src/main/args.rs`: keep upstream's new args plus the fork's per-test unique `database_path` in `default_test`.
- Config/example: additive merge — upstream added ~474 lines of options; fork adds `default_power_level_content_override` and `mindroom_*` options plus the `check/mindroom.rs` hook in `check.rs`.
- `src/database/maps.rs`: keep upstream's new maps plus fork `userid_deactivation_reason` descriptor.
- `src/service/{mod,services}.rs`: additive — register fork `edit_purge` service alongside upstream's new services.
- `src/service/media/mod.rs`: keep upstream changes plus fork `mxc_is_owned_by_user`/`delete_owned_by` helpers.
- `src/core/matrix/event.rs`: one-line re-export union (fork adds `ExtractRelatesToInfo`, `RelatesToInfo`).
- `.github/workflows/main.yml`: keep fork's removal of push/PR triggers (fork runs its own `mindroom-ci.yml`); take upstream's other changes.
- `Cargo.lock`: take upstream's, then regenerate so fork-only deps (e.g. for `mindroom-tests`/native Apple JWT) are re-added by cargo.
- Edit purge vs upstream timeline changes: upstream touched `service/rooms/timeline` (2 commits); verify the fork's purge still deletes every index it expects (including `roomid_ts_pducount` handling added during the v1.6.1 rebase) — the `mindroom-tests` regression crate plus edit-purge unit tests gate this.

## Conflict Log

- Stop 1: commit `87e98323` (`mindroom/edits: compact /sync and purge superseded edits`)
  - Conflict: `src/service/services.rs` (import list only).
  - Cause: upstream added the new `fetcher` service to the same `use crate::{...}` list where the fork adds `edit_purge`.
  - Resolution: union of both imports. The `Services` struct field, `build()` entry, and `cast!` list for both `edit_purge` and `fetcher` had already auto-merged cleanly.
  - Test gate: passed (433 passed / 0 failed, log `/tmp/rebase-stop1-test.log`).
- Stop 2: commit `afa49ef3` (`oauth: fall back to Apple id_token claims when userinfo fails`)
  - Conflict: `src/api/client/session/sso.rs`.
  - Cause: upstream's `0d051f74` split `sso_callback_route` into helpers; the token-response-to-session block the fork had patched (to persist `id_token`) became upstream's `apply_token_response` helper. The fork's `.or_else` Apple-fallback around `request_userinfo` auto-merged cleanly against the new structure.
  - Resolution: dropped the fork's now-superseded inline token-response block and instead added `id_token: token.id_token` to upstream's `apply_token_response`, preserving the fork behavior (the fallback decodes claims from `session.id_token`, so it must be persisted before `request_userinfo`). `src/service/oauth/sessions.rs` (`Session.id_token` field) auto-merged.
  - Test gate: passed (437 passed / 0 failed, log `/tmp/rebase-stop2-test.log`).
- Stop 3: commit `a06a5792` (`users/sso: reactivate self-deactivated accounts on login`)
  - Conflicts: modify/delete on `src/admin/user/commands.rs` and `src/api/client/account.rs` (both deleted upstream by the per-handler module splits `1f7b15dc` and `d8d7d401`).
  - Resolution: accepted upstream deletions (`git rm` both files) and re-applied the fork's edits at the handlers' new homes:
    - `src/admin/user/mod.rs`: `deactivate_user` helper now passes `DeactivationReason::Admin` to `full_deactivate`/`deactivate_account`, plus the `DeactivationReason` import.
    - `src/api/client/account/deactivate.rs`: `deactivate_route` passes `DeactivationReason::SelfService`, plus the SSO self-reactivation doc note.
  - New upstream call site adapted: `src/api/oidc/account/account_deactivate.rs` (OIDC account-management page, new in v1.7.1) calls `deactivate_account`; gave it `DeactivationReason::SelfService` since it is a user-initiated deactivation, matching the fork's treatment of the Matrix self-deactivation endpoint.
  - Auto-merged cleanly: `src/service/users/mod.rs` (deactivation-reason map + sentinel-password origin preservation alongside upstream's OIDC token work), `src/database/maps.rs`, `src/api/client/session/sso.rs`, fork-only `src/service/users/sso.rs`, `deactivate`/`emergency`/`membership` call-site updates.
  - Test gate: passed (446 passed / 0 failed, log `/tmp/rebase-stop3-test.log`).
- Stop 4: commit `e750cbca` (`Add default room power level override`)
  - Conflicts: `src/api/client/room/create.rs`, `src/core/config/check.rs`.
  - Cause (create.rs): upstream `20615234` replaced the inline power-levels block in `create_room_route` with an `apply_power_levels_pdu` helper; the fork had patched that inline block to pass the configured default override into `default_power_levels_content`.
  - Resolution (create.rs): kept upstream's `apply_power_levels_pdu(...)` call in `create_room_route`; the fork's signature change and `merge_power_level_content_override` calls inside `default_power_levels_content` auto-merged, so the only manual change was passing `services.config.default_power_level_content_override.as_ref()` from `apply_power_levels_pdu`. The fork's `mindroom_power_levels` submodule landed untouched.
  - Cause (check.rs): upstream added `fs::read_to_string` to the same `use std::{...}` line where the fork inserts `mod mindroom;`.
  - Resolution (check.rs): kept both; the fork's `mindroom::check(config)?` call auto-merged.
  - Test gate: passed (449 passed / 0 failed, log `/tmp/rebase-stop4-test.log`).
- Stop 5: commit `31adcdb0` (`auth: add native Apple login exchange (#2)`)
  - Conflict: `src/api/client/session/sso.rs` (two hunks).
  - Cause: this fork commit extracted the post-userinfo logic of `sso_callback_route` into `complete_sso_session` (shared with the new native Apple route) and removed the `ensure_sso_account_active` helper from the earlier reactivation commit; upstream had restructured the same region into its own helpers.
  - Resolution:
    - Import hunk: union (`Json`/`IntoResponse` for the native route + upstream's `CookieJar`).
    - Body hunk: took the fork's `complete_sso_session(...)` call, replacing the inline block (whose `ensure_sso_account_active` definition the commit had already auto-removed).
    - Divergence reduction: inside the auto-merged `complete_sso_session`, replaced the fork's inlined `get_by_unique_id` match with upstream's semantically identical `existing_identity_session` helper, which would otherwise have become dead code.
  - Test gate: passed (463 passed / 0 failed, log `/tmp/rebase-stop5-test.log`).
- Commit `9948d441` (`auth/apple: address native login review (#3)`) applied cleanly, creating `src/api/client/session/sso/native_apple.rs`.
- Stop 6: commit `e9c25c7c` (`Harden SSO grant cookie path (#4)`)
  - Conflict: `src/api/client/session/sso.rs` (import lines only).
  - Cause: commit #3 had moved the native Apple route (and its `Json`/`IntoResponse` imports) out to `native_apple.rs`, so this commit's import context disagreed with the post-Stop-5 merged imports; upstream's `CookieJar` (used by `validate_session_cookie`) sat on the same line.
  - Resolution: fork-side imports plus `CookieJar`. Verified `Json`/`IntoResponse` are no longer referenced in `sso.rs` and that `grant_session_cookie_path` is applied to both the set and removal cookie sites.
  - Test gate: passed (474 passed / 0 failed, log `/tmp/rebase-stop6-test.log`).
- Remaining commits (`Add rebase regression tests`, `ci: add GitHub-hosted MindRoom checks (#5)`) applied cleanly; the rebase finished without further stops.

## Test Log

- Baseline fork-tip test (first attempt, v1.6.1-era command without `RUSTC_WRAPPER=`):
  - Result: failed before project compilation; every dependency crate failed with sccache `GLIBC_ABI_DT_X86_64_PLT not found` errors via the repo's `.cargo/config.toml` `rustc-wrapper`.
- Baseline fork-tip test at `origin/main` (= `d64264da`) with the current command (sccache disabled):
  - Result: passed; log at `/tmp/rebase-v171-baseline-test.log`.
  - Summary: workspace/all-targets, 395 passed / 0 failed / 4 ignored total, including 200 passed / 2 ignored in `tuwunel_service` and the two `mindroom_rebase_tests` integration tests.

## Rebase Result

- Rebase completed successfully: `main` = `141c295c` (`ci: add GitHub-hosted MindRoom checks (#5)`) = upstream `v1.7.1` + the 19 fork commits, linear, no fixups needed.
- `--update-refs` (from git config) moved `backup/origin-main-before-rebase-v1.7.1-20260612` during the rebase, same as in the v1.6.1 session; it was restored to `d64264da` immediately afterwards.
- `git rerere` recorded preimages/resolutions for the conflicted files during the rebase.
- Not pushed: `origin/main` still points at `d64264da`; publishing the rebase requires a force-push, which was deliberately left as a separate step.

### Post-Rebase Verification

- Final tip test run: passed — 476 passed / 0 failed / 4 ignored across workspace/all-targets, log `/tmp/rebase-v171-final-test.log`. Includes the `mindroom_rebase_tests` integration tests and all edit-purge/SSO unit tests.
- `cargo fmt --check`: clean at the final tip (no autosquash or per-commit fixups were required this time).
- `git diff --check`: clean; no conflict markers anywhere in `src/`.
- Fork-delta cross-check (interdiff style):
  - Fork-only files: byte-identical between `origin/main` and rebased `main`.
  - Changed-file set `v1.7.1..main` vs `v1.7.0..origin/main` differs only by the three intentional adaptations: `src/admin/user/commands.rs` → `src/admin/user/mod.rs`, `src/api/client/account.rs` → `src/api/client/account/deactivate.rs`, plus new coverage of `src/api/oidc/account/account_deactivate.rs`.
  - Per-file fork deltas in all other overlap files are line-for-line identical (`src/service/services.rs` differs only by upstream `fetcher` context lines).
  - `Cargo.lock` fork delta is exactly the `mindroom_rebase_tests` package entry.
  - Spot-checked at the final tip: `id_token` persisted in `apply_token_response`, Apple `id_token` userinfo fallback, `grant_session_cookie_path` on both cookie sites, `maybe_repair_legacy_sso_origin`, `maybe_reactivate_deactivated_sso`, `native_apple_login_route` export and `org.mindroom.login/apple` route, `mindroom::check` config hook, `default_power_level_content_override` threading via `apply_power_levels_pdu`.
- Remaining working tree items: this note and the previous session's note are untracked at `docs/rebase-v1.7.1-2026-06-12.md` and `docs/rebase-v1.6.1-2026-05-04.md`.

## Per-Commit Validation Pass

Run after the initial rebase, prompted by review: the conflict-stop gates had only covered 6 of the 19 commits plus the tip, and clippy had not been run at all.

- Clippy at the rebased tip with the fork CI's exact flags (`cargo clippy --locked --workspace --all-targets --all-features -- -D warnings`) found one violation: fork-only `edit_purge::purge_cycle` (179 lines) trips `clippy::too_many_lines` because upstream lowered `too-many-lines-threshold` in `clippy.toml` from 200 to 150 in `v1.7.1`. The function itself is unchanged from `origin/main`.
  - Fix: `#[allow(clippy::too_many_lines)]` on `purge_cycle` (behavior-preserving; splitting the function is a possible follow-up), committed as `fixup!` into the introducing edit-purge commit.
- Per-commit validation: `GIT_SEQUENCE_EDITOR=: git rebase -i --autosquash --reschedule-failed-exec --exec 'bash /tmp/percommit-gate.sh' v1.7.1`, where the gate runs `cargo fmt --check` plus the full sanitized workspace test command, logs per commit at `/tmp/rebase-exec-<shortsha>.log`.
  - Result: all 19 gates passed on the first pass; no fmt or test fixups were needed at any commit.
- Rewritten `main` tip: `4183a609` (`ci: add GitHub-hosted MindRoom checks (#5)`); diff vs the pre-validation tip is exactly the one `#[allow]` line; `git log --grep='fixup!' v1.7.1..main` is empty; backup refs unmoved.
- Final tip CI-parity checks, all passed:
  - `cargo clippy --locked --workspace --all-targets --all-features -- -D warnings` (log `/tmp/rebase-v171-clippy2.log`).
  - `cargo test --locked --workspace --all-targets --all-features`: 476 passed / 0 failed / 4 ignored (log `/tmp/rebase-v171-allfeatures-test.log`).
- Not run locally: the CI `cargo build --locked --release -p tuwunel` step (long LTO build); it is covered by the fork's GitHub-hosted CI and release workflows on push.

## Post-Rebase Test Hardening

Added after the per-commit validation pass, as commit `97ad1e54` (`tests: pin rebase-sensitive SSO and power-level behaviors`) on top of the rebased stack. Rationale: the riskiest rebase resolutions were semantic — code that compiles either way but only behaves correctly one way — and three of them had no test coverage:

- `apply_token_response` id_token persistence (the Stop-2 resolution). New unit test in `src/api/client/session/sso.rs` (`apply_token_response_persists_id_token_for_userinfo_fallback`). Previously, dropping the `id_token: token.id_token` line compiled cleanly and broke Apple fallback only when Apple's userinfo endpoint failed in production.
- Config-to-room-state threading of `default_power_level_content_override` (the Stop-4 resolution). New end-to-end test `src/mindroom-tests/tests/power_level_override.rs` creates real rooms through the router and asserts the override lands in `m.room.power_levels` state, request override still winning. The existing `mindroom_power_levels.rs` unit tests call `default_power_levels_content` directly and would not catch a future call site passing `None`.
- The `appleoidc` userinfo fallback wiring in `sso_callback_route` (Stop 2/5 region). New end-to-end test `src/mindroom-tests/tests/apple_userinfo_fallback.rs` drives the full redirect → callback flow against a mock IdP (discovery + token endpoint serving an unsigned id_token, userinfo returning 500) and asserts login completes, the user registers with `sso` origin, and the session retains the id_token.
- Harness change: generic `mock_server(Router)` helper in `tests/support/mod.rs`; `base64` added as a `mindroom-tests` dev-dependency.

Already covered before this pass (no new tests needed): power-level merge semantics (`mindroom_power_levels.rs` unit tests), all three deactivation-reason reactivation policies (`src/service/users/sso.rs` service tests), edit-purge index cleanup (service tests), grant-cookie path and id_token claim decoding (sso.rs unit tests).

Accepted residual risk (documented, not tested): the per-call-site `DeactivationReason` choices (admin → `Admin`, client + OIDC account page → `SelfService`) are one-line policy decisions; testing the OIDC page end-to-end requires the account-management login-token dance and was judged not worth the harness complexity. The service-level semantics of each reason are fully tested.

Gates for this commit: `cargo fmt --check`, `cargo clippy --workspace --all-targets --all-features -- -D warnings`, and the full workspace test suite — all passed; suite is now 479 passed / 0 failed / 4 ignored (log `/tmp/newtests-full.log`).

## Follow-Ups

- Force-push `main` to `origin` when ready (`git push --force-with-lease origin main`), then let the fork release automation tag `v1.7.1-mindroom.1`.
- After pushing, update the Nix pin on `hetzner-matrix` (`tuwunelVersion` + hash) per the host runbook when a release exists.
- The dev-shell sccache breakage (`GLIBC_ABI_DT_X86_64_PLT`) predates this rebase and still exists; builds need `RUSTC_WRAPPER=` until the shell is rebuilt with a consistent glibc.
