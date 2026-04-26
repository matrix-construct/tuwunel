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

##### Release build with stable Rust toolchain

```bash
export cargo_profile="release"
export rust_toolchain="stable"
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
