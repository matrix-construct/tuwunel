# Tuwunel for OpenBSD

Tuwunel builds and runs on OpenBSD, but OpenBSD is not a supported platform.
Nothing in the CI matrix builds it, no release artifact is published for it, and
there is no port or package. Of the three BSDs this takes the most setup, and
one requirement is not optional: **the Rust in 7.9-release is too old**, so the
compiler has to come from `-current`.

Verified against 1.8.3 on OpenBSD 7.9, `arm64` and `amd64`. The resulting server
opens its database, answers the client and federation version endpoints,
registers an account, creates a room, sends and reads back a message, reopens a
populated database across a restart, and exits cleanly on `SIGTERM`. That
binary was built without `jemalloc`. A release build with the feature set below,
`jemalloc` included, passed the same checks on both architectures against the
tree after 1.9.1.

Contributions for getting Tuwunel into ports are welcome.


## The compiler has to come from -current

7.9-release packages Rust 1.94.1. The workspace declares `rust-version =
"1.95.0"` and means it:

```
error[E0658]: `if let` guards are experimental
   --> src/service/resolver/actual.rs:116:10
    |
116 |         | None if let Some(pos) = dest.as_str().find(':') =>
```

`if let` match guards were stabilised in 1.95.0. `cargo build
--ignore-rust-version` skips the manifest check but not the compiler, so it does
not help here.

This is the same on both architectures: 7.9-release packages 1.94.1 for `arm64`
and for `amd64`.

The `-current` snapshots carry 1.97.1, which installs and runs on a 7.9 system.
The last path element is the package architecture, `aarch64` or `amd64`:

```sh
PKG_PATH=https://cdn.openbsd.org/pub/OpenBSD/snapshots/packages/amd64/ \
    pkg_add -u rust
```

Mixing a snapshot package into a release system is not something OpenBSD
supports in general. Nothing else was taken from `-current` for this build, and
`rustc` and `cargo` ran without complaint, but treat it as the compromise it is:
a build host running `-current`, or waiting for a release whose `rust` package is
1.95 or newer, is the cleaner arrangement.


## Toolchain

```sh
pkg_add git cmake gmake curl
pkg_add -I llvm-21.1.8p4
```

`pkg_add llvm` on its own fails with `Ambiguous: llvm could be llvm-19.1.7p14
llvm-21.1.8p4 llvm-20.1.8p6` and installs nothing, so name the version. It
provides `/usr/local/llvm21/lib/libclang.so.0.0`, which `rust-librocksdb-sys`
runs bindgen against; point the build at that directory with `LIBCLANG_PATH`.
OpenBSD ships no unversioned `libclang.so` symlink, which is fine, because
bindgen matches the versioned name.

This is the set that was verified rather than a minimal one. `curl` is only used
by the checks further down this page.


## Raising the login class limits

This is the step most likely to be missed. The `daemon` class root logs in under
allows a 4 GB data segment and 128 open files, and neither the build nor the
server fits in that. Both limits are hard ceilings set by the login class, so
`ulimit` alone cannot lift them.

Append a class to `/etc/login.conf`:

```
tuwunel:\
	:datasize-cur=infinity:\
	:datasize-max=infinity:\
	:openfiles-cur=8192:\
	:openfiles-max=16384:\
	:maxproc-cur=1024:\
	:maxproc-max=2048:\
	:stacksize-cur=16M:\
	:tc=daemon:
```

Rebuild the capability database if one exists, and raise the system wide file
table, which defaults to 7030 and would otherwise cap the per-process figure
above:

```sh
[ -f /etc/login.conf.db ] && cap_mkdb /etc/login.conf
sysctl kern.maxfiles=32768
echo "kern.maxfiles=32768" >> /etc/sysctl.conf
```

Put the account doing the build into the class and log in again:

```sh
usermod -L tuwunel root
```

`ulimit -a` should then report `nofiles 8192`, and a data segment raised well
clear of the 4 GB default. `infinity` means the architecture ceiling rather than
a literal absence of one, so `arm64` reports `unlimited` and `amd64` reports
128 GB.


## Where to build

The installer's auto layout splits the disk into small partitions, and on an
80 GB disk the only one large enough for a cargo target directory is `/home`:

```console
$ df -h
/dev/sd0a      986M   85.2M    851M    10%    /
/dev/sd0l     27.8G   18.0K   26.5G     1%    /home
/dev/sd0h     10.5G    146K   10.0G     1%    /usr/local
```

Set `CARGO_TARGET_DIR` accordingly rather than building in place.


## RocksDB include ordering

Nothing extra is needed here now, but a build against an older `rust-rocksdb`
pin fails on `aarch64` like this:

```
/usr/include/sys/sysctl.h:373:2: error: unknown type name 'u_int64_t'
```

`rocksdb/util/crc32c_arm64.cc` has an OpenBSD include block, and it listed
`<sys/sysctl.h>` before `<sys/types.h>`. OpenBSD's `sysctl.h` is not self
contained and needs the types first, so the whole header failed to parse. That
block has probably never compiled since it was written.

The order is corrected as of `rust-rocksdb` `c6e73b5`, which is what the
workspace pins. Building an older revision needs the types forced in ahead of
everything instead:

```sh
export CXXFLAGS="-include sys/types.h"
```

This affects `aarch64` only. `crc32c_arm64.cc` is not compiled on `amd64`.

The file then compiles and the runtime check works as written: OpenBSD reads
`CTL_MACHDEP`/`CPU_ID_AA64ISAR0` through `sysctl` and selects the hardware
CRC32C path when the CPU reports it.


## RocksDB on amd64

Nothing fails to build here, but the default `amd64` build is slower than it
needs to be. RocksDB picks its CRC32C implementation when it is compiled, and on
x86 there is no runtime fallback:

```
// NOTE: runtime detection no longer supported on x86
```

`cargo build` targets the `x86-64` baseline, which carries `fxsr`, `sse` and
`sse2` and nothing else. `__SSE4_2__` is therefore never defined, and the table
driven software routine is what ends up in the binary, on hardware that has the
instruction. Every write-ahead log record is checksummed with it, and so is
every record read back during recovery. Table blocks are checksummed with XXH3
instead and are unaffected. `librocksdb-sys` says so during the build, though
cargo hides build warnings from dependencies by default:

```
compiling without SSE4.2: CRC will be slow (set RUSTFLAGS="-Ctarget-cpu=..."
to optimize RocksDB e.g. -Ctarget-cpu=broadwell)
```

Raising the target CPU is all it takes:

```sh
RUSTFLAGS="-Ctarget-cpu=westmere" cargo build --release -p tuwunel \
    --no-default-features --features ...
```

`sse4.2` alone buys the single stream hardware CRC32. The three way version
RocksDB prefers also wants `pclmulqdq`, and `westmere` is the oldest
`-Ctarget-cpu` supplying both; anything newer serves. Comparing the two builds
confirms it: the baseline one carries RocksDB's software lookup tables and no
`crc32c_3way`, and the `westmere` one carries `crc32c_3way` and no tables.
Neither build time nor binary size moved meaningfully.

This is the axis the `x86_64-v1` through `x86_64-v4` release packages sit on. A
build with no `-Ctarget-cpu` is the `v1` one, and nothing selects a higher level
for you here.


## Features

The default feature set includes `io_uring` and `systemd`, both of which are
Linux only. Every OpenBSD build therefore has to opt out of the defaults and
name its features explicitly:

```
brotli_compression
element_hacks
gzip_compression
jemalloc
jemalloc_conf
media_thumbnail
release_max_log_level
url_preview
zstd_compression
```

That is the default set with `io_uring` and `systemd` removed.

Tuwunel 1.9.1 and earlier cannot use `jemalloc` here: a binary built with it
aborts before it starts. Build those releases without `jemalloc` and
`jemalloc_conf`, which leaves OpenBSD's own allocator in charge. Later releases
can leave both out too, if you would rather every allocation went through
OpenBSD's allocator and its hardening.

On OpenBSD jemalloc is built with a symbol prefix, so Tuwunel's own allocations
go through it while RocksDB and the other C and C++ libraries keep OpenBSD's
allocator. The prefix covers jemalloc's configuration variable too: to change
the tuning Tuwunel ships with, set `_RJEM_MALLOC_CONF` rather than
`MALLOC_CONF`, and check what a binary is running with:

```sh
_RJEM_MALLOC_CONF=stats_print:true tuwunel -V
```


## Building

```sh
export PATH=/usr/local/bin:/usr/local/sbin:$PATH
export LIBCLANG_PATH=/usr/local/llvm21/lib
export CARGO_TARGET_DIR=/home/build/target

cargo build --release -p tuwunel --no-default-features \
    --features brotli_compression,element_hacks,gzip_compression,jemalloc,jemalloc_conf,media_thumbnail,release_max_log_level,url_preview,zstd_compression
```

Building the 1.8.3 release rather than the current tree needs one more change.
`tuwunel_core` denies `missing_docs`, and the OpenBSD arm of
`available_parallelism` in `src/core/utils/sys/compute.rs` carried no doc
comment, so the crate failed to compile on OpenBSD and nowhere else:

```
error: missing documentation for a function
   --> src/core/utils/sys/compute.rs:131:1
    |
131 | pub fn available_parallelism() -> usize { num_cpus::get() }
```

Adding a doc comment above it is enough. This is fixed in the tree.

A cold build took about 57 minutes on 2 jobs on `arm64`, and about 80 minutes on
2 jobs on `amd64`, producing an 81.6 MB and a 90 MB binary respectively. The
`amd64` timing is from the tuned build described above, which is the cold one
that was measured; the flag does not change build time. The result is
dynamically linked, and against base system libraries only:

```console
$ ldd /usr/local/bin/tuwunel
	/usr/lib/libc++.so.12.0
	/usr/lib/libpthread.so.28.1
	/usr/lib/libc++abi.so.9.0
	/usr/lib/libc.so.103.0
	/usr/lib/libm.so.10.1
```

Nothing from `/usr/local/lib` is needed at run time, so the binary can be copied
to a host with no build toolchain. It still needs its login class limits there.

Cargo wedged twice partway through this build, sleeping with no `rustc` child
and nothing in the system logs, and had to be killed and restarted. It resumes
from cache, and the build that completed ran at `-j2`. If you see a build stop
making progress, check whether `rustc` is still running before assuming it is
merely slow.


## Running under rc.d

Create an account in the login class from above, install the binary, and give it
a database directory:

```sh
useradd -c "tuwunel" -d /var/tuwunel -s /sbin/nologin -L tuwunel _tuwunel
install -m 755 /home/build/target/release/tuwunel /usr/local/bin/tuwunel
mkdir -p /etc/tuwunel /var/tuwunel
chown _tuwunel:_tuwunel /var/tuwunel
```

Save this as `/etc/rc.d/tuwunel` and `chmod 555` it:

```sh
#!/bin/ksh

daemon="/usr/local/bin/tuwunel"
daemon_flags="-c /etc/tuwunel/tuwunel.toml"
daemon_user="_tuwunel"
daemon_class="tuwunel"

. /etc/rc.d/rc.subr

rc_bg=YES
rc_reload=NO

rc_cmd $1
```

```sh
rcctl enable tuwunel
rcctl start tuwunel
```

`rc_bg=YES` is required: tuwunel runs in the foreground and never forks, so
without it `rcctl start` would block. `daemon_class` is what carries the raised
limits to the server, and is the reason `_tuwunel` is created with `-L tuwunel`;
without it the daemon runs with 128 file descriptors and will not get far.

A minimal configuration to go with it:

```toml
[global]
server_name = "example.com"
address = "127.0.0.1"
port = 8008
database_path = "/var/tuwunel"
log_colors = false
```

Set `log_colors = false` whenever the log is not going to a terminal. Tuwunel
decides on colour from the configuration rather than from the sink, so the log
otherwise accumulates ANSI escapes.


## Differences from the shipped platforms

**No `io_uring`.** The feature is Linux only, so RocksDB uses the POSIX I/O
backend. This is the same code path every non-Linux build takes.

**No systemd integration.** Readiness and watchdog notification, socket
activation, and the reload handling described in
[Systemd Socket Activation](socket-activation.md) and
[Reloading Configuration](configuration-reload.md) are all Linux only. Use the
`rc.d` script above instead.

**No CPU feature levels.** The build targets the architecture baseline, and
nothing selects one of the `x86_64-v1` through `x86_64-v4` levels the release
packages come in. On `arm64` there is nothing to choose. On `amd64` the baseline
is the `v1` level, which costs RocksDB its hardware CRC32C; see
[RocksDB on amd64](#rocksdb-on-amd64) for the flag that raises it.
