# Benchmarks and Performance

Tuwunel has three related performance paths. They answer different questions
and produce different kinds of output.

| Path | Question | Output |
|---|---|---|
| Criterion benchmarks | Did a focused operation get faster or slower? | Statistical timing and throughput samples |
| Local `perf` helper | Where does one benchmark spend CPU time? | Hardware counters and a sampled call graph |
| Complement perf in CI | How does an optimized server behave under a protocol workload? | Runtime metrics, hardware counters, and comparisons with prior runs |

## Run Criterion benchmarks locally

Benchmark targets are declared in each crate's `Cargo.toml`. The database codec
benchmarks use the `ser` target in the `tuwunel_database` package. From the
repository root, run the full target with:

```bash
cargo +nightly bench -p tuwunel_database --bench ser
```

Cargo prints the benchmark executable path in its build output. It normally
looks like `target/release/deps/ser-<hash>`. Build that executable without
running it when preparing to use the profiling helper:

```bash
cargo +nightly bench -p tuwunel_database --bench ser --no-run
```

Criterion benchmark names form paths. A full path is also a useful filter when
running the compiled executable directly:

```bash
target/release/deps/ser-<hash> \
  formats/cbor/scalar/database_serialize
```

The `ser` target includes database key serialization, CBOR and JSON format
comparisons, and allocation-sensitive minicbor cases. Criterion prints timing
and throughput estimates and stores its samples beneath `target/criterion/`.

Compare results on the same machine with the same toolchain, compiler flags,
and feature set. Keep other CPU-heavy work off the host during a comparison.
Criterion's baseline and significance controls are appropriate for timing
comparisons. The profiling helper below intentionally disables that analysis.

## Profile a benchmark with Linux perf

`tests/perf` runs a compiled Criterion benchmark under Linux
`perf`. It takes three positional arguments:

1. the benchmark executable;
2. a Criterion benchmark filter;
3. an output prefix.

Compile the benchmark first, copy the executable path printed by Cargo, then
run:

```bash
mkdir -p target/perf

tests/perf \
  target/release/deps/ser-<hash> \
  formats/cbor/scalar/database_serialize \
  target/perf/cbor-scalar
```

Use a narrow filter unless profiling several cases together is intentional.
The helper performs two fixed-duration passes:

1. `perf stat` records task time, scheduler activity, faults, instructions,
   cycles, branches, and cache counters in `target/perf/cbor-scalar.stat`.
2. `perf record` samples a call graph in
   `target/perf/cbor-scalar.data`.

Both passes use Criterion's profiling mode. Criterion iterates the matching
benchmark for the requested time without statistical analysis and without
updating its stored results. With the default settings, allow about 20 seconds
per matching benchmark, plus startup overhead.

Inspect the outputs with:

```bash
less target/perf/cbor-scalar.stat
perf report -i target/perf/cbor-scalar.data
```

The helper accepts these environment controls:

| Variable | Default | Effect |
|---|---:|---|
| `PROFILE_SECONDS` | `10` | Duration of each profiling pass |
| `PERF_COMMAND` | `perf` | Perf executable to invoke |
| `PERF_EVENT` | `cpu-clock` | Sampling event used by `perf record` |
| `PERF_FREQUENCY` | `999` | Sampling frequency used by `perf record` |

For example:

```bash
PROFILE_SECONDS=30 \
PERF_EVENT=cycles \
PERF_FREQUENCY=1999 \
tests/perf \
  target/release/deps/ser-<hash> \
  minicbor/tuple/stack_roundtrip \
  target/perf/minicbor-tuple
```

The `perf stat` event list is fixed in the helper. The event and frequency
variables affect only the sampling pass.

### Host requirements

The helper requires Linux perf tools that match the running kernel and
permission to open the requested performance events. Some distributions expose
`/usr/bin/perf` as a launcher and require a separate kernel-specific tools
package. Point `PERF_COMMAND` at the working executable when it is elsewhere.

Kernel policy may also restrict hardware counters or sampling to privileged
users. The local helper exits on such an error so it cannot silently produce an
incomplete profile. Symbol names are retained in the benchmark profile, but
complete call stacks still depend on the host's unwind support and perf setup.

## Performance coverage in CI

CI has two separate performance paths. Neither currently acts as an automatic
regression threshold.

### Criterion Bench job

The `Bench` job builds and runs the workspace benchmark targets with the
nightly toolchain, all features, the optimized `bench` profile, and the
`x86_64-v3` system target. The unit bake target runs library benchmarks; the
integration bake target runs each registered benchmark executable.

Criterion output is visible in the job log. CI does not currently retain the
Criterion sample directory as an artifact, compare it with a named baseline,
or fail a run because a timing changed. Treat this lane as benchmark build and
execution coverage, not as a stable comparison between shared-runner results.

### Complement server profiling

The optimized `Compliance (tuwunel)` job enables a separate perf wrapper around
each Tuwunel testee. This is a full server workload, not the
`tests/perf` helper. The job uses the `bench` profile, the
`logging` feature set, nightly Rust, and `x86_64-v3`.

The runner grants `CAP_PERFMON`, starts each server under `perf stat`, and
collects the report beside the server's runtime metrics. The job summary can
show instructions, cycles, IPC, branch misses, cache and TLB miss rates, and
frontend or backend bounds. Branch runs are compared with the main-branch
anchor and recent successful runs. Main-branch runs establish the anchor.
The raw bundle is uploaded as a `complement_runtime_metrics-*.tar.zst`
artifact.

If perf is missing or event access is denied, the CI wrapper logs a warning and
runs the server normally. Hardware-counter rows are omitted in that case.

The workflow currently enables this profiling automatically for the optimized
Tuwunel Complement job. It does not expose separate dispatch controls for the
event list, sampling rate, or perf enablement. Those values remain part of the
CI implementation.
