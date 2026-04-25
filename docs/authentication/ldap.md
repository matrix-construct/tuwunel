# LDAP Authentication

Tuwunel can authenticate password logins against an LDAP directory. When a
user logs in with `m.login.password`, Tuwunel locates them in the directory,
verifies the password by binding as that user, and creates a Matrix account on
first successful login if one does not yet exist.

LDAP support is a **compile-time feature**. It is not part of the default
build — you must compile with `--features ldap` (or use a release artifact
that already includes it) before any of the configuration below has effect.

## Enabling LDAP

LDAP is configured under a single `[global.ldap]` section. The minimum to
enable it is the `enable` flag and a `uri`:

```toml
[global.ldap]
enable = true
uri = "ldaps://ldap.example.org:636"
base_dn = "ou=users,dc=example,dc=org"
```

The URI scheme decides the transport: `ldap://` is plaintext, `ldaps://`
upgrades to TLS using the system's installed CAs.

When `enable = true`, every `m.login.password` request is routed through LDAP
first. Local password authentication still happens as a fallback when the
LDAP search returns zero matches — useful for the bootstrap admin account or
for service users that should not exist in the directory. If the LDAP search
returns two or more matches the login is rejected.

## Bind modes

Tuwunel supports three bind strategies, selected by what you put in `bind_dn`
and `bind_password_file`.

### Search-then-bind (recommended)

A service account binds to the directory, searches for the user, then Tuwunel
re-binds as the user with the supplied password to verify it. This mode is
required if you want admin synchronization (see below).

```toml
[global.ldap]
enable = true
uri = "ldaps://ldap.example.org:636"
base_dn = "ou=users,dc=example,dc=org"
bind_dn = "cn=ldap-reader,dc=example,dc=org"
bind_password_file = "/etc/tuwunel/.ldap_bind_password"
filter = "(&(objectClass=person)(memberOf=cn=matrix,ou=groups,dc=example,dc=org))"
```

The bind password is read from `bind_password_file` rather than placed inline
in the config. The file must be readable by the Tuwunel process and must not
be empty.

### Anonymous search-then-bind

If your directory permits anonymous searches, omit both `bind_dn` and
`bind_password_file`. Tuwunel skips the initial bind and queries the
directory unauthenticated, then re-binds as the user to verify the password.

### Direct bind

If `bind_dn` contains the literal substring `{username}`, Tuwunel skips the
search entirely and binds directly with that DN, substituting the user's
localpart for `{username}` and using the supplied login password as the bind
password:

```toml
[global.ldap]
enable = true
uri = "ldaps://ldap.example.org:636"
bind_dn = "cn={username},ou=users,dc=example,dc=org"
```

This is the simplest mode but has two limitations: it cannot apply a search
filter (so anyone in the bind DN's subtree can log in), and **admin
synchronization does not work** because Tuwunel never gets a chance to query
the directory under a service account.

## Configuration reference

| Field | Default | Description |
|---|---|---|
| `enable` | `false` | Master switch for LDAP login. Has no effect unless the binary was compiled with `--features ldap`. |
| `uri` | — | LDAP server URI. `ldap://host:389` for plaintext, `ldaps://host:636` for TLS. |
| `base_dn` | `""` | Subtree under which user searches are rooted. |
| `bind_dn` | — | DN used for the initial bind. Contains `{username}` for direct-bind mode; otherwise identifies a service account. Omit for anonymous search. |
| `bind_password_file` | — | Path to a file containing the password for `bind_dn`. Ignored in direct-bind mode (the user's login password is used). |
| `filter` | `"(objectClass=*)"` | LDAP search filter applied during user lookup. Supports `{username}` substitution. |
| `uid_attribute` | `"uid"` | Attribute that uniquely identifies the user. Returned entries must contain the user's localpart in this attribute (or in `name_attribute`). |
| `name_attribute` | `"givenName"` | Secondary attribute checked for the localpart. Useful when login should match either an account name or a display name. |
| `admin_base_dn` | `""` | Subtree for the admin search. Falls back to `base_dn` when empty. |
| `admin_filter` | `""` | Filter that selects administrative users. Empty disables admin synchronization entirely. Supports `{username}` substitution. |

The localpart match is case-insensitive — Tuwunel sends a lowercased version
of the localpart through `{username}` substitution and accepts an entry if
either the original or lowercased form appears in `uid_attribute` or
`name_attribute`.

## Admin synchronization

Setting `admin_filter` to a non-empty value turns the LDAP directory into the
source of truth for who is a Tuwunel admin. On every successful LDAP login,
Tuwunel runs a second search rooted at `admin_base_dn` (or `base_dn` if
empty) using `admin_filter`. Membership in the result set is compared against
the user's current admin status in Tuwunel:

- In LDAP admin set, not a Tuwunel admin → granted admin.
- Not in LDAP admin set, currently a Tuwunel admin → admin revoked.
- Otherwise → no change.

Two examples:

```toml
# Admins are users with a custom objectClass.
admin_filter = "(objectClass=tuwunelAdmin)"

# Admins are members of an LDAP group, looked up under a different subtree.
admin_base_dn = "ou=admins,dc=example,dc=org"
admin_filter = "(uid={username})"
```

Admin synchronization only runs in the search-then-bind modes. In direct-bind
mode the admin search is silently skipped — manage admins manually with
`!admin users make-admin` and `!admin users revoke-admin` if you need that
combination.

## Account lifecycle

The first time a user successfully authenticates against LDAP, Tuwunel
auto-creates a local Matrix account for them (the same way Synapse,
Nextcloud, and Jellyfin behave). The account is registered with origin
`"ldap"` and a placeholder password value — the local password field is
never consulted for an LDAP user, so they can only log in by re-authenticating
against the directory.

Subsequent logins reuse the existing account and only update admin status
if `admin_filter` is configured.

Deactivating a user in LDAP prevents future logins but does **not**
automatically deactivate or delete the corresponding Matrix account. Use
`!admin users deactivate` if you also want to remove access to existing
sessions and devices.

## Admin commands for testing

Two admin commands invoke the LDAP code paths directly without going through
the login API. They are useful for verifying that `filter`, `uid_attribute`,
`bind_dn`, and TLS configuration produce the expected results.

| Command | Description |
|---|---|
| `!admin query users search-ldap @alice:example.org` | Run the configured search for a user and print the matching DNs along with their admin status. Returns an empty list if the filter matches nothing. |
| `!admin query users auth-ldap "cn=alice,ou=users,dc=example,dc=org" "<password>"` | Attempt a direct bind with the given DN and password. Use this to confirm credentials and TLS setup; the password is logged in plaintext to the admin room, so revoke or rotate afterwards. |

Both commands are gated by the `ldap` build feature.

## Disabling password login for non-LDAP users

Tuwunel's LDAP integration always falls back to local password verification
when the LDAP search returns no matches. To enforce LDAP-only login for
everyone (apart from accounts that authenticate via SSO), pair LDAP with a
restrictive `filter` that matches every legitimate user, and remove or
invalidate local passwords for accounts that should no longer be able to log
in directly. Alternatively, set `login_with_password = false` and rely on
[identity providers](providers.md) for non-LDAP users.
