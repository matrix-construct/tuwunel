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

### Online backups

Currently only RocksDB supports online backups. If you'd like to backup your
database online without any downtime, see the `!admin server` command for the
backup commands and the `database_backup_path` config options in the example
config.

Please note that the format of the database backup is not the exact same. This is
unfortunately a bad design choice by Facebook as we are using the database backup
engine API from RocksDB, however the data is still there and can still be joined
together.

#### Restoring online backup

To restore a backup from an online RocksDB backup:

- shutdown Tuwunel
- create a new directory for merging together the data
- in the online backup created, copy all `.sst` files in
`$DATABASE_BACKUP_PATH/shared_checksum` to your new directory
- trim all the strings so instead of `######_sxxxxxxxxx.sst`, it reads
`######.sst`. A way of doing this with sed and bash is `for file in *.sst; do mv
"$file" "$(echo "$file" | sed 's/_s.*/.sst/')"; done`
- copy all the files in `$DATABASE_BACKUP_PATH/1` (or the latest backup number
if you have multiple) to your new directory
- set your `database_path` config option to your new directory, or replace your
old one with the new one you crafted
- start up Tuwunel again and it should open as normal

### Offline backups

If you'd like to do an offline backup, shutdown Tuwunel and copy your
`database_path` directory elsewhere. This can be restored with no modifications
needed.

Backing up media is also just copying the `media/` directory from your database
directory.

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
