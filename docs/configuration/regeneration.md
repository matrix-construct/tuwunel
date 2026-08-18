# Configuration regeneration

Tuwunel can rebuild an existing configuration with the schema and documentation
compiled into the current binary. Explicitly configured values remain active,
aliases are rewritten to their canonical names, and fields return to the order
used by the current example configuration. Unset fields remain commented so
that future default changes still apply. Dynamic sections such as identity
providers, appservices, and storage providers are rendered from their current
instances.

Regeneration only writes a configuration document. The standalone command exits
before normal server startup and does not open the database. The admin command
does not reload the generated file.

## Commands

| Goal | Command | Default destination |
| --- | --- | --- |
| Generate a pristine example | `tuwunel --generate-config` | Standard output. |
| Regenerate selected configuration files | `tuwunel --regenerate-config` | `<input>.new` beside a single input file. |
| Regenerate from a running server | `!admin server regenerate-config <path>` | The required absolute path on the server. |

An optional CLI destination must be attached to the command with `=`:

```console
tuwunel --generate-config=/etc/tuwunel/tuwunel-example.toml
tuwunel -c /etc/tuwunel/tuwunel.toml \
  --regenerate-config=/etc/tuwunel/tuwunel.toml.new
```

Regeneration requires at least one existing input file. Select files with `-c` or
`--config`, or through one of the supported configuration path environment
variables. Inputs use the same [source order](../configuration.md#relevance-of-configuration-settings)
as normal startup. Multiple input files are collapsed into one document and
therefore require an explicit destination.

## Controls

| Control | Behavior |
| --- | --- |
| `--force` | Replace an existing destination after preserving it as `<destination>.bak`. |
| `--include-env` | Materialize configuration values supplied through the environment. |
| `--strip-unknown` | Comment out deprecated and unknown values instead of leaving them active. |

The admin command accepts the same controls after its destination path. The
environment and unknown-key controls apply only to regeneration, not pristine
example generation.

Without `--include-env`, file-backed values are retained and environment-backed
values are identified by comments naming their environment variables. This
avoids making a temporary environment override persistent. Command-line
overrides such as `-O` are never copied into the generated file.

Using `--include-env` can copy secrets and temporary overrides from the process
environment into the file. Those values remain configured if the environment
variables are later removed.

Deprecated and unknown keys remain active by default, with an explanatory
comment. This preserves the behavior and startup warnings of the input. Valid
but undocumented keys also remain active. The startup-only
`database_restore_backup` and `force_migration` controls are never emitted and
are named in the command summary when removed.

## Output safety

> [!IMPORTANT]
> The generated document contains configuration secrets. Store and review it
> with the same protections as the active configuration.

Tuwunel applies the following safeguards:

- A new destination is created with mode `0600` on Unix.
- An existing destination is never replaced unless `--force` is supplied.
- Forced replacement preserves the destination's mode and ownership, and saves
  its previous contents to an adjacent `.bak` file. Replacement is refused if
  that backup already exists.
- On macOS, forced replacement also carries the destination's ACLs and extended
  attributes onto the replacement, and onto its backup on a best-effort basis.
- Nonregular destinations, including symbolic links, are refused.
- Forced replacement is supported on Linux and macOS. Other platforms can write
  a new destination and replace it manually after review.
- Writes use a temporary file in the destination directory and are installed
  atomically.
- The output is parsed and compared with the selected input values before and
  after installation.

The admin command returns only the destination and a summary. It does not send
the configuration or its secrets to the admin room.

The destination for an admin command must be writable by the Tuwunel service
account.

## Limitations

- Handwritten comments are not carried into the generated document. The default
  `.new` output leaves the original available, while forced replacement retains
  it as `.bak`.
- Lexical formatting can change even when the TOML values are equivalent.
- Headerless input is normalized into `[global]`. Named profiles other than
  `default` and `global` are refused.
- Multiple input files lose their layer boundaries in the combined output.
- Regeneration uses the schema of the binary that runs the command.
- The generated file is not reloaded or adopted automatically.

## Review and adopt the result

The safest workflow leaves the active file untouched:

```console
tuwunel -c /etc/tuwunel/tuwunel.toml --regenerate-config
diff -u /etc/tuwunel/tuwunel.toml /etc/tuwunel/tuwunel.toml.new
```

Keep the original file available while reviewing any operational notes it
contains.

On Linux and macOS, an explicit forced replacement can install the reviewed form
while retaining the previous file:

```console
tuwunel -c /etc/tuwunel/tuwunel.toml \
  --regenerate-config=/etc/tuwunel/tuwunel.toml --force
```

This creates `/etc/tuwunel/tuwunel.toml.bak`. Remove or archive an existing
backup before repeating a forced replacement. Restart Tuwunel or use
[configuration reload](../deploying/configuration-reload.md) only after
reviewing the generated file.

For a running server, write to a new absolute path first:

```text
!admin server regenerate-config /etc/tuwunel/tuwunel.toml.new
```

Review that server-local file before replacing or reloading the active
configuration.
