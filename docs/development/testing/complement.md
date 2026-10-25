# Matrix Protocol Compliance Testing

[Complement](https://github.com/matrix-org/complement) is the Matrix
protocol compliance test suite. It verifies that a homeserver correctly
implements the Matrix client-server and federation specifications by running
Go-based tests against live server instances. We maintain a fork at
[github.com/matrix-construct/complement](https://github.com/matrix-construct/complement)
with fixes for tests that had issues upstream.

Complement works differently from ordinary test runners: it uses the Docker
daemon API to create isolated networks and start fresh homeserver instances
for each test (or small group of tests). This requires the test runner itself
to have access to the Docker socket — which creates a docker-in-docker
situation when running inside a container.

Tuwunel's CI handles this by splitting the work into two images and a
shell script:

```
complement-tester   ← contains the Complement binary (the Go test runner)
complement-testee   ← contains the Tuwunel binary (the system under test)
```

`docker/complement.sh` runs `complement-tester` as a container with:
- The host Docker socket mounted (`-v /var/run/docker.sock:/var/run/docker.sock`)
- Host networking (`--network=host`) for test containers to communicate

The tester container then orchestrates the test run by telling the host Docker
daemon to start `complement-testee` instances, connect them, and run the tests.
The Go code inside the tester runs against these instances, while all the
container management happens on the host's daemon — not truly docker-in-docker.

## Running locally

Prerequisites:
- Docker with BuildKit and a configured builder.
- `network.host` entitlement enabled on the builder

##### Basic run (debug build, all tests)

```bash
docker/bake.sh complement-tester complement-testee && docker/complement.sh
```

##### Release build

```bash
cargo_profile="release" docker/bake.sh complement-tester complement-testee && \
  cargo_profile="release" docker/complement.sh
```

##### Release build with the msrv toolchain

```bash
export cargo_profile="release"
export rust_toolchain="msrv"
docker/bake.sh complement-tester complement-testee && docker/complement.sh
```

##### Run a single test by name

```bash
docker/bake.sh complement-tester complement-testee && \
  docker/complement.sh TestAvatarUrlUpdate
```

##### Run multiple tests by pattern

```bash
docker/bake.sh complement-tester complement-testee && \
  docker/complement.sh "TestAvatarUrlUpdate|TestEvent"
```

The argument to `complement.sh` becomes the `complement_run` regex, passed to
Go's test runner via `-run`.

##### View logs from the last run

```bash
cat tests/complement/logs.jsonl | jq .
```

## Results and baseline comparison

After each run, `complement.sh` extracts two files from the tester container:

| File | Contents |
|---|---|
| `tests/complement/results.jsonl` | Pass/fail result for every test case |
| `tests/complement/logs.jsonl` | Full verbose output from the test run |

The results file is version-controlled. `complement.sh` runs
`git diff --exit-code` on it: if results match the stored baseline exactly,
the script exits 0 (pass). Any change — new failures or new passes — produces
a non-zero exit and the diff is printed. In CI, the diff and logs are uploaded
as artifacts for review. It tracks *changes in compliance*. A test that was
previously failing and starts passing is caught just as clearly as a regression.
The baseline must be deliberately updated when the compliance profile
intentionally changes.


## Interoperability runs (heterogeneous homeservers)

By default every homeserver Complement spawns (`hs1`, `hs2`, ...) uses the same
`complement-testee` image, so a run tests Tuwunel against itself. Complement was
designed to also test different implementations against each other: it reads a
per-homeserver image override from `COMPLEMENT_BASE_IMAGE_<hsname>` and falls
back to the shared `COMPLEMENT_BASE_IMAGE` for any homeserver without one. An
*interop* run assigns a different image to one or more homeservers so federation
is exercised between two distinct servers. These are still honest peers; the
only difference is the implementation behind each one.

The most fundamental case is two builds of the same server (for example a
release image as `hs1` and a development image as `hs2`). The goal case is a
foreign implementation, such as Synapse against Tuwunel.

`docker/complement.sh` exposes this through a few selectors, in increasing
precedence:

| Selector | Effect |
|---|---|
| `interop=synapse` | `hs2` runs the published Synapse Complement image |
| `interop_image=<ref>` | `hs2` runs `<ref>` |
| `interop_hs="hs2 hs4"` | which homeservers the foreign image owns (default `hs2`) |
| `COMPLEMENT_BASE_IMAGE_hs2=<ref>` | raw per-homeserver passthrough |

The runner pre-pulls each foreign image into the host daemon (Complement will
not pull a missing image itself) and forwards the override into the tester
container. The suffix is normalized to lowercase because Complement looks the
override up by the literal homeserver name (`hs1`, `hs2`).

##### Synapse vs Tuwunel

```bash
docker/bake.sh complement-tester complement-testee && \
  interop=synapse docker/complement.sh "TestFederation.*"
```

`hs1` is Tuwunel and `hs2` is Synapse. The Synapse image
(`ghcr.io/element-hq/synapse/complement-synapse:latest`) is published by
Synapse's own CI, so this needs no local Synapse checkout and is reproducible in
CI. Override the ref with `synapse_image=...` for a pinned tag.

##### Two Tuwunel versions

```bash
# build a second testee from another checkout, tag it, then:
interop_image=complement-testee-prev:latest docker/complement.sh "TestFederation.*"
```

##### Federation TLS

For cross-implementation federation to work, each homeserver must present a
certificate that its peer trusts. Complement generates one Certificate Authority
per run and copies it into every container at `/complement/ca/`. The Tuwunel
testee's entrypoint signs a short-lived federation certificate for its
`$SERVER_NAME` with that CA at startup (mirroring how Synapse, Dendrite, and
others behave), so a peer that validates certificates (Synapse does) accepts the
connection. Outside Complement, where no run CA is present, the image keeps its
baked self-signed certificate.

##### Results

A heterogeneous run does not match the homogeneous baseline in
`tests/complement/results.jsonl`, so any interop selector switches the run to
report-only: results land under `tests/complement/interop/` and the baseline
gate is skipped. The run prints a pass/fail/skip summary instead. A dedicated
interop baseline (to gate the Synapse-vs-Tuwunel matrix on regressions) is a
natural follow-up.

##### In CI

CI carries two interop jobs, both reusing the homogeneous run's
`complement-tester` and `complement-testee` images. `Interoperability (synapse)`
runs them against the published Synapse image as `hs2`;
`Interoperability Inverted (synapse)` swaps the roles so Synapse drives as `hs1`
and Tuwunel answers as `hs2`, exercising the responder side. The peer name in
both titles comes from `complement_interop_peer`, which defaults to `synapse`.
Because the runs are report-only they never gate the pipeline; their outcome is
the per-run step summary plus the uploaded `complement_interop_*` artifacts.

**Both run by default, on every pipeline.** The `Main` workflow exposes
`enable_test_complement_interop` and `enable_test_complement_interop_inverse` as
dispatch inputs, plus `complement_interop_run` to narrow the selector. Those two
inputs are declared `default: false`, but the default is **inert**: they are
consumed as `fromJSON(inputs.<name> || 'true')`, and a false boolean is falsy in
a GitHub expression, so the `|| 'true'` branch is taken whether the box is ticked
or not. There is no commit-message directive for interop.

To actually skip them, disable the Complement stage: both jobs additionally
require `enable_complement` and `enable_complement_debug`, so neither runs when
Complement is off. The inverted job also requires the forward job's
`enable_complement_interop`, so it cannot run alone.

##### Known limitation: blueprint construction

Tests built on `Deploy(t, n)` construct a shared multi-server blueprint the
first time one runs in a package, then redeploy the committed images for each
test. The first redeploy of an overridden homeserver, immediately after that
construction, currently comes up without its `SERVER_NAME`. An image whose
entrypoint hard-requires that variable (Synapse signs its certificate from it)
fails to start for that one test, while later deploys of the same blueprint in
the run succeed. Federation queries on a freshly deployed pair (for example the
profile tests) are unaffected. This is a Complement-side interaction between
blueprint construction and per-homeserver image overrides, not a Tuwunel or
Synapse protocol difference.

## Image naming

Images are tagged with the full matrix vector so they can be unambiguously
matched:

```
complement-tester--<sys_name>--<sys_version>--<sys_target>
complement-testee--<cargo_profile>--<rust_toolchain>--<rust_target>--<feat_set>--<sys_name>--<sys_version>--<sys_target>
```

For example, a debug run produces:
```
complement-tester--debian--testing-slim--x86_64-v1-linux-gnu
complement-testee--test--nightly--x86_64-unknown-linux-gnu--all--debian--testing-slim--x86_64-v1-linux-gnu
```

---

## Nix-based Complement (unmaintained)

> [!WARNING]
> The workflow described below is **not currently maintained** and is no longer
> recommended. It is preserved here for any contributor who wants to reconstitute
> it.

Tuwunel's `flake.nix` provides a `complement` package that builds a Complement
OCI image using Nix. With [Nix and direnv installed](https://direnv.net/docs/hook.html)
(run `direnv allow` after setup):

- `./bin/complement "$COMPLEMENT_SRC"` — build, run, and output logs to the
  specified paths; also outputs the OCI image to `result`
- `nix build .#complement` — build just the OCI image (a `.tar.gz` at `result`)
- `nix build .#linux-complement` — for macOS hosts needing a Linux image

Pre-built images from CI artifacts can be placed at
`complement_oci_image.tar.gz` in the project root and used without Nix.
