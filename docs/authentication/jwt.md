# JSON web token for enterprise

Tuwunel can accept signed JSON Web Tokens as proof of identity, both as a
login flow (`POST /_matrix/client/v3/login` with `type = org.matrix.login.jwt`)
and as a User-Interactive Authentication step for sensitive operations.

This is an **enterprise feature** intended for managed deployments where
identity is owned by an external system. Two typical uses:

- **Externalized user management.** A hosting provider or corporate
  identity service mints short-lived JWTs for its users; Tuwunel becomes
  a stateless consumer of those tokens and creates Matrix accounts on
  first login.

- **Account override.** An operator-controlled signing key can mint a
  token that authenticates as any user for password resets, key recovery,
  or legal compliance without modifying the user's credentials.


## Enabling JWT

The minimum configuration to accept JWTs is `enable = true` and a `key`:

```toml
[global.jwt]
enable = true
key = "yJKn0!E2g$5Hs!rUv9NQwL@ZmpQ3xVT"
```

With these defaults, Tuwunel will accept any HS256-signed token whose
`sub` claim is the localpart of an MXID on this server.

`POST /_matrix/client/v3/login` then accepts:

```json
{
  "type": "org.matrix.login.jwt",
  "token": "<jwt>"
}
```

`GET /_matrix/client/v3/login` advertises the flow as long as
`enable = true`.

The `sub` (subject) claim is the **localpart** of the user's MXID. Tuwunel
combines it with this server's `server_name` to form the full MXID. The
subject is lowercased before lookup.

For a server with `server_name = "matrix.example.org"`, a token with
`"sub": "alice"` authenticates as `@alice:matrix.example.org`. The token
issuer must agree with the server on this naming.


## Configuration reference

| Field | Default | Description |
|---|---|---|
| `enable` | `false` | Master switch for JWT login. Also gates the UIAA flow. |
| `key` | — | Verification key. Plaintext, base64, or PEM depending on `format`. **Sensitive** — keep private when used as an HMAC secret. |
| `format` | `"HMAC"` | One of `HMAC`, `B64HMAC`, `ECDSA`, `EDDSA`. Selects how `key` is decoded. |
| `algorithm` | `"HS256"` | JWT `alg` header value. Must be compatible with `format`. |
| `register_user` | `true` | Auto-create a Matrix account on first valid login if the user doesn't already exist. Set to `false` to require pre-existing accounts. |
| `audience` | `[]` | Optional list of accepted `aud` claim values. When non-empty, tokens must claim at least one entry; `aud` becomes a required claim. |
| `issuer` | `[]` | Optional list of accepted `iss` claim values. When non-empty, tokens must claim at least one entry; `iss` becomes a required claim. |
| `require_exp` | `false` | If `true`, tokens without an `exp` claim are rejected. Defaults to `false` for Synapse compatibility. |
| `require_nbf` | `false` | If `true`, tokens without an `nbf` claim are rejected. |
| `validate_exp` | `true` | When `exp` is present, enforce that the token has not expired. |
| `validate_nbf` | `true` | When `nbf` is present, enforce that the token has reached its validity window. |

`key` is also accepted under the alias `secret` to match Synapse config
files.


## Migrating from Synapse

Synapse's JWT support uses a configuration of similar shape. To migrate
a Synapse `experimental_features.jwt_config` block:

| Synapse | Tuwunel |
|---|---|
| `enabled` | `enable` |
| `secret` | `key` (also accepted as `secret` for direct migration) |
| `algorithm` | `algorithm` |
| `audiences` | `audience` |
| `issuer` | `issuer` (now a list; wrap a single value as `["..."]`) |

Synapse defaults to optional `exp`/`nbf` and accepts the localpart in
the `sub` claim. Tuwunel matches both behaviors out of the box, so a
straight key+algorithm port should authenticate the same set of tokens.


## Account lifecycle

The first time a token authenticates as a user that does not yet exist:

- If `register_user = true`, Tuwunel creates the account with origin
  `"jwt"` and a placeholder password marker. The local password field is
  never read for a JWT-authenticated user — they can only re-authenticate
  by presenting another valid JWT.
- If `register_user = false`, the request fails with `M_NOT_FOUND` and
  the account is not created.

Subsequent logins reuse the existing account.

JWT does not synchronize admin status, group membership, or display
names — the token grants login only. If you need ongoing identity
attribute synchronization, use [LDAP](ldap.md) or an
[OIDC identity provider](providers.md) instead.


## UIAA — JWT for account override

When `enable = true`, the `m.login.jwt` UIAA stage becomes available
alongside `m.login.password` and `m.login.sso` for sensitive operations
that require interactive re-authentication (deactivate account, change
password, add 3PID, etc.).

A JWT presented at the UIAA stage validates the *user* but does **not**
auto-register: the token's subject must already exist as a Matrix
account. This restriction prevents an account-override flow from
accidentally creating new accounts when an operator intends only to
substitute identity for an existing one.

A typical operator workflow for a forced password reset:

1. Sign a JWT with `sub` set to the target user's localpart.
2. Submit it as the `auth` field of `POST /_matrix/client/v3/account/password`:

   ```json
   {
     "auth": {
       "type": "org.matrix.login.jwt",
       "token": "<jwt>"
     },
     "new_password": "<new password>"
   }
   ```

3. Tuwunel validates the signature, confirms the user exists, and
   completes the password change without ever consulting the user's
   existing credentials.

Limit access to the signing key accordingly. Anyone with the HMAC
secret, or the matching ECDSA/EdDSA private key, can authenticate as any
user on the server.


## Key formats and algorithms

`format` selects how `key` is interpreted. `algorithm` selects the JWT
signing algorithm. The two must agree.

| `format` | `algorithm` examples | `key` content |
|---|---|---|
| `HMAC` (default) | `HS256`, `HS384`, `HS512` | Plaintext shared secret. |
| `B64HMAC` | `HS256`, `HS384`, `HS512` | Base64-encoded shared secret. Use this when the secret contains non-printable bytes. |
| `ECDSA` | `ES256`, `ES384` | PEM-encoded ECDSA public key. |
| `EDDSA` | `EdDSA` | PEM-encoded Ed25519 public key. |

For asymmetric formats (`ECDSA`, `EDDSA`) the `key` is the **public** key —
Tuwunel only verifies, it never signs. The corresponding private key
stays with the issuer.

```toml
# HMAC shared secret (Synapse-compatible default)
format = "HMAC"
algorithm = "HS256"
key = "..."

# ECDSA public-key
format = "ECDSA"
algorithm = "ES256"
key = """-----BEGIN PUBLIC KEY-----
MFkwEwYHKoZIzj0CAQYIKoZIzj0DAQcDQgAE...
-----END PUBLIC KEY-----"""
```

## Time-bounded tokens

`exp` (expiration) and `nbf` (not-before) follow the spec semantics:
seconds since the Unix epoch. Configure based on the issuer's behavior:

- **Issuer always sets `exp`** — set `require_exp = true` to reject any
  token without an expiration. `validate_exp` is `true` by default.
- **Issuer never sets `exp`** — leave both `require_exp` and
  `validate_exp` at their defaults; tokens without `exp` are accepted as
  non-expiring (use cautiously).
- **Mixed** — leave `require_exp = false` and `validate_exp = true` (the
  defaults). This is Synapse-compatible: `exp` is optional but enforced
  when present.

`nbf` is symmetric with `exp` and most deployments leave it unset on both
issuer and consumer.


## Audience and issuer validation

By default no `aud` or `iss` validation is performed. To restrict
acceptance to tokens issued by, or destined for, specific systems, set
the respective config field:

```toml
audience = ["https://matrix.example.org"]
issuer   = ["https://idp.example.org"]
```

When set, the corresponding claim becomes **required** in addition to
being checked against the allowed list. Multiple values are treated as
"any of these is acceptable."
