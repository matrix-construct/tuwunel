# Rebase onto upstream `main` (post-`v1.8.0`) - 2026-07-05

## Goal

Rebase the MindRoom fork off `v1.8.0` and onto the unreleased upstream `main`
tip so the fork can **drop its own `m.replace` edit-bundling delta** and adopt
upstream's native implementation (MSC3925, config `bundle_edit_relations`),
which landed upstream after `v1.8.0`. Keep the fork edit-purge; verify the purge
composes with upstream's new `relatesto_typed` typed index. Reshape the fork
history into a small set of logical commits (history rewrite is explicitly
allowed for this fork).

## Baseline

- Fork tip of record: `origin/main` = `f233bfd9`
  (`mindroom/edits: serve bundled m.replace aggregations on originals (#6)`),
  tagged `v1.8.0-mindroom.2`. `origin/main` = `v1.8.0` + 22 fork commits.
- Fork base before this rebase: upstream `v1.8.0` = `c17c1d9c` (2026-06-27, the
  latest *tagged* upstream release).
- Upstream target: `upstream/main` tip `59f13052`
  (`oauth: Borrow the claims in the Apple id_token test helper.`), **unreleased**
  — the latest tagged release is still `v1.8.0`. Chosen deliberately (user
  decision) because the edit-bundling feature that supersedes the fork delta is
  only on `main`, not in any release yet.
- Upstream delta: `v1.8.0..upstream/main` = 35 commits.

Notable upstream commits in that delta:
- `c01d1dbf` pdu_metadata: Bundle the latest m.replace edit as a full event (MSC3925).
- `b8d384dd` rooms/pdu_metadata: Add a typed relation index for the edit bundle (`relatesto_typed`).
- `ed91e081` pdu_metadata: Bundle m.reference children as an event-id chunk (MSC3267).
- `db430617` api/client/search: Bundle aggregations on search context events (MSC3666).
- `a791d7ed` client: Forbid /messages on a room the user cannot see.
- `127c5228` oauth: fall back to Apple id_token claims when userinfo fails — **this is the fork's own commit `d7c8c138`, merged upstream** (+ merge `58f806b0` + test tweak `59f13052`).
- `1f14bbf7` Bump Rust 1.95.0 (post-rebase the toolchain is 1.95.0; the fork's 1.94 dev pin no longer builds the workspace — `rust-version` now requires 1.95).
- `db1c68c6` oidc: Serve native account registration and login (#479) — profile/users restructure.
- `fb5a4ea9` Profile refactor — moved `Propagation`/`propagation_default` to `crate::profile`; moved `userid_displayname` out of the `users` Data struct.

## Dropped fork commits

`git cherry -v upstream/main f233bfd9 v1.8.0` flagged exactly one fork commit as
already upstream. Two commits were dropped in the rebase:

- `d7c8c138` `oauth: fall back to Apple id_token claims when userinfo fails` —
  patch-id match with upstream `127c5228`; dropped (already upstream).
- `f233bfd9` `mindroom/edits: serve bundled m.replace aggregations on originals
  (#6)` — the fork's own read-time `m.replace` bundling, **superseded by
  upstream's `bundle_edit_relations`**; dropped. Its search-context fix is
  covered by upstream `db430617`; its /messages path by upstream `a791d7ed` +
  the uniform `bundle_aggregations`.

## Safety refs

- `backup/origin-main-before-rebase-upstreammain-20260705` = `f233bfd9`.
- `backup/feature-bundled-edit-aggregations-20260705` = `bce9c710`.
- `backup/upstream-main-target-20260705` = `59f13052`.

Local `main` was reset to `origin/main` (`f233bfd9`) after `--update-refs`
dragged it onto the rebased tip; the rebase lives on branch
`mindroom/rebase-upstream-main-20260705`.

## Test command

Post-rebase the workspace requires Rust **1.95.0** (upstream bump). The 1.94 pin
from prior sessions no longer builds it. A unified 1.95 toolchain was assembled
from nixpkgs store paths (cargo 1.95.0 + rustc 1.95.0 + a `nix-store --realise`d
clippy 1.95.0), plus the usual sanitized env (see prior rebase docs and the
`tuwunel-rebase-test-command` memory): unset the Nix musl/static cross vars,
`RUSTC_WRAPPER=`, `LIBCLANG_PATH`, liburing pkg-config/include/runtime paths,
`RUST_TEST_THREADS=1`, `-u TUWUNEL_DATABASE_PATH`, separate `CARGO_TARGET_DIR`.

## Conflict log

Base (`53e41bd1` tests-db-path, `23c6b575` compact/purge) applied clean —
notably the edit-purge onto upstream's new relation-index code merged with no
conflict.

- Stop 1 — `a18e17ca` (auth/uiaa strict-CSP SSO fallback), `src/api/router/auth/uiaa.rs`.
  - Cause: upstream now defaults `UiaaInfo.params` to `to_raw_value(&json!({})).ok()`;
    the fork commit's context predated that line.
  - Resolution: keep upstream's `params` default. It is only the no-IdP fallback;
    the fork's `bind_sso_identity_provider` still overrides `params` with the
    `m.login.sso` binding when an IdP is bound (unconflicted logic).
- Stop 2 — `19892e2a` (users/sso self-reactivation), `src/service/deactivate/mod.rs` + `src/service/users/mod.rs`.
  - Cause: upstream's profile refactor moved `Propagation` to `crate::profile`
    and removed `userid_displayname` from the `users` Data struct.
  - Resolution: import `Propagation` from `crate::profile` and `DeactivationReason`
    from the fork's `users::sso`; keep only the genuinely fork-new
    `userid_deactivation_reason` map field/handle; drop the fork's re-add of
    `userid_displayname` (now owned by the profile service; the map still exists
    in `maps.rs`).
- Stop 3 — `fb493b23` (native Apple login #2), `src/api/client/session/sso.rs`.
  - Cause: import-line divergence (upstream's futures import set vs the fork's).
  - Resolution: take the fork's superset (`StreamExt` + `http::StatusCode`),
    which `native_apple_login_route` uses at this commit (`StatusCode::OK`).

Remaining fork commits (native review #3, grant-cookie #4, regression tests, CI
checks, risk-area tests, SSO/power pins) applied clean.

## Adopting upstream edit bundling

- Fork default flipped: `bundle_edit_relations` now defaults **true** in the
  fork (`#[serde(default = "true_fn")]`, `src/core/config/mod.rs`) with a doc
  note. Upstream ships it off (opt-in); the fork's purge deletes superseded
  edits, so without the bundle a history endpoint would serve an original with
  its stale pre-edit body. `bundle_reference_relations` is left at upstream's
  default (off) — references are client plumbing MindRoom does not render.
- Purge ↔ typed-index composition: verified. Upstream's bundler reads
  `relatesto_typed` newest-first and tolerates dangling rows (`get_pdu_from_id
  ().ok()` → skip → fall through). The purge deletes edit PDUs (from `pduid_pdu`
  et al.) but not `relatesto_typed`/`tofrom_relation`, so it leaves dangling
  rows for deleted edits — which the bundler skips. It keeps the newest edit,
  which the bundler wants, so the surviving edit is still bundled. Upstream also
  ships a startup migration (`rebuild_relatesto_typed`) + admin command that
  rebuild the index from every stored PDU, so existing (pre-upgrade) edits get
  indexed and bundle correctly. The purge does **not** need to maintain
  `relatesto_typed`.
- New test: `src/mindroom-tests/tests/edit_purge_bundle_compose.rs` drives a
  real `edit_purge.purge_cycle()` then asserts the surviving edit is still
  bundled, and that a dangling *newest* typed-index row is skipped so the
  bundler falls through to the surviving edit. `edit_purge::purge_cycle` was made
  `pub` so integration tests (and operators) can trigger a cycle.
- Serialization difference to watch: upstream serializes the bundled child as
  `AnySyncMessageLikeEvent` (omits `room_id`); the dropped fork code used
  `AnyTimelineEvent` (included `room_id`). Both include `origin_server_ts` (the
  field the fork noted MindRoom Cinny requires). Client rendering of edits from
  the bundle should be re-verified against Cinny after deploy.

## Result

- Rebase completed: branch `mindroom/rebase-upstream-main-20260705` =
  `upstream/main` + 20 fork commits (22 − 2 dropped), later reshaped into logical
  commits (see below). No conflict markers; `git diff --check` clean.

## Reshape

History was rewritten (allowed for this fork) into 7 logical, self-contained
commits on branch `mindroom/rebase-clean-20260705` (built by cherry-picking the
20 replayed commits into feature groups, in a dependency-preserving order so the
shared files — `sso.rs`, `config/mod.rs`, `tuwunel-example.toml` — kept their
per-commit hunks). The reshaped tree is byte-identical to the validated
pre-reshape stack except `FORK_CHANGES.md` (rewritten). Commits:

1. `mindroom/edits: compact /sync, purge superseded edits, bundle the survivor`
2. `config/rooms: default room power-level override`
3. `auth/sso: strict-CSP UIAA fallback and self-reactivation`
4. `auth/apple: native iOS Apple login exchange and SSO cookie hardening`
5. `ci: fork release automation, container publishing, and GitHub checks`
6. `tests: integration coverage pinning fork behaviors`
7. `docs: fork runtime and release overview`

Validation: all 7 commits build (`cargo check --workspace --all-targets`, Rust
1.95); the tip passes the full test suite and `cargo clippy -D warnings`
(1.95). Two resurfaced/1.95 lints were fixed in-place: `#[allow(too_many_lines)]`
on `edit_purge::purge_cycle`, and `Duration::from_mins` in `native_apple.rs`.

Backup refs: `backup/origin-main-before-rebase-upstreammain-20260705`
(`f233bfd9`, pre-rebase origin/main), `backup/rebase-clean-tip-20260705`
(reshaped tip), `backup/feature-bundled-edit-aggregations-20260705`,
`backup/upstream-main-target-20260705`. Local `main` still points at
`f233bfd9`; publishing is a manual force-push.

## Follow-ups

- Deploy note: `bundle_edit_relations` now defaults on in the fork, so the Nix
  config need not set it; the startup `rebuild_relatesto_typed` migration runs
  once on first boot of the rebased build.
- Re-verify edit rendering in MindRoom Cinny against upstream's bundle shape
  (no `room_id` in the bundled child).
- Publish: force-push the branch / update `origin/main` when ready (left manual).
