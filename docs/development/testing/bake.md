# Docker Bake System

The builder is implemented using [Docker Buildx Bake](https://docs.docker.com/build/bake/),
a declarative build system for multi-stage Docker builds. All configuration lives
in `docker/bake.hcl`. The shell script `docker/bake.sh` is the user-facing entry
point for invoking it.

A conventional CI system runs sequential jobs that each build everything they
need from scratch, or caches at coarse granularity (the whole Cargo registry,
the whole `target/` directory). Bake instead models the build as a directed
acyclic graph of image layers. Intermediate layers are shared across every
target that needs them: if `deps-build` already exists in the cache, every
target that depends on it — `unit`, `clippy`, `install`, `deb` — skips
rebuilding it and jumps straight to their unique work.

This is especially valuable for the build matrix: compiling dependencies once
per (toolchain, feature set) pair and reusing that layer for all tests and build
targets on top of it cuts total CI time dramatically.


## Directory layout

| File | Role |
|---|---|
| `docker/bake.hcl` | Declarative target graph; all variables, groups, and targets |
| `docker/bake.sh` | Shell wrapper; sets defaults, invokes `docker buildx bake` |
| `docker/complement.sh` | Complement test orchestrator; runs after bake builds the images |
| `docker/Dockerfile.*` | "Library functions" — generic, variable-driven build stages |

The `Dockerfile.*` files are intentionally generic: they accept `ARG` variables
and are reused across many targets via Bake's variable substitution, rather than
having one Dockerfile per use case.

### Builder setup

Bake requires a named BuildKit builder. Locally, the builder name defaults to
`owo`. In CI it is the GitHub actor name (`$GITHUB_ACTOR`). Create one with:

```bash
cat <<EOF > buildkitd.toml
[system]
  platformsCacheMaxAge = "504h"
[worker.oci]
  enabled = true
  gc = true
  reservedSpace = "64GB"
  maxUsedSpace = "128GB"
[[worker.oci.gcpolicy]]
  reservedSpace = "64GB"
  maxUsedSpace = "128GB"
  all = true
EOF

docker buildx create \
  --name owo \
  --bootstrap \
  --buildkitd-config ./buildkitd.toml \
  --driver docker-container \
  --buildkitd-flags "--allow-insecure-entitlement network.host"
```

The `--allow-insecure-entitlement network.host` flag is required for
Complement (which needs host networking during testing). It can be omitted if
you only need to run other targets.


## bake.sh usage

`docker/bake.sh` is the standard entry point. It accepts target names as
positional arguments and reads matrix dimensions from environment variables.

```bash
# Basic usage
docker/bake.sh <target> [<target>...]

# With environment overrides (singular form for convenience)
cargo_profile="release" rust_toolchain="stable" docker/bake.sh install

# Multiple targets
docker/bake.sh fmt clippy
```

## Targets

Bake groups collect multiple targets under one name for convenience:

| Group | Members |
|---|---|
| `lints` | `audit`, `check`, `clippy`, `fmt`, `lychee`, `typos` |
| `tests` | All unit, integ, doc, bench targets |
| `smoke` | `smoke-version`, `smoke-startup`, `smoke-perf`, `smoke-valgrind`, `smoke-nix` |
| `integration` | `rust-sdk-integ`, `rust-sdk-valgrind` |
| `complement` | All complement tester/testee targets |
| `installs` | `install`, `static`, `docker`, `oci` |
| `pkg` | `book`, `docs`, `deb`, `deb-install`, `rpm`, `rpm-install`, `nix` |
| `publish` | `ghcr_io`, `docker_io` |
| `default` | A representative single-vector build |


## Variables

Variables at the top of `bake.hcl` control every aspect of the build. Most have
sensible defaults for local use and are overridden by `bake.sh` or the CI
workflows for production runs.

### Single-value selectors

| Environment | Default | Description |
|---|---|---|
| `cargo_profile` | `test` | Single profile (wrapped into JSON array) |
| `feat_set` | `all` | Single feature set |
| `rust_toolchain` | `nightly` | Single toolchain |
| `rust_target` | `x86_64-unknown-linux-gnu` | Single Rust target triple |
| `sys_name` | `debian` | Base OS name |
| `sys_version` | `testing-slim` | Base OS version |
| `sys_target` | `x86_64-v1-linux-gnu` | CPU optimization level |

For multi-value matrix runs (as used in CI), set the `cargo_profiles`,
`feat_sets`, etc. variables directly to JSON arrays.


### Multi-value defaults

```hcl
variable "cargo_profiles"   { default = "[\"test\", \"release\"]"          }
variable "feat_sets"        { default = "[\"none\", \"default\", \"all\"]" }
variable "rust_toolchains"  { default = "[\"nightly\", \"stable\"]"        }
variable "rust_targets"     { default = "[\"x86_64-unknown-linux-gnu\"]"   }
variable "sys_names"        { default = "[\"debian\"]"                     }
variable "sys_versions"     { default = "[\"testing-slim\"]"               }
variable "sys_targets"      { default = "[\"x86_64-v1-linux-gnu\"]"        }
```

These are escaped JSON arrays passed as strings.


## Target Hierarchy

Every target is a leaf or branch in a single dependency tree. Building a leaf
automatically triggers all its transitive dependencies. The tree (bottom to top):

```
system                      ← Debian base image + runtime packages
  ├── rust                  ← rustup + toolchain installation
  │     └── rustup
  ├── rocksdb-fetch         ← RocksDB source checkout
  │     └── rocksdb-build   ← compiled librocksdb
  │           └── rocksdb
  ├── valgrind              ← Valgrind installation
  └── perf                  ← Linux perf tools

kitchen                     ← build environment (inherits system + rust + rocksdb)
  └── builder               ← adds nproc/env setup

source                      ← project source code (via git checkout)
  └── preparing             ← sets up cargo workspace for chef
        └── ingredients     ← cargo-chef recipe
              └── recipe    ← pre-built dependency layer (cargo-chef cook)

deps-base          ← base dependency compilation
deps-build         ← full dep build (links recipe + kitchen)
deps-build-tests   ← test deps
deps-build-bins    ← binary deps
deps-clippy        ← clippy-specific deps
deps-check         ← check-specific deps

build              ← compiles the main binary
build-tests        ← compiles test binary
build-bins         ← compiles all binaries
build-deb          ← deb packaging build
build-rpm          ← rpm packaging build

# Lint targets (leaves):
fmt  typos  audit  lychee  check  clippy

# Test targets (leaves):
doc  unit  unit-valgrind  integ  integ-valgrind
smoke-version  smoke-startup  smoke-perf  smoke-valgrind  smoke-nix
rust-sdk-integ  rust-sdk-valgrind

# Complement targets (leaves):
complement-base  complement-config
complement-tester  complement-tester-valgrind
complement-testee  complement-testee-valgrind

# Install/package targets (leaves):
install  install-valgrind  install-perf  static
docker  oci
deb  deb-install  rpm  rpm-install  nix  build-nix
book  docs

# Publish targets (leaves):
ghcr_io  docker_io
```
