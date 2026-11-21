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

`brotli_compression`, `bzip2_compression`, `console`, `cpu_affinity`, `direct_tls`,
`element_hacks`, `email`, `gcs_storage`, `gzip_compression`, `io_uring`,
`jemalloc`, `jemalloc_conf`, `jwt_login`, `ldap`, `lz4_compression`, `markdown`,
`mas`, `media_thumbnail`, `oidc`, `release_max_log_level`, `s3_storage`,
`sentry_telemetry`, `synapse_admin`, `systemd`, `url_preview`, `zstd_compression`

#### `logging`

All of `default` without `release_max_log_level`, so release builds keep the
debug and trace log levels.

#### `all`

Everything, including `release_max_log_level`. The most exhaustive compilation
target; used for clippy, doc tests, and integration tests.

> [!NOTE]
> The feature `direct_tls` is always added to every build regardless of the
> selected feature set (`cargo_features_always` in `bake.hcl`).


## Rust Toolchains

| Toolchain | Version | Used for |
|---|---|---|
| `nightly` | Current nightly unless CI pins a version | Default for `docker/bake.sh`. Some compiler flags and `rustfmt` options require nightly. |
| `stable` | Current stable release unless CI pins a version | Release binaries, containers, Debian, RPM and Nix packages. CI also runs Clippy, unit, integration and smoke tests on stable. |
| `msrv` | Minimum supported Rust version from `rust-toolchain.toml` | Clippy, unit and integration tests, and release smoke testing on `x86_64-v1-linux-gnu`. |

The builder installs the current stable release when it builds the toolchain
layer. Use `[ci clean-rust]` to refresh it. To pin a version in CI, set the
`RUST_STABLE` or `RUST_NIGHTLY` repository variable. For `msrv`, `bake.sh`
reads the version from `rust-toolchain.toml`.

Nix takes its stable compiler from the locked `fenix` input. Update that input
to advance the Nix toolchain.


## Rust Targets (Cross-compilation)

These are the `--target` values passed to Cargo:

| Target | Architecture |
|---|---|
| `x86_64-unknown-linux-gnu` | x86_64 Linux (primary) |
| `aarch64-unknown-linux-gnu` | ARM64 Linux |

Cross-compilation to `aarch64` runs on `ARM64` GitHub Actions runners and
produces binaries for Raspberry Pi 4+, AWS Graviton, and Apple Silicon (via
Rosetta or native under Linux).

64-bit RISC-V is absent from the matrix because no runner provides that
architecture, but it does build and run. See [RISC-V](../riscv.md) for the
cross-compilation recipe.

32-bit ARM is absent for the same reason, and likewise builds and runs. See
[32-bit ARM](../arm32.md), which also covers why that port uses a glibc cross
toolchain where the RISC-V one uses musl.


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

For binary builds, including the RPM package, read the table from top to
bottom. The first matching row determines how the build links its libraries.

| Cargo profile | Rust toolchain | Linking |
|---|---|---|
| `release-native` | Requires `nightly` for its compiler flags | Static, tuned to the build host's CPU |
| `release`, `release-debuginfo` or `bench` | Any | Static (`-C relocation-model=static`, `+crt-static`) |
| Other, including `test` | `msrv` or `stable` | Static |
| Other, including `test` | `nightly` | Dynamic (PIC, shared `libc`) |
| Other | Any remaining toolchain | `rustc` defaults |

A statically linked binary carries its libraries with it. The host still needs
a compatible CPU. Dynamic linking speeds up incremental builds, for example
when you use `nightly` with the `test` profile.

The Debian package uses its own flags for every profile and toolchain. It uses
position-independent code (PIC) and links glibc's `libc` and `libm`
dynamically. It links `libstdc++` and any enabled compression and `io_uring`
libraries statically.


## Matrix Combinations in Practice

The table shows representative configurations. Each workflow starts with the
selected matrix and excludes combinations. For example, Clippy skips the
`bench` profile, and the smoke job skips `stable` and `msrv` for `test` builds.

| Job | Profiles | Feature sets | Toolchains | CPU targets |
|---|---|---|---|---|
| `clippy` | `test`, `release` | `none`, `default`, `all` | `nightly`, `msrv`, `stable` | `v1` |
| `unit` | `test` | `all` | `nightly`, `msrv`, `stable` | `v1` |
| `bench` | `bench` | `all` | `nightly` | `v3` |
| `memcheck` | `bench` | `all` | `nightly` | `v3` |
| `smoke` | `test`, `release` | `default`, `all` | `nightly`, `msrv`, `stable` | `v1`, `v3` |
| `rust-sdk-integ` | `test`, `bench` | `all` | `nightly` | `v1`, `v3` |
| `complement` | `test`, `bench` | `logging` | `nightly` | `v1`, `v3` |
| `binary` (package) | `release` | `default` | `stable` | `v1`, `v2`, `v3`, `v4`, `aarch64-v8` |
| `container` (package) | `release` | `default` | `stable` | `v3` |
| `distro` (package) | `release` | `default` | `stable` | `v1` |
