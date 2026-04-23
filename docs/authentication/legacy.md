# Legacy Authentication

By default, registration is disabled. You must explicitly enable it and choose
what conditions, if any, a prospective user must meet before an account is
created.

## Enabling registration

Set `allow_registration = true` to enable registration. On its own this is not
enough — you must also configure at least one of the following:

- A registration token (recommended)
- The open-registration confirmation flag (not recommended)
- One or more [identity providers](providers.md)

### Token-based registration

A registration token acts as a shared secret that prospective users must
supply when creating an account. This is the recommended approach for private
or invite-only servers.

**Static token** — set a single token directly in the config:

```toml
allow_registration = true
registration_token = "o&^uCtes4HPf0Vu@F20jQeeWE7"
```

**File-based tokens** — read tokens from a file, one per line or separated by
whitespace. Useful for rotating tokens without restarting the server:

```toml
allow_registration = true
registration_token_file = "/etc/tuwunel/.reg_tokens"
```

Both options can be set at the same time; the file takes priority.

**Admin-issued tokens** — generate short-lived or single-use tokens from the
admin room without touching the config file:

| Command | Description |
|---|---|
| `!admin token issue` | Issue a token with no restrictions. |
| `!admin token issue --once` | Issue a single-use token (shorthand for `--max-uses 1`). |
| `!admin token issue --max-uses <N>` | Issue a token that expires after N uses. |
| `!admin token issue --max-age <duration>` | Issue a token that expires after a duration (e.g. `30m`, `7d`). |
| `!admin token revoke <token>` | Revoke a token immediately. |
| `!admin token list` | List all active tokens. |

### Open registration

To allow anyone to register without a token, you must set an additional
confirmation flag that acknowledges the abuse risk:

```toml
allow_registration = true
yes_i_am_very_very_sure_i_want_an_open_registration_server_prone_to_abuse = true
```

This is not recommended for public-facing servers. Consider token-based
registration or SSO providers instead.

## Guest registration

Guest accounts are anonymous sessions that some clients (e.g. Element) create
automatically before a user logs in. Guest registration is separate from
normal registration and is disabled by default.

| Option | Default | Description |
|---|---|---|
| `allow_guest_registration` | `false` | Allow guest account creation. |
| `log_guest_registrations` | `false` | Log each guest registration to the admin room. May be noisy on public servers. |
| `allow_guests_auto_join_rooms` | `false` | Allow guest users to auto-join rooms listed in `auto_join_rooms`. |

## Login options

These options control which login methods are accepted regardless of how
accounts were created.

| Option | Default | Description |
|---|---|---|
| `login_with_password` | `true` | Accept username and password login. Set to `false` to enforce SSO-only login. |
| `login_via_token` | `true` | Accept `m.login.token` login tokens. Disabling this can break SSO flows where the server issues a token to complete the login. |
| `login_via_existing_session` | `true` | Allow an authenticated session to mint a login token that a second client can use to log in. Requires interactive re-authentication. Disable if you want to prevent clients from spawning additional sessions this way. |

## Token and session lifetimes

| Option | Default | Description |
|---|---|---|
| `login_token_ttl` | `120000` | Lifetime of `m.login.token` tokens in milliseconds (default: 2 minutes). |
| `access_token_ttl` | `604800` | Lifetime of access tokens in seconds for clients that support refresh tokens. After expiry the client is soft-logged-out until it refreshes (default: 7 days). |
| `openid_token_ttl` | `3600` | Lifetime of OpenID 1.0 tokens in seconds. These are used for Matrix account integrations such as Vector Integrations in Element, **not** for OIDC/OpenID Connect logins (default: 1 hour). |

## Emergency password

The emergency password lets you log in to the server bot account
(`@conduit:<server_name>`) when normal access is unavailable — for example,
if you have lost access to your admin room.

```toml
emergency_password = "F670$2CP@Hw8mG7RY1$%!#Ic7YA"
```

Remove this option and restart the server once you have regained access — all
sessions for the bot account are logged out when it is unset. See the
[troubleshooting guide](../troubleshooting.md) for other recovery methods.
