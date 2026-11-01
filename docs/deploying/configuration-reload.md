# Reloading configuration

Tuwunel re-reads its configuration on `SIGUSR1` without restarting or dropping
connections. The packaged service units wire `systemctl reload` to that signal:

```bash
systemctl reload tuwunel
```

Sending the signal by hand does the same thing, and is the route to use in a
container or when running the binary directly:

```bash
kill -USR1 "$(pidof tuwunel)"
```

A reload rebuilds the configuration from the same sources the server started
with: the files named by `TUWUNEL_CONFIG` and by `--config`, the
`TUWUNEL_`-prefixed environment variables merged over them, and finally the
`-O` overrides from the command line. The result is swapped in atomically.

Since the command line is replayed rather than remembered, an option pinned
with `-O` stays pinned across a reload, and editing that key in the file has no
effect until the server is restarted without the override.

Configuration that fails to parse or fails validation is rejected and the
running configuration is kept, with the reason logged:

```
ERROR Failed to reload config: ...
```

Set `config_reload_signal = false` to make the server ignore `SIGUSR1`.

## How each package delivers the reload

The Arch unit uses `Type=notify-reload`, so `systemctl reload` waits for the
server to finish reloading before returning. That needs systemd 253.

The Debian and Red Hat units use `ExecReload=` instead, which works on every
systemd version but returns as soon as the signal is delivered. Debian 12 and
Enterprise Linux 9 both ship systemd 252, where an unrecognised `Type=` value
is ignored and the service silently falls back to `Type=simple`, so those two
units cannot use the newer mechanism.

Either way the reload reports success to systemd whether or not the new
configuration was accepted, because the server carries on serving the old one
and is therefore still healthy. The outcome travels in the unit's status line
instead of the exit code:

```console
$ systemctl status tuwunel
     Status: "Configuration rejected: There was a problem with your configuration file: ..."
```

A successful reload replaces it with `Configuration reloaded`, and the same
reason is logged either way. The line describes the last reload attempt only;
a freshly started server reads `Running`.

## What a reload applies

Options documented `reloadable: yes` in `tuwunel-example.toml` take effect
immediately. Three are rejected outright, failing the whole reload:

- `server_name`
- `ip_source`
- any `[global.storage_provider.<ID>]` section, whether added, removed or
  changed

Two more are refused outright rather than merely ignored, because the server
would refuse to start with them set in a file: `maintenance` and
`database_restore_backup`. Either one fails the whole reload, not just its own
key.

Anything else is accepted, but only takes effect where the server reads it.
Listening sockets are the case worth knowing: `address`, `port`,
`unix_socket_path` and the `[global.tls]` settings are consumed once during
startup, so changing them needs a restart. None of them are marked reloadable.

## Secrets kept in their own files

Several secrets can live in a file of their own rather than inline in the
configuration:

| Option | Contents |
|---|---|
| `registration_token_file` | registration tokens, separated by whitespace |
| `registration_shared_secret_file` | shared secret for `/_synapse/admin/v1/register` |
| `turn_secret_file` | TURN HMAC secret |
| `ldap.bind_password_file` | LDAP bind password |
| `identity_provider.<id>.client_secret_file` | OIDC client secret |

Each of these is read at the point it is used, so replacing the contents of the
file takes effect on the next request that needs it. No reload and no restart
is required. A reload is only needed to change *which* path an option names.

## Systemd credentials

Secrets can be handed to the service by systemd rather than left readable on
disk. Add a drop-in with `systemctl edit tuwunel.service` and point one of the
`_file` options above at the credentials directory, which the `%d` specifier
expands to:

```ini
[Service]
LoadCredential=turn_secret:/etc/tuwunel/turn_secret
Environment=TUWUNEL_TURN_SECRET_FILE=%d/turn_secret
```

Systemd copies the credential into a private tmpfs readable only by the
service, so the source file can stay owned by root and unreadable by the
`tuwunel` user. `LoadCredentialEncrypted=` and `ImportCredential=` work the
same way, as does setting the path in the configuration file directly instead
of through the environment.

Credentials are re-materialised each time the service starts, so
`systemctl restart tuwunel` always picks up a changed source file. Having
`systemctl reload` refresh them as well takes `RefreshOnReload=`, which systemd
applies before signalling the server, so the configuration and the secret files
are both current by the time the reload runs.

The Arch unit sets it already. It needs systemd 260, which rules it out of the
Debian and Red Hat units, since Debian 12 and 13 ship 252 and 257 and
Enterprise Linux 9 and 10 ship 252 and 257. On a host new enough to have it,
add it in the same drop-in:

```ini
[Service]
RefreshOnReload=yes
```

Older versions log `Unknown key name 'RefreshOnReload'` and carry on, leaving
restart as the way to pick up a rotated credential.

Prefer `yes` over the narrower `RefreshOnReload=credentials`. The latter warns
on every load when the unit has no credentials configured, and masks the
credentials tree in that case.
