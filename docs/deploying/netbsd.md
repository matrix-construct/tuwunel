# Tuwunel for NetBSD

Tuwunel builds and runs on NetBSD, but NetBSD is not a supported platform.
Nothing in the CI matrix builds it, no release artifact is published for it, and
there is no pkgsrc package. This page records a build that was verified end to
end, and the handful of things that differ from the platforms we do ship.

Verified against 1.8.3 on NetBSD 11.0, `evbarm-aarch64` and `amd64`. The
resulting server opens its database, answers the client and federation version
endpoints, registers an account, creates a room, sends and reads back a message,
reopens a populated database across a restart, and exits cleanly on `SIGTERM`.

Contributions for getting Tuwunel into pkgsrc are welcome.


## Toolchain

A stock NetBSD install has `pkg_add` but no `pkgin`, so bootstrap it first:

```sh
export PKG_PATH=https://cdn.NetBSD.org/pub/pkgsrc/packages/NetBSD/amd64/11.0/All/
pkg_add pkgin
echo "https://cdn.NetBSD.org/pub/pkgsrc/packages/NetBSD/amd64/11.0/All" \
    > /usr/pkg/etc/pkgin/repositories.conf
pkgin update
```

The architecture in those two URLs is the pkgsrc one, `amd64` here and
`aarch64` on `evbarm-aarch64`. Neither is what `uname -m` prints on both ports,
so substitute it by hand rather than deriving it.

Then install the build dependencies:

```sh
pkgin install rust git cmake clang gmake pkgconf curl
```

`rust` carries 1.96.0 on both branches, which is at or above the `rust-version`
the workspace declares. The `clang` package supplies `/usr/pkg/lib/libclang.so`,
which `rust-librocksdb-sys` runs bindgen against; point the build at it with
`LIBCLANG_PATH`. NetBSD's base compiler is GCC and does not provide it.

Everything under `/usr/pkg` needs to be on `PATH`, which is not the case for a
non-interactive `ssh` command:

```sh
export PATH=/usr/pkg/bin:/usr/pkg/sbin:$PATH
```

This is the set that was verified rather than a minimal one. `curl` is only used
by the checks further down this page.


## Features

The default feature set includes `io_uring` and `systemd`, both of which are
Linux only. Every NetBSD build therefore has to opt out of the defaults and name
its features explicitly:

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


## Building

```sh
export PATH=/usr/pkg/bin:/usr/pkg/sbin:$PATH
export LIBCLANG_PATH=/usr/pkg/lib

cargo build --release -p tuwunel --no-default-features \
    --features brotli_compression,element_hacks,gzip_compression,jemalloc,jemalloc_conf,media_thumbnail,release_max_log_level,url_preview,zstd_compression
```

No source changes or compiler flag workarounds are needed. The pinned RocksDB
code probes NetBSD's exported CPU ID at run time and uses the arm64 CRC32C and
PMULL paths when the CPU reports those extensions.

On `amd64` the build is equally clean, but it is worth raising the target CPU
before running it in anger; see [RocksDB on amd64](#rocksdb-on-amd64) below.

A cold build took about 66 minutes on 4 jobs on `evbarm-aarch64`, and about 31
minutes on 4 jobs on `amd64`, producing a 79 MB and a 94 MB binary respectively.
The result is dynamically linked, and against base system libraries only:

```console
$ ldd target/release/tuwunel
	-lstdc++.9 => /usr/lib/libstdc++.so.9
	-lm.0 => /usr/lib/libm.so.0
	-lgcc_s.1 => /usr/lib/libgcc_s.so.1
	-lc.12 => /usr/lib/libc.so.12
	-lpthread.1 => /usr/lib/libpthread.so.1
```

Nothing under `/usr/pkg` is needed at run time, so the binary can be copied to a
host that has no pkgsrc toolchain installed.


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
`crc32c_3way`, and the `westmere` one carries `crc32c_3way` and no tables. The
tuned build finished a minute and a half faster than the baseline and came out
marginally smaller, so the flag costs nothing.

This is the axis the `x86_64-v1` through `x86_64-v4` release packages sit on. A
build with no `-Ctarget-cpu` is the `v1` one, and nothing selects a higher level
for you here.

CRC32C ends up in software by default on `amd64` because of the compiler target,
and the flag above fixes it. On `evbarm-aarch64`, RocksDB instead probes
NetBSD's exported CPU ID at run time and uses the hardware CRC32C and PMULL paths
when available.


## Running

Configuration is no different from any other platform; see
[Configuration](../configuration.md). A minimal file to prove the build:

```toml
[global]
server_name = "example.com"
address = "0.0.0.0"
port = 8008
database_path = "/var/db/tuwunel"
```

```sh
tuwunel -c /usr/pkg/etc/tuwunel/tuwunel.toml
```


## Running under rc.d

Create an account for the service, install the binary, and give it a database
directory:

```sh
groupadd tuwunel
useradd -g tuwunel -d /nonexistent -s /sbin/nologin tuwunel
install -m 755 target/release/tuwunel /usr/pkg/bin/tuwunel
mkdir -p /usr/pkg/etc/tuwunel /var/db/tuwunel
chown tuwunel:tuwunel /var/db/tuwunel
install -o tuwunel -g tuwunel -m 640 /dev/null /var/log/tuwunel.log
```

That last line matters. NetBSD has no `daemon(8)`, so the script below
backgrounds the server itself and redirects its output, and `rc.subr` applies
the redirect *after* it has dropped to `tuwunel_user`. Without a log file the
account already owns, the start fails with `sh: cannot create
/var/log/tuwunel.log: permission denied`.

Save this as `/etc/rc.d/tuwunel` and `chmod 555` it:

```sh
#!/bin/sh
#
# PROVIDE: tuwunel
# REQUIRE: DAEMON NETWORKING
# KEYWORD: shutdown

$_rc_subr_loaded . /etc/rc.subr

name="tuwunel"
rcvar=$name
command="/usr/pkg/bin/tuwunel"
command_args="-c /usr/pkg/etc/tuwunel/tuwunel.toml >> /var/log/tuwunel.log 2>&1 &"
tuwunel_user="tuwunel"

load_rc_config $name
run_rc_command "$1"
```

```sh
echo "tuwunel=YES" >> /etc/rc.conf
/etc/rc.d/tuwunel start
```

Set `log_colors = false` in the configuration when logging to a file this way.
Tuwunel decides on colour from the configuration rather than from whether the
sink is a terminal, so the log otherwise accumulates ANSI escapes.


## Differences from the shipped platforms

**CRC32C and PMULL are detected at run time on arm64.** RocksDB reads
`machdep.cpu0.cpu_id` through `sysctlbyname` and inspects the exported
`ID_AA64ISAR0_EL1` value. It selects hardware CRC32C when CPU 0 reports the
CRC32 extension, and enables the PMULL path when CPU 0 also reports PMULL. If
the CRC32 probe fails or the extension is unavailable, checksums use the
software fallback. A failed PMULL probe disables only that path. Table blocks
use XXH3 and are unaffected.

**No `io_uring`.** The feature is Linux only, so RocksDB uses the POSIX I/O
backend. This is the same code path every non-Linux build takes.

**No systemd integration.** Readiness and watchdog notification, socket
activation, and the reload handling described in
[Systemd Socket Activation](socket-activation.md) and
[Reloading Configuration](configuration-reload.md) are all Linux only. Run the
server under `rc.d` or a supervisor of your choice instead.

**No CPU feature levels.** The build targets the architecture baseline, and
nothing selects one of the `x86_64-v1` through `x86_64-v4` levels the release
packages come in. On `evbarm-aarch64` there is nothing to choose. On `amd64` the
baseline is the `v1` level, which costs RocksDB its hardware CRC32C; see
[RocksDB on amd64](#rocksdb-on-amd64) for the flag that raises it.
