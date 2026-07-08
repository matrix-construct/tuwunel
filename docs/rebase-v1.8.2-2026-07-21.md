# Rebase on upstream v1.8.2 - 2026-07-21

## Goal

Rebase the MindRoom fork onto upstream `v1.8.2` (the latest published GitHub
release) with the usual explicit overlap review, per-commit build gates, and a
sweep for semantic conflicts that a textually clean rebase would hide. First
rebase in which fork features were absorbed upstream via our own submitted
branches, so this session also prunes the fork delta.

## Baseline

- Fork tip of record: `origin/main` = `efc8b615e`
  (`fix(e2ee): reject replacement of existing device identity keys (#10)`).
- Fork base before this rebase: upstream `v1.8.1` = `af3b4ad4`.
- Upstream target: `v1.8.2` = `9099defe5` (`Bump 1.8.2.`, tagged/published
  2026-07-17; `upstream/main` was already 39 commits past it — we track
  releases, not main).
- Upstream delta: `v1.8.1..v1.8.2` = 119 commits.
- Fork delta: 12 linear commits, no merges — the six logical commits from the
  v1.8.1 session plus two follow-up fixes (ci container-asset wait, Synapse
  admin deactivation test) and four merged PRs (#7, #8, #9, #10).

Notable upstream commits in the delta:

- Two of our own upstream submissions merged (see Dropped below): the
  zero/drained one-time-key counts fix (`164b8da61` + upstream's own follow-up
  `007033cd5` matching Synapse's OTK count shape) and public MatrixRTC
  transport discovery (`a1776c368`).
- Forward-extremity work: scored prune engine, receive-path cap, admin
  prune/list commands, band preservation on soft-failed events
  (`timeline/append.rs`).
- Media rework: URL previews stored as CBOR, relay-media caching, new
  `mediaid_lazy`/`mediaid_lazycontent`/`url_preview` maps (`url_previews`
  dropped), og:video/og:audio support, admin media hardening.
- Admin output overhaul (size-capped buffer, reply/thread splitting, file
  attachment) with new `admin_output_max_events`/`admin_output_threads`
  options; more Synapse admin endpoints (server notices, user redaction,
  login-as, federation destinations, media info/purge/statistics).
- MSC3202/MSC4203 appservice E2EE transaction extensions; database backup
  restore/verify commands; `--health-check` and `--restore-backup` CLI args;
  packaging (RPM/COPR, apt repo) and CI storage tuning.
- A new `documented_defaults_match_the_code` test
  (`src/core/config/tests.rs`) parsing `config/mod.rs` doc comments against
  the one-line integer default fns — this now also covers the fork's
  `mindroom_edit_purge_*` options.
- Dependency bumps including ruma (always emits the room ephemeral section in
  `/sync`).

## Dropped: fork commits absorbed upstream

`git cherry -v v1.8.2 main` marked `c2729b76c` equivalent (`-`); manual review
of the two `mindroom-ai/caveman/*` merge branches in the upstream log
confirmed both submissions landed:

- **DROPPED** `c2729b76c` `fix(e2ee): report drained one-time-key pools (#7)`
  — byte-identical upstream as `164b8da61` (`users: Preserve zero
  one-time-key counts.`); upstream then reshaped the same function in
  `007033cd5` (`users: Match Synapse one-time-key count shape.`), which the
  fork now inherits instead of carrying its own copy.
- **DROPPED** `806ef6b9c` `fix(matrixrtc): allow public transport discovery
  (#8)` — byte-identical upstream as `a1776c368` (only hunk offsets/context
  differ; `git cherry` missed it because `d16303553` added Matrix v1.18/v1.19
  to the surrounding version list).

Not absorbed, kept: `efc8b615e` (#10) — upstream's `upload_keys.rs` was
untouched in this window (the `store_device_keys` helper both sides share
predates v1.8.1) and upstream `remove_device` still carries the
`TODO: Remove onetimekeys` the fork resolved. The upstream submission branch
(`origin/fix/reject-device-key-replacement`) had not merged as of v1.8.2.

## Safety Refs

- `backup/origin-main-before-rebase-v1.8.2-20260721` = `efc8b615e` preserves
  the fork tip this rebase starts from (including the original, pre-fold
  commits).
- Work branch: `mindroom/rebase-v1.8.2-20260721`; local `main` untouched until
  validation completed. `--no-update-refs` passed to every rebase (global
  `rebase.updateRefs=true`).

## Restacking (fold pass)

Before leaving the v1.8.1 base, the accumulated follow-ups were folded into
their logical parents and the docs commit moved back to the stack tip, so the
stack replays as clean per-feature units:

- `aa59033c6` (`ci: wait for both container release assets before building`)
  squashed into the ci commit; its rationale preserved in the combined body.
- `a42a76b67` (`tests: pin Synapse-admin deactivation to the admin reason`)
  squashed into the tests commit; ditto.
- `e1ba208f9` (docs) moved from mid-stack to last. All moved pairs touch
  disjoint files.

Verification: the folded stack's tree is **identical** to `origin/main`
(`git diff main` empty), so the fold changed only history shape, not content.

## Overlap Review

Same-file overlap (`comm -12` of both `--name-only` diffs): 17 files, of
which 3 (`versions.rs`, `auth/dispatch.rs`, `users/keys.rs`) belong only to
the dropped commits, leaving 14 real overlaps:

- `Cargo.lock` — fork adds the `mindroom_rebase_tests` entry; upstream bumps
  workspace crates to 1.8.2, ruma, and friends. Disjoint hunks.
- `README.md` — fork banner at top; upstream support/SECURITY edits ~40 lines
  below. Disjoint.
- `.github/workflows/main.yml` — fork removes the push/PR/tag triggers
  (workflow_dispatch only); upstream tunes builder storage parameters and
  threads a new `apt_ssh_key` secret. Disjoint. Upstream's new tag-triggered
  `copr.yml` excludes `v*-*` tags, so the fork's `v<base>-mindroom.<n>`
  release tags cannot trigger COPR submission; the apt publish path only runs
  from `main.yml`, which the fork already gates to workflow_dispatch.
- `src/api/router.rs` — fork's native-Apple route vs upstream's new Synapse
  admin routes ~60 lines away. Disjoint.
- `src/core/config/mod.rs`, `tuwunel-example.toml`, `check.rs` — fork's
  mindroom options/defaults vs upstream's new options (dns_servers,
  url_preview agents, admin_output_*) in different regions. Disjoint.
- `src/database/maps.rs` — fork's `userid_deactivation_reason` (~554) vs
  upstream's new media/url-preview maps (~149, ~507). Disjoint.
- `src/main/args.rs` — fork's `default_test` database-path isolation vs
  upstream's new `--health-check`/`--restore-backup` args; `default_test`
  mutates a parsed default rather than constructing `Args` literally, so the
  new fields compose without adaptation.
- `src/service/deactivate/mod.rs` — upstream only dropped a `.boxed()` in
  `full_deactivate`; fork's reason threading untouched.
- `src/service/media/mod.rs`, `data.rs` — fork's
  `mxc_is_owned_by_user`/`delete_owned_by` sit inside upstream's reworked
  media module; `mediaid_user` (the map the helper reads) survives the CBOR
  preview/relay-media rework with the same key schema and write sites, and
  `delete_owned_by` composes with upstream's new lazy-content-aware
  `delete`.
- `src/service/pusher/send.rs` — both sides touch `send_push_notice` and
  `send_notice`: fork adds the stream-suppression early return and terminal
  push-content mapping, upstream adds a `user_id` parameter and deletes
  pushers the gateway rejects. Hunks disjoint; composition reviewed by hand
  post-rebase.
- `src/service/users/device.rs` — fork's identity-key/OTK purge in
  `remove_device` vs upstream changes elsewhere in the file (create_device,
  refresh tokens, MSC3202 fanout helpers). Disjoint;
  `onetimekeyid4225_otk` still exists with the same access pattern.

The rebase completed with **zero textual conflicts** and `git range-diff`
showed all 8 kept commits replayed patch-identical.

## Semantic Reconciliation

Sweeps for interactions a clean merge would hide:

- **Deactivation seam quiet this window**: `deactivate_account` and
  `set_password` call-site counts are identical between v1.8.1 and v1.8.2
  (7 and 11), and no new caller files appeared — the first rebase since the
  sso commit that needed no `DeactivationReason` adaptation.
- **Edit-purge index audit**: upstream's `timeline/append.rs` changes are
  forward-extremity band preservation only (`nonempty_band`); no new per-PDU
  index was added, so the fork's edit-purge deletion set (`pduid_pdu`,
  `eventid_pduid`, `roomid_tscount_pducount`, sidecar media) remains complete
  relative to its reviewed v1.8.1 baseline. `purge_history` remains the
  authoritative checklist.
- **Pusher composition**: upstream's rejected-pushkey deletion runs after the
  fork's suppression logic by construction (suppressed events never reach the
  gateway); the fork's `send_notice` content mapping feeds upstream's new
  `Request::new(notify)` path unchanged.
- **Documented-defaults test**: the new upstream test parses the fork's
  `mindroom_edit_purge_*` doc comments too; they already satisfy the
  `/// default: N` ↔ one-line-fn contract (verified by the test run below).
  `bundle_edit_relations` (bool) is outside the test's integer scope.
- **Dropped-#7 follow-on**: upstream `007033cd5` changed the OTK count shape
  after absorbing the fork fix; nothing in the fork pins the old shape (the
  mindroom-tests suite does not exercise OTK counts), so inheriting the
  upstream behavior is the intended outcome.
- **Newly enabled clippy lints**: upstream turned on `option_if_let_else`
  (and `significant_drop_in_scrutinee`, `cognitive_complexity`,
  `unused_braces`) workspace-wide in this window. Three fork sites needed the
  `map_or_else` conversion — the resume-key stream selection in
  `src/service/edit_purge/mod.rs` and the `replace_target` test-content
  helpers in `edit_purge/mod.rs` and
  `src/api/client/sync/mindroom_edits.rs`. Folded into the edits commit.

## Kept / updated fork commits

Result: 8 logical commits on the new base (12 before folding/drops).

- KEPT (replayed patch-identical) `5112447c2` → sso, `dbf94d222` → apple,
  `34baf29bc` → #9 stream push, `efc8b615e` → #10 device-key immutability.
- **UPDATED** `73411e48c` → edits: replayed patch-identical, then extended
  with the three `option_if_let_else` conversions above.
- **UPDATED (fold only)** ci: absorbs `aa59033c6`; tests: absorbs
  `a42a76b67`. Content byte-identical to the sum of their parts.
- **UPDATED** docs: this file; FORK_CHANGES.md re-based to v1.8.2, its
  dropped-features note extended (#7/#8), and new sections documenting the
  stream-push (#9) and device-key immutability (#10) features that landed
  after the last docs pass.

## Validation

macOS aarch64-apple-darwin, rustup toolchain 1.95.0 per `rust-toolchain.toml`;
plain cargo, no Nix dev-shell surgery (see v1.8.1 notes).

- Per-commit gates: `cargo check --locked --workspace --all-targets` at every
  code commit of the stack (edits, sso, apple, tests, #9, #10 = tip-1) — all
  clean, so the stack bisects. After the clippy conversions were folded into
  the edits commit, that commit was re-gated the same way (the two touched
  files are never modified by later commits, so the downstream gates hold).
- `cargo clippy --locked --workspace --all-targets -- -D warnings` at the
  tip — clean after the three conversions; the failures it caught first were
  exactly the new-lint sites listed above.
- `RUST_TEST_THREADS=1 cargo test --locked --workspace --all-targets` at the
  tip — 686 passed, 0 failed across 39 suites, including the 334-test
  `tuwunel_service` unit suite (edit_purge, users::sso reactivation, the #9
  stream-push classification, upstream's tasks/backup suites), the 165-test
  `tuwunel_core` suite with upstream's new
  `documented_defaults_match_the_code` (which now also parses the fork's
  `mindroom_edit_purge_*` options — they satisfy the `/// default: N` ↔
  one-line-fn contract), and the seven `mindroom_rebase_tests` binaries
  (admin_deactivate_erase, apple_userinfo_fallback, deactivate_erase,
  edit_purge_bundle_compose, sso_callback_completion, sso_redirect,
  synapse_admin_deactivate). The `--all-features` axis is covered by
  `mindroom-ci.yml` on push (Linux runners). Note for future sessions: don't
  pipe the test run through `head` — the pipeline exit code becomes `head`'s
  and truncation can masquerade as a pass; this session re-ran the suite
  unfiltered after noticing exactly that.
- `cargo +nightly fmt --all -- --check` — no reformatting.
- Boot smoke on the rebased tip (fresh DB, port 28998): CS API versions 200
  (declares v1.19 and `org.matrix.msc4143`), the
  `org.matrix.msc4143/rtc/transports` route answers **without a token** —
  end-to-end proof the dropped #8 behavior survives via upstream's code — the
  fork Apple route rejects with the camelCase `identityToken` contract error,
  `/_synapse/admin/v1/whois` answers 401, and the fresh database opens the
  fork's `userid_deactivation_reason` column family alongside upstream's new
  `mediaid_lazy`/`url_preview` families.

## Post-rebase notes

- Pushing `main` triggers `auto-mindroom-release.yml`, which computes
  `v1.8.2-mindroom.1` (base auto-detected from `Cargo.toml`, now 1.8.2) and
  publishes binaries/containers. Upstream's new `copr.yml` ignores the
  hyphenated fork tags.
- Deploy pins (`tuwunelVersion`/`tuwunelArchiveHash` in
  `dotfiles/configs/nixos/hosts/{hetzner-matrix,mindroom}/constants.nix`;
  aarch64 vs x86_64 hashes differ) can move to the new tag once release
  assets exist.
- The #10 upstream submission (`fix/reject-device-key-replacement` plus its
  undeserializable-keys and device-removal-purge follow-ups) was still
  unmerged at v1.8.2; if it lands upstream, drop #10 at the next rebase the
  same way #7/#8 were dropped here.
