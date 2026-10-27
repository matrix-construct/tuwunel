# Configuration

This chapter describes various ways to configure Tuwunel.

## Basics

Tuwunel uses a config file for the majority of the settings, but also supports
setting individual config options via commandline.

Please refer to the [example config file](./configuration/examples.md#example-configuration) for all of those settings.

The config file to use can be specified on the commandline when running
Tuwunel by specifying the `-c`, `--config` flag. It is also possible to specify
more than one config file.

Alternatively, you can use the environment variable `TUWUNEL_CONFIG` to specify
the config file to used. Conduit's environment variables are supported for 
backwards compatibility.

> [!IMPORTANT]
> It is bad practice to uncomment default options without changing them. Many
> defaults are updated by developers as features evolve and can be essential
> to expected server function.

## Configuration tables

Settings are grouped by subsystem beneath `[global]`. Bootstrap settings such as `server_name` and `database_path` remain at the root:

```toml
[global]
server_name = "example.com"
database_path = "/var/lib/tuwunel"

[global.http]
max_request_size = 20000000

[global.rocksdb]
checksums = true
```

Existing flat names, including `max_request_size` and `rocksdb_checksums`, remain supported throughout 2.x. Existing configurations do not need to be rewritten. [Configuration regeneration](./configuration/regeneration.md) can produce a canonical file when you want to reorganize it.

Existing headerless files remain supported. Use explicit `[global.*]` headers for new files containing only tables: without any non-table value at the root, table names are interpreted as profiles rather than inferred configuration groups. Do not mix bare root settings with an explicit `[global]` profile in one file.

New group names, such as `http`, `rocksdb` and `logging`, are reserved for tables. An old unknown scalar under one of these names cannot remain a scalar in the new hierarchy.

Unknown options warn by default, including misspelled settings inside the new groups; `configuration.error_on_unknown_opts = true` makes them errors. Supported legacy names remain accepted in strict mode and are not newly deprecated. Existing nested sections and dynamic maps keep their existing unknown-key behavior.

## Option commandline flag

Tuwunel supports setting individual config options in TOML format from the
`-O` / `--option` flag. For example, you can set your server name via `-O
server_name=\"example.com\"`.

One setting is refused here, as it is from configuration files and the
environment: `database_restore_backup` belongs to `--restore-backup` alone,
since restoring a backup is destructive and must not repeat.

Option paths are relative to `global`: use `rocksdb.checksums`, not `global.rocksdb.checksums`. The option value is TOML, so quote string values for both TOML and your shell:

- `-O registration.enable=true`
- `-O http.max_request_size=99999999`
- `-O 'logging.filter="debug"'`
- `-O 'server_name="example.com"'`

## Relevance of configuration settings

Settings are read in this order, with later explicit values taking precedence:

1. The file selected by `CONDUIT_CONFIG`.
2. The file selected by `CONDUWUIT_CONFIG`.
3. The file selected by `TUWUNEL_CONFIG`.
4. Files selected with `-c`, in command-line order.
5. Settings supplied through `CONDUIT_` environment variables.
6. Settings supplied through `CONDUWUIT_` environment variables.
7. Settings supplied through `TUWUNEL_` environment variables.
8. Individual `-O` assignments, in command-line order.

This order applies regardless of spelling. For example, `rocksdb.checksums` in a later file overrides `rocksdb_checksums` in an earlier file, and the reverse works too. A later assignment leaves unrelated settings from earlier sources intact. Ordinary array overrides replace the array.

For settings moved into the hierarchy, two spellings of the same setting in one file or one environment namespace are an error, even when their values agree. This includes supported aliases. Use only one spelling per source. Each `-O` occurrence is a separate source, so repeated assignments use the last occurrence, including a mixture of old and canonical spellings.

## Environment variables

Use uppercase names with a supported prefix and double underscores between hierarchy components. Single underscores remain part of a setting's name. For example, `http.max_request_size` becomes `TUWUNEL_HTTP__MAX_REQUEST_SIZE`, and `well_known.server` becomes `TUWUNEL_WELL_KNOWN__SERVER`.

Both `TUWUNEL_ROCKSDB__CHECKSUMS` and the old `TUWUNEL_ROCKSDB_CHECKSUMS` spelling are supported, but setting both in the same environment namespace is an error. The `CONDUIT_` and `CONDUWUIT_` prefixes support the same old and canonical paths. The config-file selectors `CONDUIT_CONFIG`, `CONDUWUIT_CONFIG` and `TUWUNEL_CONFIG` keep their existing names.

The restore control described above remains restricted to its dedicated command-line flag.

## Execute commandline flag

Tuwunel supports running admin commands on startup using the commandline
argument `--execute`. The most notable use for this is to create an admin user
on first startup.

The syntax of this is a standard admin command without the prefix such as
`./tuwunel --execute "users create_user june"`

An example output of a success is:
```
INFO tuwunel_service::admin::startup: Startup command #0 completed:
Created user with user_id: @june:girlboss.ceo and password: `<redacted>`
```

This commandline argument can be paired with the `--option` flag.
