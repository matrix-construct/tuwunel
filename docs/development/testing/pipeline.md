# Pipeline Phases

The CI pipeline runs in four sequential phases. Each phase must pass in its
entirety before the next begins. This structure is intentional: linting fails
fast before expensive test jobs start; tests must pass before build artifacts
are created; packages are only published when everything before them succeeded.

> [!TIP]
> Commit messages can suppress individual phases to avoid waiting for unrelated
> results when only interested in specific jobs.
>
> | Flag in commit message | Effect |
> |---|---|
> | `[ci no lint]` | Skip the linting phase |
> | `[ci no test]` | Skip the testing phase |
> | `[ci no build]` | Skip build targets within tests/packages |
> | `[ci no package]` | Skip the package phase |
> | `[ci no publish]` | Skip the publish phase |
> | `[ci only it]` | Run only integration tests |


## 1. Linting Phase

Linting runs first and fails fast. None of the heavier test or build jobs start
until every lint check passes. All lint jobs are in
`docker/` targets invoked through `.github/workflows/lint.yml`.

#### Format (`fmt`)

All Rust source must be formatted with **nightly** `rustfmt`. The check runs
`cargo fmt --check` and produces a diff showing every formatting violation if
it fails.

- Dockerfile: `docker/Dockerfile.cargo.fmt`
- Bake target: `fmt`
- Toolchain: nightly (nightly rustfmt has formatting options stable does not)

#### Typos (`typos`)

All text in the repository — source code, comments, and Markdown documentation
— is checked for spelling errors using the
[typos](https://github.com/crate-ci/typos) tool.

- Dockerfile: `docker/Dockerfile.cargo.typos`
- Bake target: `typos`

#### Security Audit (`audit`)

`cargo audit` checks every dependency in `Cargo.lock` against the
[RustSec Advisory Database](https://rustsec.org/). Any unignored advisory causes
a failure. This job runs on branch pushes and all "fat" refs (main, test,
release tags); it is skipped on pull requests to avoid blocking contributors
on advisories they did not introduce.

- Bake target: `audit`
- Workflow condition: `is_branch || is_fat`

#### Dead Link Check (`lychee`)

[lychee](https://github.com/lycheeverse/lychee) scans all Markdown files for
broken hyperlinks. Internal links (relative paths) and external URLs are both
checked.

- Dockerfile: `docker/Dockerfile.cargo.lychee`
- Bake target: `lychee`

#### Cargo Check (`check`)

`cargo check` is used as a fail-faster form of clippy, although the latter is
not gated on the former, the intent is to cancel all other tasks before they
inevitably fail on the same error. Only one instance of this is usually run.

- Bake target: `check`

#### Clippy (`clippy`)

`cargo clippy` runs with `--deny warnings` — any lint warning is a build
failure. Crucially, clippy runs for **every combination** of the build matrix:
all cargo profiles, all feature sets, stable and nightly toolchains. This
catches warnings that only manifest under specific feature combinations or with
a specific compiler, which individual developers rarely exercise locally.

- Bake target: `clippy`
- Policy: zero warnings permitted across all matrix dimensions



## 2. Testing Phase

The testing phase covers correctness at every level, from individual functions
to full Matrix protocol compliance. The most complex workflow. Receives matrix
selectors and per-test enable flags. Jobs are roughly ordered by cost: cheaper
jobs run first; expensive jobs (Nix, Complement) run last or are gated on
cheaper jobs passing.

#### Cargo Doc Tests (`doc`)

`cargo test --doc` runs all code examples embedded in rustdoc comments. Runs on
`release` profile with `all` features and `nightly` toolchain on `x86_64-v1`.

- Bake target: `doc`

#### Cargo Unit and Integration Tests (`unit`)

`cargo test` runs the module-level unit tests and any crate-associated or
binary-associated integration. Runs on `test` profile with `all` features
and `nightly` toolchain.

- Bake target: `unit`
- Valgrind variant: `unit-valgrind`

#### Cargo Benchmark Tests (`bench`)

Benchmarks are compiled (but not executed at full duration) to verify they build
without error. Runs on `bench` profile with `all` features and `nightly`.

- Bake target: `doc` (bench profile)

#### Valgrind Memory Checking (`memcheck`)

The integration test binary runs under [Valgrind](https://valgrind.org/) to
detect memory errors at runtime. Configured with:

```
--error-exitcode=1 --exit-on-first-error=yes --undef-value-errors=no --leak-check=no
```

Runs on `bench` profile with `all` features and `nightly` on `x86_64-v3`.

- Bake target: `integ-valgrind`

#### Smoke Tests (`smoke`)

Smoke tests exercise a running Tuwunel binary without a full client.
These tests run on main and test branches; some are skipped on pull
requests to reduce costs of scaling public contribution.

- Bake group: `smoke`
- Dockerfile: `docker/Dockerfile.smoketest`

#### Nix Smoke Test (`nix`)

Verifies that `nix build` still produces a working binary. Beyond the binary,
the Nix build also runs its own set of tests:

- Cargo unit and integration tests inside the Nix sandbox
- Requires all dependency Git commits to be reachable from a branch
- Validates that the SHA256 hash in `flake.nix` matches the current
  `rust-toolchain.toml` version — this catches MSRV bumps where the flake was
  not updated

Only runs on `main` and `test` branches with the `stable` toolchain. Pull
requests skip this test intentionally — it is expensive and rarely fails for
routine code changes.

- Bake target: `smoke-nix`
- Dockerfile: `docker/Dockerfile.nix`

#### Matrix Rust SDK Integration Tests (`rust-sdk-integ`)

Runs the [matrix-rust-sdk](https://github.com/matrix-org/matrix-rust-sdk)
client-server integration test suite against a live Tuwunel process. A Tuwunel
binary from a prior build layer is started in the background while `cargo test`
runs the SDK's integration test crate right in the docker builder.

- **Debug mode** (`test` profile): Exercises code paths with assertions enabled
  and catches logic errors that only appear with unoptimized code.
- **Release mode** (`release` profile): Ensures tests pass without concurrency
  hazards or other issues that optimized builds can expose.

For the above two matrix variations `rust-sdk-integ` is run for both and
`rust-sdk-valgrind` is run for `release` profile only.

- Bake targets: `rust-sdk-integ`, `rust-sdk-valgrind`
- Dockerfile: `docker/Dockerfile.matrix-rust-sdk`

#### Matrix Compliance (`complement-tester`/`complement-testee`)

See [Complement Testing](complement.md) for full details. Briefly: the
[Complement](https://github.com/matrix-org/complement) suite runs its Go tests
against containerized Tuwunel instances via the Docker daemon, verifying
conformance to the Matrix client-server and server-server specifications.

The `complement` job builds two images (tester and testee) via `bake.yml`.
The `compliance` job then runs `docker/complement.sh`, extracts result files
from the tester container, and runs `git diff` against the stored baseline.
The diff is uploaded as an artifact regardless of pass/fail, so reviewers can
see exactly what changed in compliance.

A file named `tests/complement/tuwunel.log` contains the server logs from the
last run extracted from the testee container and is also uploaded as an
artifact.


## 3. Package Phase

The package phase produces all distributable artifacts. It runs after tests
pass. The set of artifacts produced varies by branch:

| Branch / ref | Artifacts produced |
|---|---|
| Pull requests | Minimal: binary for the pushed architecture only |
| Regular branches | Binaries + containers for x86_64 |
| `main` / `test` | Full set including distro packages and all CPU variants |
| Release tags (`v*`) | Full set, identical to `main` |
| `test` branch only | Post-package install checks (deb and rpm) |


#### rustdoc (`docs`)

Builds the Rust API documentation with `cargo doc`. Runs on `release` profile,
`all` features, `nightly`, `x86_64-v1`.

- Bake target: `docs`
- Output: `/usr/src/tuwunel/target/<triple>/doc/`

#### mdBook (`book`)

Builds this documentation site using [mdBook](https://rust-lang.github.io/mdBook/).
Runs on `release` profile, `stable`, `x86_64-v1`.

- Bake target: `book`
- Output: `/book/`

#### Static Binaries (`binary`)

Compiles statically linked binaries for all supported targets. Packaged as
release assets on GitHub.

- Bake targets: `install` (dynamic), `static` (static), `oci`, `docker`
- CPU variants: `x86_64-v1`, `x86_64-v2`, `x86_64-v3`, `x86_64-v4`, `aarch64-v8`
  (full set on main/test/release; reduced set on other branches)

#### Container Images (`container`)

Builds OCI and Docker images for `docker` and `oci` container formats.

- Bake targets: `docker`, `oci`
- Output: images pushed in the publish phase to GHCR and Docker Hub

#### Distro Packages (`distro`)

Builds `.deb`, `.rpm`, and Nix packages. Only on `main`, `test`, and release
tags.

- Dockerfiles: `docker/Dockerfile.cargo.deb`, `docker/Dockerfile.cargo.rpm`

#### Post-package Checks (`checks`)

Installs the built `.deb` and `.rpm` into a clean environment and verifies the
package installs and the binary runs. Only on the `test` branch, where the most
thorough validation is desired.

- Bake targets: `deb-install`, `rpm-install`


## 4. Publish Phase

The publish phase runs only for `main` and release tags. All publication happens
here — nothing is pushed to external services during earlier phases. This keeps
publication atomic: if anything fails before this phase, no partially-complete
releases reach users.


#### GitHub Pages (`documents`)

Uploads the built mdBook site and rustdoc to GitHub Pages. Skipped for draft
releases.

- Workflow job: `documents`

#### Container Registries (`containers`)

Pushes container images to two registries simultaneously:

- **GitHub Container Registry** (`ghcr.io/matrix-construct/tuwunel`)
- **Docker Hub** (`docker.io`)

Images are compressed with zstd at level 11. Each image is tagged with the
Git SHA and branch/tag name.

- Workflow job: `containers`

#### Manifest Bundles (`bundles` / `delivery`)

After all per-image pushes complete, manifest lists are assembled that combine
multi-architecture images under a single tag. Manifests are pushed to both
registries in the `delivery` job, which depends on `bundles` and `documents`
both completing first.

| Manifest tag | Applied to |
|---|---|
| `main` | `main` branch pushes |
| `preview` | Release candidates (`-rc`, pre-release tags) |
| `latest` | Full release tags |
