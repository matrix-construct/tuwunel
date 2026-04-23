# OIDC Server (Next-Gen Auth)

Tuwunel includes a built-in OIDC authorization server that implements
next-generation Matrix authentication. Matrix clients that support next-gen
auth interact with this server directly instead of using the legacy
`m.login.password` or `m.login.sso` flows.

This implements the following MSCs:

- **MSC2964** — OAuth 2.0 authorization code grant for Matrix
- **MSC2965** — OIDC provider discovery and account management
- **MSC2966** — Dynamic client registration (RFC 7591)
- **MSC2967** — OAuth 2.0 API scopes for Matrix
- **MSC3824** — OIDC-aware client hint (`oidc_aware_preferred`)
- **MSC4312** — Cross-signing reset requiring SSO re-authentication for OIDC
  devices

## Prerequisites

The OIDC server activates automatically when both of the following are
configured:

1. At least one `[[global.identity_provider]]` block (see
   [Identity Providers](providers.md))
2. `well_known.client` in `[global.well_known]`:

```toml
[global.well_known]
client = "https://matrix.example.com"
```

The `well_known.client` URL becomes the OIDC issuer URL. If only one of the
two prerequisites is met, Tuwunel logs a warning at startup and the OIDC
server does not start.

## Endpoints

### Discovery

| Endpoint | Description |
|---|---|
| `/.well-known/openid-configuration` | OIDC discovery document (RFC 8414) |
| `/_matrix/client/v1/auth_issuer` | Matrix auth issuer discovery (MSC2965) |
| `/_matrix/client/v1/auth_metadata` | Authorization server metadata |
| `/_matrix/client/unstable/org.matrix.msc2965/auth_issuer` | Unstable auth issuer endpoint |
| `/_matrix/client/unstable/org.matrix.msc2965/auth_metadata` | Unstable metadata endpoint |

### Authorization server

| Method | Endpoint | Description |
|---|---|---|
| `GET` | `/_tuwunel/oidc/authorize` | Authorization endpoint — starts the OAuth flow |
| `GET` | `/_tuwunel/oidc/_complete` | Completes authorization after provider callback |
| `POST` | `/_tuwunel/oidc/token` | Token endpoint — exchanges auth codes and refresh tokens |
| `POST` | `/_tuwunel/oidc/revoke` | Token revocation (RFC 7009) |
| `GET` | `/_tuwunel/oidc/jwks` | JSON Web Key Set — public keys for JWT verification |
| `GET/POST` | `/_tuwunel/oidc/userinfo` | Userinfo endpoint — returns claims for a bearer token |
| `POST` | `/_tuwunel/oidc/registration` | Dynamic client registration (RFC 7591) |

### Account management UI

| Endpoint | Description |
|---|---|
| `GET /_tuwunel/oidc/account` | Account management page (MSC4191) |

## Dynamic Client Registration

Matrix clients that support next-gen auth register themselves with Tuwunel
before initiating the authorization flow, using RFC 7591 dynamic client
registration:

```
POST /_tuwunel/oidc/registration
```

No pre-configuration of clients is required — any Matrix client that supports
dynamic registration can authenticate against Tuwunel's OIDC server.

## Account Management UI

Tuwunel serves a built-in account management page at `/_tuwunel/oidc/account`
for users authenticated via OIDC. From this page users can:

- View all active OIDC sessions
- See which client and identity provider each session belongs to
- End individual sessions
- Edit their profile

The URL for this page is advertised in the authorization server metadata under
`account_management_uri` (MSC4191).

## Cross-Signing Protection (MSC4312)

Devices that authenticated via the OIDC server are tracked as "OIDC devices."
When such a device attempts to reset cross-signing keys, Tuwunel requires
re-authentication via the original identity provider through the SSO UIAA
flow. This prevents a compromised client from resetting cross-signing without
the user actively re-authorizing through their identity provider.

Administrators can inspect which devices are OIDC devices using the admin
query commands for OAuth sessions.

## Signing Keys

Tuwunel generates and persists an ECDSA signing key on first startup, stored
in the `oidc_signingkey` database table. The corresponding public key is
served at `/_tuwunel/oidc/jwks`. This key signs ID tokens (JWTs) issued by
the token endpoint.

## Startup Warnings

If an `[[global.identity_provider]]` is configured but `well_known.client` is
missing, Tuwunel logs:

```
OIDC server (Next-gen auth) requires `well_known.client` to be configured to serve your `identity_provider`.
```

The OIDC server will not start. Traditional SSO (legacy `m.login.sso` flow)
continues to work without the OIDC server.
