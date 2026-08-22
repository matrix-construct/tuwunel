# Maintaining your Tuwunel setup

## Moderation

Tuwunel has moderation through admin room commands. "binary commands" (medium
priority) and an admin API (low priority) is planned. Some moderation-related
config options are available in the example config such as "global ACLs" and
blocking media requests to certain servers. See the example config for the
moderation config options under the "Moderation / Privacy / Security" section.

Tuwunel has moderation admin commands for:

- managing room aliases (`!admin rooms alias`)
- managing room directory (`!admin rooms directory`)
- managing room banning/blocking and user removal (`!admin rooms moderation`)
- managing user accounts (`!admin users`)
- fetching `/.well-known/matrix/support` from servers (`!admin federation`)
- blocking incoming federation for certain rooms (not the same as room banning)
(`!admin federation`)
- deleting media (see [the media section](#media))

Any commands with `-list` in them will require a codeblock in the message with
each object being newline delimited. An example of doing this is:

````
!admin rooms moderation ban-list-of-rooms
```
!roomid1:server.name
#badroomalias1:server.name
!roomid2:server.name
!roomid3:server.name
#badroomalias2:server.name
```
````

## Database (RocksDB)

Generally there is very little you need to do. [Compaction][rocksdb-compaction]
is ran automatically based on various defined thresholds tuned for Tuwunel to
be high performance with the least I/O amplifcation or overhead. Manually
running compaction is not recommended, or compaction via a timer, due to
creating unnecessary I/O amplification. RocksDB is built with io_uring support
via liburing for improved read performance.

RocksDB troubleshooting can be found
[in the RocksDB section of troubleshooting](troubleshooting.md#rocksdb--database-issues).

### Compression

Some RocksDB settings can be adjusted such as the compression method chosen. See
the RocksDB section in the [example config](configuration/examples.md).

btrfs users have reported that database compression does not need to be disabled
on Tuwunel as the filesystem already does not attempt to compress. This can be
validated by using `filefrag -v` on a `.SST` file in your database, and ensure
the `physical_offset` matches (no filesystem compression). It is very important
to ensure no additional filesystem compression takes place as this can render
unbuffered Direct IO inoperable, significantly slowing down read and write
performance. See <https://btrfs.readthedocs.io/en/latest/Compression.html#compatibility>

> [!IMPORTANT]
> Compression is done using the COW mechanism so it’s incompatible with
> nodatacow. Direct IO read works on compressed files but will fall back to
> buffered writes and leads to no compression even if force compression is set.
> Currently nodatasum and compression don’t work together.

### btrfs

btrfs is Copy-on-Write, which interacts badly with the way RocksDB allocates
its write-ahead logs. Set `rocksdb_allow_fallocate = false` in `tuwunel.toml`.
Preallocation cannot reserve in-place write space on a CoW filesystem anyway,
so there is nothing to lose by turning it off there. Tuwunel warns at startup
when `database_path` is on btrfs and the option is still enabled.

RocksDB preallocates each WAL with `fallocate(2)`, writes to it, then truncates
it to the written length on close. btrfs does not split the preallocated extent
on truncation: for as long as the file references any part of it, the whole
extent stays allocated. A WAL holding a few kilobytes of records can pin tens
of megabytes of disk this way.

Obsolete WALs compound it. They are moved to `database_path/archive` and reaped
only once the archive passes 1 GB, which RocksDB measures from the length of
the files rather than the space they occupy. Truncated WALs therefore sit there
far longer than their real cost warrants, and they accumulate quickly on a
server taking live traffic.

The gap between those two numbers is what makes this hard to recognize. `du`
reports the small one; only `df` or `btrfs filesystem usage` shows the disk
filling. `compsize` reports both for a directory. Defragmenting does not give
the space back, so the files have to be rewritten or removed: setting the
option stops any further growth, and an offline copy of `database_path` (see
[Offline backups](backups.md#offline-backups)) reclaims what has already
accumulated.

### ZFS

ZFS has several quirks that interact badly with RocksDB defaults. Apply both
the Tuwunel config changes and the dataset properties below.

In `tuwunel.toml`:

- `rocksdb_direct_io = false`. OpenZFS prior to 2.3 silently ignored
  `O_DIRECT` and fell back to buffered. OpenZFS 2.3+ honors `O_DIRECT` only
  when requests are page-aligned and a multiple of the recordsize, which
  RocksDB cannot guarantee.
- `rocksdb_allow_fallocate = false`. OpenZFS does not implement
  `fallocate(2)` preallocation; only `FALLOC_FL_PUNCH_HOLE` and
  `FALLOC_FL_ZERO_RANGE` are supported. Tuwunel warns at startup while this
  is left enabled on ZFS.
- Leave `rocksdb_optimize_for_spinning_disks = false` on NVMe or SSD pools,
  even when running on ZFS.

On the dataset hosting `database_path`:

| Property | Value | Reason |
|---|---|---|
| `recordsize` | `128K` (or `64K`) | Match RocksDB's working set. `16K` causes severe write amplification on compaction. |
| `primarycache` | `metadata` | Tuwunel's block cache already serves data; ARC caching of data duplicates RAM. |
| `compression` | `off` | RocksDB SSTs are already zstd-compressed by Tuwunel. |
| `atime` | `off` | Avoid an FS write per read. |
| `logbias` | `throughput` | Route ZIL through the normal txg path, which suits append-only WAL traffic. |

`recordsize` takes effect only on files written after the property is
changed. After adjusting it, dump the database (offline copy out, wipe the
dataset, copy back) so existing SSTs adopt the new recordsize. Without a
dump-and-reload, compaction will gradually rewrite into the new recordsize
over weeks; pre-existing files keep the old size in the meantime.

For sync write latency, in order of preference: a separate SLOG vdev, then
`logbias=throughput`, then `sync=disabled` (only if you accept that a host
crash may discard the WAL tail; Tuwunel recovers cleanly from this via
`rocksdb_recovery_mode=1`, the default).

### Files in database

Do not touch any of the files in the database directory. This must be said due
to users being mislead by the `.log` files in the RocksDB directory, thinking
they're server logs or database logs, however they are critical RocksDB files
related to WAL tracking.

The only safe files that can be deleted are the `LOG` files (all caps). These
are the real RocksDB telemetry/log files, however Tuwunel has already
configured to only store up to 3 RocksDB `LOG` files due to generally being
useless for average users unless troubleshooting something low-level. If you
would like to store nearly none at all, see the `rocksdb_max_log_files`
config option.

### Backups

Database backup, checkpoint, and restore procedures are documented on the
[Backups](backups.md) page.

## Media

Media still needs various work, however Tuwunel implements media deletion via:

- MXC URI or Event ID (unencrypted and attempts to find the MXC URI in the
event)
- Delete list of MXC URIs
- Delete remote media in the past `N` seconds/minutes via filesystem metadata on
the file created time (`btime`) or file modified time (`mtime`)

See the `!admin media` command for further information. All media in Tuwunel
is stored at `$DATABASE_DIR/media`. This will be configurable soon.

If you are finding yourself needing extensive granular control over media, we
recommend looking into [Matrix Media
Repo](https://github.com/t2bot/matrix-media-repo). Tuwunel intends to
implement various utilities for media, but MMR is dedicated to extensive media
management.

Built-in S3 support is also planned, but for now using a "S3 filesystem" on
`media/` works. Tuwunel also sends a `Cache-Control` header of 1 year and
immutable for all media requests (download and thumbnail) to reduce unnecessary
media requests from browsers, reduce bandwidth usage, and reduce load.

[rocksdb-compaction]: https://github.com/facebook/rocksdb/wiki/Compaction
