# Rebase on upstream v1.6.1 - 2026-05-04

## Goal

Rebase the MindRoom fork onto upstream `v1.6.1` with explicit overlap review, conflict-by-conflict notes, and full test runs after each conflict resolution before continuing the rebase.

## Baseline

- Local branch at start: `main` = `3cf5462fe760683d7d0d2ce82c2ed0ea69b534fa`
- Fetched fork tip: `origin/main` = `f562d1c38423e01ef8dd2b3ef63c3c67720c13a1`
- Upstream target: `v1.6.1` = `dda5123436d55c58aacd8ba6ebd0d2bae0c87260`
- Merge base of `origin/main` and `v1.6.1`: `44a85e25956f3e5664279d5f1fd41a70a3f2a00f`
- Initial state: local `main` was clean and one commit behind `origin/main`.

## Safety Refs

- `backup/main-before-rebase-v1.6.1-20260504` preserves the local branch start.
- `backup/origin-main-before-rebase-v1.6.1-20260504` preserves the fetched fork tip.

## Test Command

Current full local test command for conflict gates:

```bash
env -u TUWUNEL_DATABASE_PATH \
  -u LD_PRELOAD \
  -u NIX_CFLAGS_LINK -u NIX_CFLAGS_COMPILE -u NIX_LDFLAGS -u NIX_CC -u NIX_BINTOOLS \
  -u CC -u CXX -u LD -u AR -u AS -u RANLIB \
  -u CARGO_BUILD_TARGET -u CARGO_BUILD_RUSTFLAGS -u CARGO_PROFILE \
  -u JEMALLOC_OVERRIDE -u ROCKSDB_LIB_DIR -u cmakeFlags \
  PKG_CONFIG_PATH=/nix/store/kic9mkwsq7xgrlxzzarrd7k0k83qnhik-liburing-2.12-dev/lib/pkgconfig \
  CFLAGS=-I/nix/store/kic9mkwsq7xgrlxzzarrd7k0k83qnhik-liburing-2.12-dev/include \
  CXXFLAGS=-I/nix/store/kic9mkwsq7xgrlxzzarrd7k0k83qnhik-liburing-2.12-dev/include \
  LD_LIBRARY_PATH=/nix/store/hczbpz0rrrn86nvphr2zv9fx4qm8isb2-liburing-2.12/lib:$LD_LIBRARY_PATH \
  RUST_TEST_THREADS=1 \
  cargo test --workspace --all-targets
```

Reason: the inherited shell has Nix cross/static-link variables (`CC=x86_64-unknown-linux-musl-gcc`, `LD=x86_64-unknown-linux-musl-ld`, `NIX_CFLAGS_LINK=-static`, `CARGO_BUILD_TARGET=x86_64-unknown-linux-musl`, `JEMALLOC_OVERRIDE=<musl static jemalloc>`, etc.) that make even a simple Rust binary segfault at startup or force/link against musl-specific artifacts. The sanitized environment produced a runnable simple Rust binary. `rust-librocksdb-sys` also needs explicit `liburing` pkg-config/include/runtime paths in this shell. `RUST_TEST_THREADS=1` avoids parallel RocksDB lock contention, and `TUWUNEL_DATABASE_PATH` must be unset because Figment environment overrides beat test-local TOML database paths.

## Running Notes

- Need to inspect the full fork diff from merge base to `origin/main`.
- Need to inspect the full upstream diff from merge base to `v1.6.1`.
- Need to identify same-file and same-feature overlaps before starting the rebase.
- Need to include the missing `origin/main` merge commit in the fork tip before rebasing local `main`.

## Overlap Review

Completed first pass before starting the rebase.

Commands used:

```bash
git diff --stat --find-renames 44a85e25956f3e5664279d5f1fd41a70a3f2a00f..origin/main
git diff --stat --find-renames 44a85e25956f3e5664279d5f1fd41a70a3f2a00f..v1.6.1
git diff --name-only 44a85e25956f3e5664279d5f1fd41a70a3f2a00f..origin/main
git diff --name-only 44a85e25956f3e5664279d5f1fd41a70a3f2a00f..v1.6.1
git cherry -v v1.6.1 origin/main
```

Patch-id check:

- `git cherry -v v1.6.1 origin/main` shows all fork commits as `+`; no fork commit appears to have been directly included upstream.

Fork diff summary:

- 32 files changed, 4128 insertions, 53 deletions.
- Main runtime areas: edit compaction and purge, Apple SSO userinfo fallback, strict-CSP SSO UIAA fallback, SSO self-reactivation, default room power-level override.
- Main operational areas: fork release automation, binary/container release workflows, fork documentation.

Upstream diff summary:

- 239 files changed, 10881 insertions, 4017 deletions.
- Main runtime areas: OIDC/account-management and UIAA improvements, configurable client IP extraction, storage/S3 multipart behavior, spaces cache/timeline refactors, migration controls, device key/sync fixes, docs and packaging updates.

Same-file overlap:

- `README.md`
- `docker/bake.sh`
- `src/admin/user/commands.rs`
- `src/api/client/account.rs`
- `src/api/client/session/sso.rs`
- `src/api/router/auth/uiaa.rs`
- `src/core/config/check.rs`
- `src/core/config/mod.rs`
- `src/core/matrix/event.rs`
- `src/database/maps.rs`
- `src/service/media/mod.rs`
- `src/service/mod.rs`
- `src/service/users/mod.rs`
- `tuwunel-example.toml`

Functional overlap and expected resolution:

- SSO/UIAA is the highest-risk overlap. Upstream binds an exact IdP into UIAA session params and routes the fallback page through that IdP; the fork restricts SSO-origin users away from password/JWT UIAA paths and repairs legacy SSO-origin metadata. Resolution should preserve upstream IdP binding and fallback routing while retaining the fork's SSO-origin policy and repair behavior.
- `src/api/client/session/sso.rs` must preserve upstream `ClientIp` extractor and redirect diagnostics, plus fork Apple `id_token` fallback and SSO self-reactivation.
- `src/service/users/mod.rs` must preserve upstream `peek_login_token` and OIDC device IdP helpers, plus fork deactivation reason tracking and SSO self-reactivation.
- Timeline/storage has a semantic overlap even where file conflicts may not occur: upstream added `roomid_ts_pducount` as a timestamp index for PDUs, while the fork's edit purge deletes old PDUs directly. Resolution must delete the corresponding timestamp-index row when purging a superseded edit, otherwise timestamp lookups can reference removed PDUs.
- `src/service/media/mod.rs` should keep upstream storage provider upload changes and fork `mxc_is_owned_by_user` / `delete_owned_by` helpers used by edit purge sidecar cleanup.
- `docker/bake.sh` should keep upstream repository-root resolution and OCI package metadata, plus fork optional `BUILDX_BUILDER` handling.
- Config/examples need additive merge: upstream `ip_source`, size parsing, S3, migration, spaces cache, etc.; fork `default_power_level_content_override` and `mindroom_*` options.
- Default room power-level override appears fork-only. Upstream still only applies request-level `power_level_content_override`.
- Edit compaction/purge appears fork-only. Upstream mentions "replacement" in unrelated room upgrade/key replacement contexts, not superseded `m.replace` compaction.

## Conflict Log

- Stop 1: commit `b2e5f5ad` (`mindroom/edits: compact /sync and purge superseded edits`)
  - Conflict: `src/core/config/mod.rs`.
  - Cause: upstream added S3 multipart/redacted-format helper defaults at the end of the config helper section; fork added MindRoom edit-purge default helper functions at the same location.
  - Resolution: kept both upstream helper functions (`default_multipart_threshold`, `default_multipart_part_size`, `fmt_redacted_opt`) and fork helper functions (`default_mindroom_edit_purge_*`).
  - Semantic overlap handled: upstream `v1.6.1` adds `roomid_ts_pducount` for timestamp timeline lookups. The fork edit purge now removes `roomid_ts_pducount` when the index still points to the purged PDU, and the edit-purge tests assert timestamp-index cleanup.
  - Test gate attempt 1: failed to compile `tuwunel_service`; `OwnedRoomId::as_ref()` was ambiguous for timestamp-index key serialization.
  - Follow-up: made the timestamp-index key type explicit as `&RoomId`, matching upstream timeline writes.
  - Test gate attempt 2: compiled, then nine edit-purge tests failed because the test harness used fake 9-byte PDU keys and the timestamp-index adaptation parses real `RawPduId` keys.
  - Follow-up: changed the edit-purge test helper to generate real `RawPduId` bytes from `PduId`.
  - Test gate attempt 3: passed.
  - Summary: workspace/all-targets passed, including 157 passed / 2 ignored in `tuwunel_service` and 6 passed in `tuwunel_service` state-res integration tests.
- Stop 2: commit `5a573fa3` (`auth/uiaa: add strict-CSP-safe SSO fallback flow`)
  - Conflict: `src/api/router/auth/uiaa.rs`.
  - Cause: upstream `v1.6.1` binds an exact IdP into `UiaaInfo.params` for SSO fallback routing and can expose JWT UIAA when configured; the fork hides password/JWT UIAA for SSO-origin users and repairs legacy SSO origins before choosing flows.
  - Resolution: kept legacy SSO-origin repair, kept fork `sender_uses_sso` policy for password/JWT, kept upstream exact IdP binding, and only advertises `m.login.sso` when a concrete IdP can be bound to the UIAA session.
  - Test gate: passed.
  - Summary: workspace/all-targets passed, including 157 passed / 2 ignored in `tuwunel_service` and 6 passed in `tuwunel_service` state-res integration tests.
- Stop 3: commit `f562d1c3` (`Merge pull request #1 from mindroom-ai/mindroom-sidecar-purge-cleanup`)
  - Conflict: `src/service/edit_purge/mod.rs`.
  - Cause: the sidecar cleanup commit added per-candidate sender/MXC metadata and sidecar media deletion; the earlier v1.6.1 resolution had added per-candidate room/timestamp metadata to remove upstream's `roomid_ts_pducount` index.
  - Resolution: merged both candidate metadata sets, kept sidecar cleanup logic, kept timestamp-index deletion, and updated sidecar-content test fixtures to populate `roomid_ts_pducount`.
  - Test gate attempt 1: failed by hang.
  - Failure details: full workspace/all-targets built successfully and entered tests, but `src/main/tests/smoke.rs` stayed asleep for several minutes as `target/debug/deps/smoke-*`; the run was interrupted and the orphaned smoke process was killed before continuing investigation.
  - Follow-up: isolated `cargo test -p tuwunel --test smoke` with the same sanitized environment passed in 1.35s. Rerunning the full gate with output redirected to a file to avoid blocking on the large captured log stream.
  - Test gate attempt 2: passed.
  - Summary: workspace/all-targets passed with output redirected to `/tmp/rebase-stop3-test.log`, including 168 passed / 2 ignored in `tuwunel_service` and 6 passed in `tuwunel_service` state-res integration tests.

## Test Log

- Baseline fork-tip test, unsanitized:
  - Command: `cargo test --workspace --all-targets`
  - Result: failed before project compilation.
  - Error: multiple dependency build scripts exited with `signal: 11, SIGSEGV`, including `typenum`, `generic-array`, `serde`, `libc`, `quote`, `proc-macro2`, and `portable-atomic`.
- Reproduction:
  - Command: `cargo test -j1 --workspace --all-targets`
  - Result: same `proc-macro2` build-script `SIGSEGV`.
  - A minimal Rust hello-world binary compiled under the inherited environment also segfaulted.
- Environment isolation:
  - A minimal Rust hello-world binary compiled with `LD_PRELOAD`, `NIX_CFLAGS_LINK`, `NIX_CFLAGS_COMPILE`, `NIX_LDFLAGS`, `NIX_CC`, `NIX_BINTOOLS`, `CC`, and `LD` unset ran successfully.
- First sanitized Cargo run:
  - Command: `env -u LD_PRELOAD -u NIX_CFLAGS_LINK -u NIX_CFLAGS_COMPILE -u NIX_LDFLAGS -u NIX_CC -u NIX_BINTOOLS -u CC -u LD cargo test --workspace --all-targets`
  - Result: got past dependency build-script segfaults, then failed in `ring` because `CARGO_BUILD_TARGET=x86_64-unknown-linux-musl` was still inherited and `x86_64-linux-musl-gcc` was not found.
- Second sanitized Cargo run:
  - Command also removed `CARGO_BUILD_TARGET`, `CARGO_BUILD_RUSTFLAGS`, and `CARGO_PROFILE`.
  - Result: got to `tuwunel_core` link, then failed because inherited `JEMALLOC_OVERRIDE` pointed at a musl static jemalloc and produced undefined `sdallocx`, `nallocx`, `mallocx`, `sallocx`, and `rallocx` symbols.
- Third sanitized Cargo run:
  - Command also removed `JEMALLOC_OVERRIDE`, `ROCKSDB_LIB_DIR`, musl compiler variables, and added the local `liburing` pkg-config/include path.
  - Result: built and ran tests, then failed at runtime because `liburing.so.2` was not on the dynamic loader path.
- Fourth sanitized Cargo run:
  - Command added the local `liburing` runtime directory to `LD_LIBRARY_PATH`.
  - Result: built and ran tests, then failed with multiple RocksDB lock collisions under parallel test execution.
- Serial sanitized Cargo run:
  - Command added `RUST_TEST_THREADS=1`.
  - Result: reduced failures to `users::tests::self_deactivated_sso_account_reactivates`.
  - Root cause: `TUWUNEL_DATABASE_PATH` from the test/build environment overrides the users tests' generated TOML path via Figment environment providers, so both SSO service tests opened the same default `/var/tmp/tuwunel.db` database and the first `Services` instance kept its RocksDB lock in-process.
  - Confirmation: `cargo test -p tuwunel_service users::tests:: -- --nocapture` failed without `-u TUWUNEL_DATABASE_PATH`; the same command passed with `-u TUWUNEL_DATABASE_PATH`.
- Baseline fork-tip test with current command:
  - Command: the full command in [Test Command](#test-command).
  - Result: passed.
  - Summary: workspace/all-targets passed, including 161 passed / 2 ignored in `tuwunel_service` and 6 passed in `tuwunel_service` state-res integration tests.

## Rebase Result

- Rebase completed successfully: `main` now points at `b8ece3bd` on top of upstream `v1.6.1`.
- Backup refs restored after `--update-refs` moved them during rebase:
  - `backup/main-before-rebase-v1.6.1-20260504` -> `3cf5462f`.
  - `backup/origin-main-before-rebase-v1.6.1-20260504` -> `f562d1c3`.
- Final verification passed from the completed branch tip with the full command in [Test Command](#test-command), redirected to `/tmp/rebase-final-test.log`.
- Final summary: workspace/all-targets passed, including 168 passed / 2 ignored in `tuwunel_service` and 6 passed in `tuwunel_service` state-res integration tests.
- Final cleanliness checks passed: no conflict markers outside this note, and `git diff --check` is clean.
- Remaining working tree item: this rebase note is present as an untracked file at `docs/rebase-v1.6.1-2026-05-04.md`.

## Autosquash Pass

- Stop A1: folding `e6254e43` (`fixup! mindroom/edits: compact /sync and purge superseded edits`) into `890538e6`.
  - Conflict: `src/service/edit_purge/mod.rs`.
  - Cause: the timestamp-index fixup was created after the later sidecar-media commit existed, so its patch context tried to carry sidecar test helper functions backward into the earlier edit-purge commit.
  - Resolution: kept only the timestamp-index assertion helper and same-timestamp regression test in the earlier edit-purge commit; left media helper functions for the later sidecar cleanup commit.
  - Format gate before continue: `cargo fmt --check` failed in `src/main/args.rs`, which belongs to already-replayed commit `a7a9570` rather than this edit-purge conflict. Defer the formatting fix to the per-commit validation pass so it can be amended into the introducing commit.
  - Test gate before continue: passed with the full command in [Test Command](#test-command), redirected to `/tmp/autosquash-conflict-A1-test.log`.
- Stop A2: applying `d33186d4` (`mindroom/edits: clean up superseded sidecar media`) after autosquashing the earlier edit-purge timestamp test.
  - Conflict: `src/service/edit_purge/mod.rs`.
  - Cause: sidecar cleanup added media test helpers at the same helper-boundary where the earlier autosquashed timestamp test added `assert_event_payload_indexes_absent`.
  - Resolution: kept both helper sets: timestamp payload-index absence assertions and sidecar media ownership/presence helpers.
  - Format gate before continue: `cargo fmt --check` failed in already-replayed commits: `src/main/args.rs` from `a7a9570` and `src/core/config/mod.rs` from the default power-level override line. Defer both to the per-commit validation pass.
  - Test gate before continue: passed with the full command in [Test Command](#test-command), redirected to `/tmp/autosquash-conflict-A2-test.log`.

## Per-Commit Validation Pass

- Stop V1: commit `a7a9570` (`tests: isolate default_test database path`).
  - Gate failure: `cargo fmt --check` failed in `src/main/args.rs`.
  - Resolution: applied rustfmt's line wrapping to the new `database_path` option construction and amended the commit.
- Stop V2: commit `a83c88f1` (`auth/uiaa: add strict-CSP-safe SSO fallback flow`).
  - Gate failure: `cargo fmt --check` failed in `src/service/users/mod.rs`.
  - Resolution: applied rustfmt's multiline wrapping to `maybe_repair_legacy_sso_origin` and amended the commit.
- Stop V3: commit `21bf4daa` (`Add default room power level override`).
  - Gate failure: `cargo fmt --check` failed in `src/core/config/mod.rs`.
  - Follow-up: after amending the rustfmt change, the rescheduled exec passed but regenerated `tuwunel-example.toml` with the same wrapped comment, leaving the worktree dirty and preventing the rebase from advancing.
  - Resolution: amended both the source doc-comment wrap and the matching generated example-config wrap into the commit.

## Autosquash + Per-Commit Validation Result

- Autosquash completed with no remaining `fixup!` commits in `v1.6.1..HEAD`.
- Per-commit validation completed successfully across the rewritten fork commits using `git rebase --reschedule-failed-exec --exec` with:
  - `cargo fmt --check`.
  - The full command in [Test Command](#test-command), redirected per commit to `/tmp/rebase-exec-<shortsha>.log`.
- Rewritten `main` tip: `bbb48ce3` (`mindroom/edits: clean up superseded sidecar media`).
- Final tip verification:
  - `cargo fmt --check`: passed.
  - `git log --oneline --grep='fixup!' v1.6.1..HEAD`: empty.
  - Full workspace/all-targets test command: passed, redirected to `/tmp/rebase-final-after-autosquash-test.log`.
  - Final test summary included 171 passed / 2 ignored in `tuwunel_service` and 6 passed in `tuwunel_service` state-res integration tests.
- Remaining working tree item: this rebase note is still untracked at `docs/rebase-v1.6.1-2026-05-04.md`.
