# Build Matrix

Tuwunel's CI tests a large combinatorial space of build configurations. The
matrix has six independent dimensions. Not all combinations are valid or useful,
so the bake configuration and workflow files apply exclusions to keep the total
job count tractable.


## Cargo Profiles

| Profile | Use | Notes |
|---|---|---|
| `test` | Most tests and linting | Debug-like; assertions enabled; fastest to compile |
| `release` | Production builds and smoke tests | Thin LTO; the profile shipped to users |
| `bench` | Benchmarks and Valgrind | Optimized with debug symbols for profiling |
| `release-debuginfo` | Release + full debug info | For crash analysis without sacrificing optimization |
| `release-native` | Performance benchmarking on CI hardware | `target-cpu=native`; non-portable; never shipped |


## Cargo Feature Sets

The feature sets are named groups of Cargo features, defined in `docker/bake.hcl`
as the `cargo_feat_sets` map.

#### `none`

No optional features. Produces the smallest, most minimal binary. Used to verify
that nothing in the default code path accidentally depends on an optional feature.

#### `default`

The feature set shipped to users in standard packages:

`brotli_compression`, `element_hacks`, `gzip_compression`, `io_uring`,
`jemalloc`, `jemalloc_conf`, `media_thumbnail`, `release_max_log_level`,
`systemd`, `url_preview`, `zstd_compression`

#### `logging`

All of `default` (without `release_max_log_level`) plus features useful for
development and diagnostics:

`blurhashing`, `bzip2_compression`, `console`, `direct_tls`, `jemalloc_prof`,
`jemalloc_stats`, `ldap`, `lz4_compression`, `perf_measurements`,
`sentry_telemetry`, `tokio_console`, `tuwunel_mods`

#### `all`

Everything, including `release_max_log_level`. The most exhaustive compilation
target; used for clippy, doc tests, and integration tests.

> [!NOTE]
> The feature `direct_tls` is always added to every build regardless of the
> selected feature set (`cargo_features_always` in `bake.hcl`).


## Rust Toolchains

| Toolchain key | Resolved to | Used for |
|---|---|---|
| `nightly` | The current nightly (or a specific nightly in CI) | Default for all builds; required for some flags and `rustfmt` options |
| `stable` | The MSRV from `rust-toolchain.toml` | Release packages, Nix smoke test |

The `bake.sh` script reads `rust-toolchain.toml` to resolve `stable` to the
project's minimum supported Rust version, ensuring released binaries are always
built against a pinned toolchain rather than whatever stable happens to be
current. Nix additionally verifies this with a SHA256 check in `flake.nix`.


## Rust Targets (Cross-compilation)

These are the `--target` values passed to Cargo:

| Target | Architecture |
|---|---|
| `x86_64-unknown-linux-gnu` | x86_64 Linux (primary) |
| `aarch64-unknown-linux-gnu` | ARM64 Linux |

Cross-compilation to `aarch64` runs on `ARM64` GitHub Actions runners and
produces binaries for Raspberry Pi 4+, AWS Graviton, and Apple Silicon (via
Rosetta or native under Linux).


## CPU Optimization Levels (System Targets)

The `sys_target` dimension controls which x86_64 microarchitecture level the
binary is compiled for. This affects both Rust compiler flags and, for targets
that include RocksDB, the native library build.

| sys_target | x86_64 level | Key instruction sets | Recommended for |
|---|---|---|---|
| `x86_64-v1-linux-gnu` | v1 baseline | SSE2 | Any x86_64 CPU; used for compatibility testing |
| `x86_64-v2-linux-gnu` | v2 | + POPCNT, SSE3, SSE4.1/4.2, SSSE3 | CPUs from ~2009+; minimum for good RocksDB CRC32 performance |
| `x86_64-v3-linux-gnu` | v3 | + AVX, AVX2, BMI1/2, F16C, FMA, MOVBE | Haswell (2013) and newer; the recommended shipping target |
| `x86_64-v4-linux-gnu` | v4 | + AVX-512F/BW/CD/DQ/VL | Skylake-X/Ice Lake server; highest throughput |
| `aarch64-v8-linux-gnu` | ARMv8 | NEON, AES, SHA | All 64-bit ARM |

> [!WARNING]
> Running a binary compiled for a higher level than the host CPU supports causes
> an `Illegal Instruction` (SIGILL) crash immediately on startup. The
> [generic deployment guide](../../deploying/generic.md) includes a command to
> determine which level your CPU supports.

RocksDB benefits significantly from hardware CRC32 (available from v2 onward)
and from SIMD compression routines (improved further at v3). For production
deployments, `-v3-` binaries are the recommended default.


## System Images

The base OS image for build and runtime containers. Currently only one system
is tested:

| sys_name | sys_version | Base image |
|---|---|---|
| `debian` | `testing-slim` | `debian:testing-slim` |


## Linking Mode

Static versus dynamic linking is selected automatically based on the profile and
toolchain:

| Condition | Linking |
|---|---|
| `release` or `bench` profile with `stable` toolchain | Static (`-C relocation-model=static`, `+crt-static`) |
| All other combinations | Dynamic (PIC, shared libc) |

Static linking produces fully portable binaries that run on any Linux system
without matching library versions. Dynamic linking is used during development and
testing because it produces faster incremental builds.


## Matrix Combinations in Practice

The CI does not test every possible combination — the cross-product of all
dimensions would be in the thousands. Instead, each workflow job selects a slice
of the matrix appropriate to its task:

| Job | Profiles | Feature sets | Toolchains | CPU targets |
|---|---|---|---|---|
| `clippy` | test, release, bench | none, default, all | nightly, stable | v1 |
| `unit` | test | all | nightly | v1 |
| `bench` | bench | all | nightly | v3 |
| `memcheck` | bench | all | nightly | v3 |
| `smoke` | test, release | default, all | nightly | v1, v3 |
| `rust-sdk-integ` | test, release | all | nightly | v1 |
| `complement` | test, release | all | nightly | v1 |
| `binary` (package) | release | default | stable | v1, v2, v3, v4, aarch64-v8 |
| `container` (package) | release | default | stable | v3 |
| `distro` (package) | release | default | stable | v1 |

The `test.yml` workflow embeds about 51 explicit exclusions to remove
combinations that are redundant, impossible, or not worth the resource cost.
