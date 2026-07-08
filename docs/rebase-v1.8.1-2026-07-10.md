# Rebase on upstream v1.8.1 - 2026-07-10

## Goal

Rebase the MindRoom fork onto upstream `v1.8.1` (the latest published GitHub
release) with the usual explicit overlap review, per-commit build gates, and a
sweep for semantic conflicts that a textually clean rebase would hide.

## Baseline

- Fork tip of record: `origin/main` = `213dde5e`
  (`docs: fork runtime and release overview`).
- Fork base before this rebase: upstream `main` `2323a8ec`
  (the 2026-07-07 rebase target, past `v1.8.0`).
- Upstream target: `v1.8.1` = `af3b4ad4` (`Bump 1.8.1.`, tagged 2026-07-09,
  published 2026-07-10; identical to the `upstream/main` tip at session time).
- Upstream delta: `2323a8ec..v1.8.1` = 25 commits.
- Fork delta: 6 linear commits, no merges.
- `git cherry -v v1.8.1 main`: all 6 fork commits `+` — nothing was absorbed
  upstream in this window, so nothing is dropped.

Notable upstream commits in the delta:

- The Synapse admin API surface (#38/#494): `/_synapse/admin` user, device,
  token, room, media, federation, and misc endpoints, a new `service/tasks`
  background-task tracker, and new room purge/delete machinery
  (`rooms/timeline/purge.rs`, `pdu_metadata::purge_event_relations`).
- Migration hardening for conduit imports (#41) and a longer systemd start
  timeout during long migrations.
- Private read receipts now carry a timestamp (`read_receipt` schema change,
  threaded through `timeline/append.rs`).
- `synapse-admin-api` dependency rev bump; version bump to 1.8.1;
  SECURITY.md, issue templates, PR template.

## Safety Refs

- `backup/origin-main-before-rebase-v1.8.1-20260710` = `213dde5e` preserves
  the fork tip this rebase starts from.
- Work branch: `mindroom/rebase-v1.8.1-20260710`; local `main` untouched until
  validation completed. `--no-update-refs` passed to every rebase because the
  global `rebase.updateRefs=true` dragged `main` along during the 2026-07-07
  session.

## Overlap Review

Same-file overlap (`comm -12` of both `--name-only` diffs) — 6 files, down
from 18 in the v1.7.1 rebase:

- `Cargo.lock` — fork adds the `mindroom_rebase_tests` package entry;
  upstream bumps every workspace crate to 1.8.1 and the `synapse-admin-api`
  rev. Disjoint hunks.
- `README.md` — fork adds the fork banner at the top; upstream reworded the
  support section (~line 144) for SECURITY.md.
- `src/api/router.rs` — fork inserts the native-Apple route after
  `login_token_route`; upstream registers the Synapse admin routers and moves
  the shared-secret register routes behind a `mas_active` gate ~25 lines away
  in the same function. The fork route
  (`/_matrix/client/unstable/org.mindroom.login/apple`) does not collide with
  upstream's new unstable admin whois aliases.
- `src/service/media/mod.rs` — fork's `mxc_is_owned_by_user`/`delete_owned_by`
  at ~line 242 sit between upstream's struct-field hunk (~45) and its new
  admin-media helpers (~474).
- `src/service/mod.rs`, `src/service/services.rs` — fork registers
  `edit_purge`, upstream registers `tasks`, at different alphabetical
  positions.

All six auto-merged; the rebase completed with **zero textual conflicts** and
`git range-diff` showed all 6 commits replayed patch-identical.

## Semantic Reconciliation

A textually clean rebase still broke the build, exactly where predicted during
the overlap review:

- Upstream's **new** Synapse admin endpoints call the pre-fork one-argument
  `users.deactivate_account`, while the fork's signature (from the sso commit)
  is `deactivate_account(user_id, reason: DeactivationReason)`:
  - `src/api/client/admin/users/deactivate_account.rs`
    (`POST /_synapse/admin/v1/deactivate/{user_id}`)
  - `src/api/client/admin/users/create_or_modify.rs`
    (`PUT /_synapse/admin/v2/users/{user_id}` with `deactivated: true`)
- Both are admin-initiated, so both now pass `DeactivationReason::Admin`,
  mirroring the existing `mas/delete_user.rs` adaptation. Folded into the sso
  commit (`auth/sso: SSO-origin UIAA hardening and self-reactivation`) so the
  stack stays per-commit buildable; its message body notes the adaptation.

The rest of the new admin surface was swept for fork-API interactions and
needed no changes:

- `reset_password.rs` and `create_or_modify.rs` call the two-argument
  `set_password`, whose signature the fork does not change; the fork's
  sentinel/origin-preservation logic composes (a real admin-set password
  clears SSO origin by design, the `PASSWORD_SENTINEL` reactivation path
  preserves it).
- `mas/reactivate_user.rs` (sentinel path) and the new suspend/lock endpoints
  touch nothing the fork modifies.

### Edit-purge index audit

Upstream's new `timeline::purge_history` is the authoritative checklist of
per-event indexes: `pduid_pdu`, `eventid_pduid`, `eventid_outlierpdu`,
`roomid_tscount_pducount` (via `bias_count`), search deindex,
`purge_event_relations` (`tofrom_relation`, `relatesto_typed`,
`referencedevents`, `softfailedeventids`), and `retention.purge_original`.
`timeline/append.rs` gained **no new per-PDU index** in this window (only the
receipt-timestamp parameter), so the fork's edit-purge deletion set
(`pduid_pdu`, `eventid_pduid`, `roomid_tscount_pducount`, sidecar media)
remains complete relative to its reviewed baseline. Relation rows for purged
superseded edits are still intentionally left dangling — the same
"harmless: relation reads discard ids that no longer resolve" rationale
upstream now documents on `purge_event_relations`. The
`edit_purge_bundle_compose` test continues to pin the dangling-row tolerance.

## Kept / updated fork commits

Result: the same 6 logical commits on the new base.

- KEPT (replayed) `5ad18572` → edits, `b9d4c8c1` → apple, `8d903677` → ci,
  `85b01a36` → tests.
- **UPDATED** `8c8b8e41` → sso: adds the two `DeactivationReason::Admin`
  call-site adaptations for upstream's new Synapse admin deactivation
  endpoints (see above).
- **UPDATED** `213dde5e` → docs: this file; FORK_CHANGES.md re-based to
  v1.8.1 and its sso section extended with the new endpoint files.

`Cargo.lock` merged coherently (fork test-crate entry intact, all workspace
crates at 1.8.1, no stale 1.8.0 entries); every build below ran `--locked`,
which proves lock/manifest coherence the same way the fork CI does.

## Validation

First rebase session on macOS (aarch64-apple-darwin, rustup toolchain 1.95.0
per `rust-toolchain.toml`) instead of the Nix Linux dev shell — none of the
liburing/sccache environment surgery from the v1.6.1/v1.7.1 notes applies
here; the repo `.cargo/config.toml` only sets `RUMA_UNSTABLE_EXHAUSTIVE_TYPES`.

- Per-commit gates: `cargo check --locked --workspace --all-targets` at the
  three compile-relevant seam commits (edits, sso, apple) and at the tip —
  all clean, so the stack bisects.
- `cargo clippy --locked --workspace --all-targets -- -D warnings` — clean.
- `RUST_TEST_THREADS=1 cargo test --locked --workspace --all-targets` — all
  suites green, including all six `mindroom_rebase_tests` binaries
  (admin_deactivate_erase, apple_userinfo_fallback, deactivate_erase,
  edit_purge_bundle_compose, sso_callback_completion, sso_redirect) and the
  `tuwunel_service` unit suite (270 passed — edit_purge, users::sso
  reactivation, and upstream's new tasks tests). The `--all-features`
  clippy/test axis is covered by `mindroom-ci.yml` on push (Linux runners).
- `rustfmt` (nightly) over the two adapted files — no reformatting.

## Post-rebase amendments (same session)

- Boot smoke on the rebased tip (fresh DB, port 28998): CS API 200, the fork
  Apple route rejects with the camelCase `identityToken` field error the
  Cinny client's contract expects, the new `/_synapse/admin` whois route
  answers 401 beside it, and the fresh database contains the fork's
  `userid_deactivation_reason` column family.
- Documented the deactivation-reason design divergence (vs Synapse and
  upstream) in `src/service/users/sso.rs` above `DeactivationReason`, with a
  matching design note in FORK_CHANGES.md — including the decision **not** to
  upstream it (reversible deactivation would effectively need an MSC). Synapse
  facts verified against the `mindroom-synapse` checkout: bare
  `users.deactivated` flag, no reason concept, SSO login of deactivated
  accounts hard-blocked (`sso_account_deactivated_template`), admin
  `activate_account` expects a password hash afterwards. Folded into the sso
  and docs commits so future rebases don't re-derive this.

## Post-rebase notes

- Pushing `main` triggers `auto-mindroom-release.yml`, which should compute
  `v1.8.1-mindroom.1` (base version auto-detected from `Cargo.toml`, now
  1.8.1) and publish binaries/containers via the release workflows.
- The production host pin (`tuwunelVersion` in
  `dotfiles/configs/nixos/hosts/hetzner-matrix`) can move to the new tag once
  the release assets exist.
