# Tuwunel for Alpine Linux

Tuwunel runs on Alpine, and this page covers compiling it there yourself.
Building natively on Alpine means the compiler itself runs against musl, which
runs into a few things a normal build does not. The recipe below is tested on
`x86_64` and `aarch64`, but Alpine is not one of our release build platforms,
so treat it as a working recipe rather than a supported configuration.

## You may not need to build at all

Alpine packages tuwunel as
[`tuwunel`](https://pkgs.alpinelinux.org/package/edge/testing/x86_64/tuwunel),
for `x86_64` and `aarch64`. It lives in `edge/testing` and is in no stable
branch, so `apk add tuwunel` finds it only on edge with the testing repository
enabled. It is packaged by the Alpine community rather than published by this
project, so check the version it offers against our
[releases](https://github.com/matrix-construct/tuwunel/releases).

Our own release binaries are fully static and run on Alpine unmodified, and the
published container images are built around that same binary. Either of those
saves you a long compile.

If you want to build from source but not natively, the Nix route in
[Compiling](generic.md#compiling) produces a static binary and is reproducible
against CI.

## Prerequisites

```bash
apk add git rust cargo build-base clang-dev linux-headers liburing-dev
```

What the less obvious ones are for:

- `clang-dev` provides `libclang`, which `bindgen` uses to generate RocksDB's
  Rust bindings. There are no pregenerated bindings, so this is always needed.
- `liburing-dev` is needed because `io_uring` is enabled by default.
- `linux-headers` is the one that is easy to miss. `/usr/include/liburing.h`
  includes a kernel header, and Alpine's `liburing-dev` does not depend on
  `linux-headers`, so installing it alone leaves you with a header that cannot
  be compiled. The build fails inside RocksDB with:

  ```
  /usr/include/liburing.h:15:10: fatal error: linux/swab.h: No such file or directory
  ```

## Building

```bash
git clone https://github.com/matrix-construct/tuwunel
cd tuwunel
RUSTFLAGS="-C target-feature=-crt-static -C target-cpu=native" cargo build --release
```

The binary lands in `target/release/tuwunel`. Set `RUSTFLAGS` before you start,
because changing it later invalidates everything compiled so far and restarts
the build.

Configuration is no different from any other platform; see
[Configuration](../configuration.md).

### Why `-C target-feature=-crt-static`

Without it the build fails, so this one is not optional.

On a musl host, Rust links binaries statically by default, and that includes
the small helper programs Cargo builds and runs during compilation. Musl's
static C library cannot load shared libraries at runtime, and one of those
helpers needs to load `libclang` to generate RocksDB's bindings. You get:

```
Unable to find libclang: "the `libclang` shared library at
/usr/lib/llvm20/lib/libclang.so.20.1.8 could not be opened:
Dynamic loading not supported"
```

Turning off `crt-static` makes those helpers dynamically linked, which lets
them load `libclang` normally. There is no way to relax the setting for them
alone, because in a native build they and the server are built the same way.

### Why `-C target-cpu=native`

This one is optional and does not fix any failure, but it is worth setting.

On `x86_64` the default target has no SSE4.2, so RocksDB compiles in its
software checksum implementation and warns about it:

```
warning: compiling without SSE4.2: CRC will be slow
```

The warning overstates the cost, which is around one percent of RocksDB's CPU
time, but the flag is free. On `aarch64` there is no such warning, and the flag
instead enables newer processor features the baseline leaves out.

Use `native` when the binary stays on the machine that built it. If you plan to
move it to other hardware, name a specific architecture instead, such as
`broadwell` on `x86_64`.

## Moving the binary to another machine

The binary this recipe produces is not self-contained, which only matters if
you copy it somewhere else, such as into a slim container image. It needs
`liburing`, `libstdc++` and `libgcc` installed. The packages above already
bring those in, so the machine that compiled it can run it, but a bare Alpine
has none of them and startup fails naming symbols rather than the package:

```
Error loading shared library liburing.so.2: No such file or directory (needed by ./tuwunel)
Error relocating ./tuwunel: io_uring_submit: symbol not found
```

Installing them on the target resolves it:

```bash
apk add liburing libstdc++ libgcc
```

If you would rather have a portable binary, use a release artifact or the Nix
route above.

## Memory behavior on musl

Tuwunel's own allocations go through jemalloc. RocksDB's go to musl's
allocator instead, so a musl build splits its heap between the two. This is
normal for a musl build rather than a problem with yours; our glibc release
binaries use jemalloc throughout.

Both of those follow from how jemalloc is built here. On musl it is compiled
with a symbol prefix, so it provides `_rjem_malloc`, `_rjem_free` and the
rest rather than taking over the plain `malloc` that RocksDB and the other C
libraries call. Tuwunel's own code calls the prefixed names directly, which
is why its allocations still land in jemalloc.

The prefix covers jemalloc's configuration variable too, so a musl build
reads `_RJEM_MALLOC_CONF` where a glibc build reads `MALLOC_CONF`. The
tuning tuwunel ships with still applies; use the prefixed name to change it:

```bash
_RJEM_MALLOC_CONF=background_thread:false tuwunel
```

> [!NOTE]
> jemalloc ignores the unprefixed name in silence, with no warning that it
> went unread. If a setting appears to have no effect, check which name the
> binary actually read.

The same variable prints what a binary is actually running with:

```bash
_RJEM_MALLOC_CONF=stats_print:true tuwunel -V
```
