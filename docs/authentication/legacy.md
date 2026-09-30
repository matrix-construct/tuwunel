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

## Email verification

Tuwunel can verify that a user controls an email address by emailing a
single-use confirmation link through your own SMTP relay. No external identity
server is involved; each verified address binds locally to one account. Email is
the only supported medium (phone-number verification is not supported). Verified
email is used for adding an address to an account, resetting a forgotten
password, and optionally [requiring email at
registration](#requiring-email-at-registration).

Point Tuwunel at an SMTP relay and set the sender mailbox. The confirmation link
is built from `well_known.client`, so that must also be set and reachable by
your users in a browser:

```toml
[global.smtp]
connection_uri = "smtps://noreply%40example.com:password@mail.example.com:465"
sender = "Example <noreply@example.com>"

[global.well_known]
client = "https://matrix.example.com"
```

| Option | Default | Description |
|---|---|---|
| `connection_uri` | (unset) | Connection URL for the outbound SMTP relay. Setting it enables the email subsystem; without it, no mail is sent. Use `smtp://` for an unencrypted or STARTTLS connection and `smtps://` for implicit TLS. Credentials and host go inline (`smtps://user:pass@host:port`). The userinfo is URL-encoded, so an `@` in the username must be written as `%40`. |
| `sender` | (unset) | The mailbox verification messages are sent from. Accepts a bare address or a display-name form (`Example <noreply@example.com>`). Required whenever `connection_uri` is set. |

The subsystem is disabled by default and stays inert until `connection_uri` is
set. If a client asks to verify an email while it is disabled, the request is
rejected with `M_THREEPID_DENIED`.

## Requiring email at registration

These options gate registration on a verified email address. Both default to
off, and both require the email verification subsystem above to be configured.

| Option | Default | Description |
|---|---|---|
| `require_email_for_registration` | `false` | Require a verified email address to complete registration. When set, the registration flow does not finish until the user proves control of an email address. |
| `require_email_for_token_registration` | `false` | Require a verified email address when registering with a [registration token](#token-based-registration). When set, token-based registration also demands a verified email address. |

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

## Login rate limits

These bound password guessing. Both are keyed on the account being tried
rather than on the client's address, so they hold behind a reverse proxy or
tunnel that does not forward the caller's address, where a per-address limit
would see one client for every user. An account that is out of tokens is
refused with `M_LIMIT_EXCEEDED` and a `retry_after_ms`.

The defaults are Synapse's `rc_login.account` and `rc_login.failed_attempts`.
Setting either `burst_count` — or either `per_second` — to `0` disables that
limit.

| Option | Default | Description |
|---|---|---|
| `login_rc_account_per_second` | `0.003` | Refill rate for password attempts against one account, successful or not. |
| `login_rc_account_burst_count` | `5` | Attempts allowed before that rate governs. |
| `login_rc_failed_per_second` | `0.17` | Refill rate for wrong passwords against one account. A drained bucket refuses the next attempt even with the correct password. |
| `login_rc_failed_burst_count` | `3` | Wrong passwords allowed before that rate governs. |

**What this costs.** While someone keeps guessing an account's password, its
owner cannot sign in with that password either — the refusal is what bounds the
guessing. Existing sessions are untouched, and so is any login that is not a
password: `m.login.token`, SSO and JWT.

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
