# RISC-V

Tuwunel compiles and runs on 64-bit RISC-V. This is a working port, not a
supported platform: nothing in the CI matrix builds it, and no release artifact
is published for it. This page records how to reproduce the build and the
handful of things that differ from the architectures we do ship.

Verified against 1.8.3 on `riscv64gc-unknown-linux-musl`. The resulting server
opens its database and answers the client and federation version endpoints
under emulation.


## Why it is not in the build matrix

Every layer in `docker/bake.hcl` inherits its platform from the `system` target,
which declares `platforms` exactly once. A bake target therefore builds its
whole toolchain and runs its compile *inside* a container of the target
platform, and the workflow matrices exclude any job whose `rust_target`,
`sys_target` and runner architecture disagree. Each architecture is built
natively on a matching runner.

There is no RISC-V runner, so adding a bake target would mean running the
compiler itself under emulation. Compiling for RISC-V is cheap; compiling as
RISC-V is not. The recipe below cross-compiles instead, pinning the build stages
to `linux/amd64` and only the runtime stage to `linux/riscv64`.


## Toolchain

The build needs a RISC-V cross toolchain with three properties, and one fix:

| Requirement | Why |
|---|---|
| GCC 12 or newer | RocksDB requires `-std=c++20`; GCC 9, which older musl cross images ship, does not accept it |
| `libstdc++.a` and `libatomic.a` for the target | The static RocksDB link needs the first; jemalloc emits `rustc-link-lib=atomic` on any RISC-V target, because GCC opencodes `__atomic_exchange_1` as a library call there |
| libclang | `rust-librocksdb-sys` runs bindgen |
| Linux UAPI headers in the sysroot | See below |

Most musl cross images ship no `linux/` or `asm/` include tree in their RISC-V
sysroot, where their x86_64 sysroot carries several hundred headers. RocksDB's
`env/io_posix.cc` includes `<linux/fs.h>` unconditionally on Linux, so the build
fails with `fatal error: linux/fs.h: No such file or directory`. Ubuntu packages
the tree as `linux-libc-dev-riscv64-cross`. UAPI headers are independent of the
C library, so the glibc cross package is the correct source for a musl sysroot.


## Building a container image

Save this as `Dockerfile.riscv64` and build it from the repository root. The
repository `.dockerignore` already excludes `target/` and `.git`.

```dockerfile
# syntax=docker/dockerfile:1

ARG RUST_TARGET=riscv64gc-unknown-linux-musl
ARG SYSROOT=/usr/local/musl/riscv64-unknown-linux-musl

# The build stages are pinned to linux/amd64 rather than $BUILDPLATFORM because
# the cross image is published for amd64 only. On an amd64 host the compiler
# runs natively; elsewhere it runs under emulation, which is still far cheaper
# than emulating the compiler as a RISC-V binary.
FROM --platform=linux/amd64 messense/rust-musl-cross:riscv64-musl AS toolchain

ARG SYSROOT
RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends linux-libc-dev-riscv64-cross; \
    src="$(dpkg -L linux-libc-dev-riscv64-cross \
        | grep -m1 '/include/linux/fs\.h$' | sed 's|/linux/fs\.h$||')"; \
    cp -a "${src}/." "${SYSROOT}/include/"; \
    test -f "${SYSROOT}/include/linux/fs.h"; \
    test -d "${SYSROOT}/include/asm"

FROM toolchain AS build

WORKDIR /usr/src/tuwunel
COPY .cargo .cargo
COPY Cargo.toml Cargo.lock clippy.toml rustfmt.toml tuwunel-example.toml ./
COPY src src

ARG RUST_TARGET
ARG SYSROOT
ARG FEATURES="brotli_compression,cpu_affinity,element_hacks,email,gcs_storage,gzip_compression,jemalloc,jemalloc_conf,markdown,media_thumbnail,release_max_log_level,s3_storage,systemd,url_preview,zstd_compression"

# RUSTUP_TOOLCHAIN pins the toolchain the image already carries, so that
# rust-toolchain.toml does not send rustup after a same-versioned but
# differently named toolchain and its components.
#
# clang does not recognise the "riscv64gc" architecture that bindgen forwards
# from the Rust triple, and silently falls back to the host headers unless the
# clang triple and sysroot are given explicitly.
RUN --mount=type=cache,target=/root/.cargo/registry,sharing=locked \
    --mount=type=cache,target=/root/.cargo/git,sharing=locked \
    --mount=type=cache,target=/usr/src/tuwunel/target,sharing=locked \
    set -eux; \
    RUSTUP_TOOLCHAIN=stable \
    LIBCLANG_PATH=/usr/lib/x86_64-linux-gnu \
    ROCKSDB_STATIC=1 \
    BINDGEN_EXTRA_CLANG_ARGS="--target=riscv64-unknown-linux-musl --sysroot=${SYSROOT}" \
    cargo build --release --target "${RUST_TARGET}" -p tuwunel \
        --no-default-features --features "${FEATURES}"; \
    install -Dm755 "target/${RUST_TARGET}/release/tuwunel" /out/tuwunel

FROM --platform=linux/riscv64 alpine:latest AS runtime

RUN apk add --no-cache libstdc++ libgcc libatomic ca-certificates

COPY --from=build /out/tuwunel /usr/local/bin/tuwunel

RUN ["/usr/local/bin/tuwunel", "--version"]

EXPOSE 8008
ENTRYPOINT ["/usr/local/bin/tuwunel"]
```

```bash
docker buildx build --platform linux/riscv64 \
    -f Dockerfile.riscv64 -t tuwunel:riscv64 --load .
```

A cold build takes about fourteen minutes and produces a 45 MB image.

The `RUN ["/usr/local/bin/tuwunel", "--version"]` line executes under
emulation during the build, so a binary that cannot run fails the build
rather than shipping.

Both Alpine and Debian publish `riscv64` base images, so either works for the
runtime stage. Alpine is used here because the binary is linked against musl.


## Running under emulation

No extra tooling is required if the host has `binfmt_misc` handlers registered
for RISC-V, which Docker Desktop does by default. Confirm with:

```bash
docker buildx inspect | grep riscv64
docker run --rm --platform linux/riscv64 alpine uname -m
```

Then run the image as usual:

```bash
docker run --rm --platform linux/riscv64 -p 8008:8008 tuwunel:riscv64 \
    -O server_name='"example.com"' -O address='"0.0.0.0"' -O port=8008
```

Startup is unhurried but not slow: the database opens its 133 column families
in roughly half a second and the server answers requests within about five
seconds of launch.


## Compiling with io_uring

The recipe above leaves `io_uring` out so that the image runs under emulation.
The feature does compile and link for RISC-V, and the paragraphs below cover
that, but treat it as a compile test only: user-mode emulation does not
implement the `io_uring` syscalls, so none of this says the code works on real
hardware.

Two things have to join the toolchain stage. The first is `pkg-config`, which
the base image does not carry. The tree enables plain `rust-rocksdb/io-uring`,
so `librocksdb-sys` takes the `pkg_config::probe_library("liburing")` path, and
the pkg-config crate shells out to that binary; without it the build fails
before it reaches the compiler. The second is liburing built for the target.

liburing has to be compiled with `-fPIC`. Its build reserves that flag for the
shared library and compiles the archive's objects without it, so linking the
position independent executable this target produces otherwise fails with:

```
liburing.a(setup.ol): relocation R_RISCV_HI20 against `a local symbol' can not
be used when making a shared object; recompile with -fPIC
```

An x86_64 musl build never hits this, because that target is `crt-static` and
does not produce a position independent executable.

Installing the archive and no shared object leaves `-luring` resolving to
`liburing.a`, so the binary carries io_uring with no runtime liburing
dependency.

```dockerfile
RUN apt-get update; \
    apt-get install -y --no-install-recommends pkg-config curl ca-certificates; \
    rm -rf /var/lib/apt/lists/*

ARG LIBURING_VERSION=2.6
RUN set -eux; \
    cd /tmp; \
    curl -fSL "https://github.com/axboe/liburing/archive/refs/tags/liburing-${LIBURING_VERSION}.tar.gz" \
        -o liburing.tgz; \
    tar xf liburing.tgz; \
    cd "liburing-liburing-${LIBURING_VERSION}"; \
    ./configure \
        --cc=riscv64-unknown-linux-musl-gcc \
        --cxx=riscv64-unknown-linux-musl-g++ \
        --prefix="${SYSROOT}"; \
    make -C src liburing.a CC="riscv64-unknown-linux-musl-gcc -fPIC"; \
    install -Dm644 src/liburing.a "${SYSROOT}/lib/liburing.a"; \
    cp -a src/include/liburing.h src/include/liburing "${SYSROOT}/include/"; \
    mkdir -p "${SYSROOT}/lib/pkgconfig"; \
    printf '%s\n' \
        "prefix=${SYSROOT}" \
        'exec_prefix=${prefix}' \
        'libdir=${exec_prefix}/lib' \
        'includedir=${prefix}/include' \
        '' \
        'Name: liburing' \
        "Version: ${LIBURING_VERSION}" \
        'Description: io_uring library' \
        'Libs: -L${libdir} -luring' \
        'Cflags: -I${includedir}' \
        > "${SYSROOT}/lib/pkgconfig/liburing.pc"; \
    rm -rf /tmp/liburing*
```

Nothing further is needed to make the probe find it when cross-compiling: the
cross image exports `TARGET_PKG_CONFIG_LIBDIR` and
`TARGET_PKG_CONFIG_ALLOW_CROSS`, and the pkg-config crate honours the `TARGET_`
prefix. Add `io_uring` to `FEATURES` and the build proceeds.

Confirm the feature really compiled in rather than being quietly skipped, by
looking for RocksDB's io_uring code in the result:

```console
$ strings -a tuwunel | grep -c io_uring_submit
10
$ strings -a tuwunel | grep -c FinalizeAsyncRead
1
```

Those message strings, and the
`rocksdb::FinalizeAsyncRead(io_uring*, io_uring_cqe*, ...)` symbol beside them,
exist only under `ROCKSDB_IOURING_PRESENT`. `readelf -d` should still list no
liburing entry, which confirms the static link.


## Differences from the shipped architectures

**`io_uring` is off by default here.** The recipe above omits it, so a RISC-V
image built that way is not feature equivalent to an x86_64 or ARM64 one. It
compiles when liburing is added, as described above, but has never been run.

**The binary is dynamically linked.** Unlike `x86_64-unknown-linux-musl`, the
`riscv64gc-unknown-linux-musl` target does not enable `crt-static` by default,
which is a property of the Rust target definition rather than of our build:

```console
$ rustc --print cfg --target x86_64-unknown-linux-musl | grep crt-static
target_feature="crt-static"
$ rustc --print cfg --target riscv64gc-unknown-linux-musl | grep crt-static
```

The result needs musl plus the GCC runtime libraries at run time, which is why
the runtime stage installs them. Adding `-C target-feature=+crt-static` should
produce a static binary, at the cost of a full rebuild.

**No CPU feature levels.** The `sys_target` dimension has no RISC-V equivalent
here. The build targets baseline `rv64gc` with no profile selection.


## Adding it to the build matrix

Should CI ever gain a RISC-V runner, the changes to `docker/bake.hcl` are small:

- a `riscv64-v1-linux-gnu` value for `sys_target`, which the existing
  `sys_target_triple`, `sys_target_ver` and `sys_target_isa` helpers already
  parse correctly
- a `["linux/riscv64"]` branch in the `platforms` expression on the `system`
  target
- `-C link-arg=-l:libatomic.a` in the static rustflags, as the RISC-V analogue
  of the `libgcc.a` entry that ARM64 carries
- plain `-ftls-model=initial-exec` in `rocksdb_cxx_flags`, with neither ARM64's
  `-mno-outline-atomics` nor x86_64's `-mpclmul`
- no `-C target-cpu=`, which is set for x86_64 only

The base layers would also need every apt package they install to exist for
`riscv64`.
