# Application Services

Application services are out-of-process programs that extend the homeserver; most bridges (IRC, XMPP, Discord, Telegram, Slack) are implemented this way. They authenticate with a pair of shared secret tokens and declare namespaces of users and room aliases that they manage.

The upstream reference is the Matrix [Application Service API specification](https://spec.matrix.org/latest/application-service-api/).

## Registration

Tuwunel supports three registration methods. They coexist and are all loaded on startup.

### Admin room

The most convenient method for live deployments. Send `!admin appservices register` to the admin room with the registration YAML in a code block below:

```
!admin appservices register
\`\`\`
id: my-bridge
url: http://localhost:29328
as_token: secret-token-a
hs_token: secret-token-b
sender_localpart: my-bridge-bot
namespaces:
  users:
    - exclusive: true
      regex: '@my-bridge_.*:example\.com'
\`\`\`
```

The registration is persisted in the database and survives restarts. Registering with an existing ID replaces the old entry. No restart is required.

### Server configuration

Application services can be declared inline in `tuwunel.toml`. The TOML section key becomes the registration ID unless `id` is explicitly set:

```toml
[global.appservice.my-bridge]
url = "http://localhost:29328"
as_token = "secret-token-a"
hs_token = "secret-token-b"
sender_localpart = "my-bridge-bot"

[[global.appservice.my-bridge.users]]
exclusive = true
regex = '@my-bridge_.*:example\.com'
```

Config-file registrations are reloaded on each restart. They cannot be removed with `!admin appservices unregister`.

### Registration file directory

Point `appservice_dir` at a directory containing YAML registration files. Every readable file is loaded at startup:

```toml
[global]
appservice_dir = "/etc/tuwunel/appservices"
```

Files use the same YAML format that bridges produce for Synapse. New or removed files are not picked up without a restart.

## Namespaces

Each registration declares which users, room aliases, and room IDs the appservice claims. Entries under `users`, `aliases`, and `rooms` are lists; each entry has a `regex` and an optional `exclusive` flag.

| Namespace | Matches | Case |
|---|---|---|
| `users` | User IDs (`@localpart:server`) | Case-insensitive |
| `aliases` | Room aliases (`#alias:server`) | Case-insensitive |
| `rooms` | Room IDs (`!opaque:server`) | Case-sensitive |

Regex matching is unanchored -- add `^` and `$` if you need to match the full string.

When `exclusive: true`, the homeserver rejects any attempt by a normal user to register a conflicting user ID or room alias. Multiple appservices can share a non-exclusive namespace; exclusive ranges must not overlap.

## Configuration reference

Fields under `[global.appservice.<ID>]`:

| Field | Default | Description |
|---|---|---|
| `id` | section key | Unique registration ID. Inferred from the TOML section key when omitted. |
| `url` | -- | Base URL the homeserver pushes events to. Set to `null` for receive-only registrations. |
| `as_token` | -- | Token the appservice sends to the homeserver in `Authorization` headers. |
| `hs_token` | -- | Token the homeserver sends to the appservice on every push. |
| `sender_localpart` | `id` | Localpart of the appservice bot user. Defaults to the registration ID. |
| `rate_limited` | `false` | Whether requests from masqueraded virtual users are rate-limited. The bot user is always exempt. |
| `protocols` | `[]` | Protocols bridged (e.g. `["irc"]`). Reported via the `/thirdparty/protocols` endpoint. |
| `receive_ephemeral` | `false` | Include ephemeral events (typing notifications, read receipts) in pushes to the appservice. |
| `device_management` | `false` | Allow the appservice to manage devices on behalf of virtual users ([MSC4190](https://github.com/matrix-org/matrix-spec-proposals/pull/4190)). |

Namespace entries under `[[global.appservice.<ID>.users]]`, `aliases`, and `rooms`:

| Field | Default | Description |
|---|---|---|
| `exclusive` | `false` | Claim exclusive ownership of matching IDs, blocking regular users from registering them. |
| `regex` | -- | Regular expression. Unanchored by default. |

## Admin commands

All commands run from the admin room (`!admin appservices <subcommand>`):

| Command | Description |
|---|---|
| `register` | Register or replace an appservice. Paste the YAML in a code block below the command. |
| `unregister <id>` | Remove a database-registered appservice and cancel pending deliveries. Config-file registrations cannot be unregistered this way. |
| `show-config <id>` | Print the stored registration as YAML. |
| `list` | List IDs of all loaded appservices. |

## Connection settings

These options go in the top-level `[global]` section:

| Option | Default | Description |
|---|---|---|
| `appservice_timeout` | `35` | Request timeout in seconds when pushing events to an appservice. |
| `appservice_idle_timeout` | `300` | Idle connection pool timeout in seconds. |
| `dns_passthru_appservices` | `false` | Bypass DNS passthru domain matching for all appservice URLs. More efficient than listing each domain in `dns_passthru_domains` when all appservices share the same network. |

## Getting help

For setup questions, join [#tuwunel:matrix.org](https://matrix.to/#/#tuwunel:matrix.org) or [open an issue](https://github.com/matrix-construct/tuwunel/issues/new).
