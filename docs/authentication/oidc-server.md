# OIDC Authorization Server (Next-Gen Auth)

Tuwunel ships a built-in OAuth 2.0 / OpenID Connect authorization server. This
is the server side of Matrix "next-generation authentication": clients that
support it talk OAuth to Tuwunel directly instead of using the legacy
`m.login.password` or `m.login.sso` flows.

Tuwunel plays two roles here, and they are easy to confuse:

- **As an authorization server**, Tuwunel is what a Matrix client authenticates
  against. It issues authorization codes, access tokens, and refresh tokens,
  and it publishes OAuth discovery metadata. That is what this page documents.
- **As a relying party**, Tuwunel does not collect passwords itself. It
  delegates authentication of the human to a configured upstream identity
  provider (GitHub, Google, Keycloak, Authelia, your own Matrix Authentication
  Service, and so on), covered on the [Identity Providers](providers.md) page.

So the OIDC server is the Matrix-facing OAuth layer, while the actual login
happens at whichever identity provider you configure. Next-gen auth needs both
halves in place.

## Standards implemented

The authorization server implements the following Matrix proposals and IETF
specifications. The MSCs define the Matrix profile; the RFCs define the
underlying OAuth behavior.

| Spec | Purpose |
|---|---|
| MSC3861 | Next-generation auth for Matrix, the umbrella proposal |
| MSC2964 | OAuth 2.0 authorization-code and refresh-token grants |
| MSC2965 | Authorization server metadata discovery and the `auth_issuer` endpoint |
| MSC2966 | Dynamic client registration |
| MSC2967 | OAuth 2.0 API and device scopes for Matrix |
| MSC3824 | OIDC-aware client hint (`oidc_aware_preferred`) |
| MSC4191 | Account-management URL and actions |
| MSC4312 | Cross-signing reset requiring SSO re-authentication |
| MSC4341 | Device authorization grant (RFC 8628) |
| RFC 6749 | The OAuth 2.0 authorization framework |
| RFC 8414 | Authorization server metadata document |
| RFC 7591 | Dynamic client registration |
| RFC 7636 | Proof Key for Code Exchange (PKCE) |
| RFC 7009 | Token revocation |
| RFC 8628 | Device authorization grant |

## Enabling the server

The OIDC server needs a client well-known base URL, which becomes the OAuth
**issuer** URL:

```toml
[global.well_known]
client = "https://matrix.example.com"
```

It then activates once at least one way to authenticate users is configured.
Choose either or both:

1. **Native accounts.** Set `oidc_native_auth = true` to let clients register
   and log in against this server's own accounts, with no third-party provider
   (see [Native authentication](#native-authentication)).
2. **An upstream identity provider.** Add at least one
   `[[global.identity_provider]]` block (see [Identity Providers](providers.md))
   to broker authentication to an external IdP.

If `well_known.client` is set but neither source is configured, the server logs
a warning at startup and does not start. Legacy SSO (the `m.login.sso` flow)
keeps working regardless.

```
OIDC server (Next-gen auth) requires `well_known.client` to be configured.
```

## How a login flows

For an interactive (browser) client, the authorization-code grant runs like
this:

1. The client discovers the issuer via `/_matrix/client/v1/auth_issuer`, then
   fetches the metadata document to learn every endpoint URL.
2. The client (registering itself first if needed, see
   [Dynamic client registration](#dynamic-client-registration)) sends the user
   to `GET /_tuwunel/oidc/authorize` with a PKCE `code_challenge`.
3. Tuwunel validates the request and redirects the browser to the upstream
   identity provider's SSO flow
   (`/_matrix/client/v3/login/sso/redirect/<provider>`). A client may request a
   specific provider with an `idp_id` query parameter; otherwise the default
   provider is used.
4. The user authenticates with that provider. The provider redirects back, and
   Tuwunel finishes the exchange at `GET /_tuwunel/oidc/_complete`.
5. Tuwunel returns an authorization code to the client, which the client
   exchanges at `POST /_tuwunel/oidc/token` for an access token, a refresh
   token, and (when `openid` was requested) an ID token.

The device that results is tagged with the provider that authenticated it, so
later step-up actions (see [Cross-signing protection](#cross-signing-protection))
re-authenticate against the same provider.

When native authentication is enabled and the request selects no upstream
provider (or carries `prompt=create`), step 3 instead serves a local
login/registration page at `GET /_tuwunel/oidc/native`. The user authenticates
against a local account and the flow rejoins at `_complete` exactly as above.
See [Native authentication](#native-authentication).

## Endpoints

### Discovery

| Endpoint | Description |
|---|---|
| `GET /.well-known/openid-configuration` | OIDC discovery document (RFC 8414) |
| `GET /_matrix/client/v1/auth_issuer` | Matrix auth issuer discovery (MSC2965) |
| `GET /_matrix/client/v1/auth_metadata` | Authorization server metadata |
| `GET /_matrix/client/unstable/org.matrix.msc2965/auth_issuer` | Unstable issuer endpoint |
| `GET /_matrix/client/unstable/org.matrix.msc2965/auth_metadata` | Unstable metadata endpoint |

The metadata document advertises `response_types_supported = ["code"]`,
`code_challenge_methods_supported = ["S256"]`, ID tokens signed with `ES256`,
and the three grant types `authorization_code`, `refresh_token`, and the device
grant.

### Authorization server

| Method | Endpoint | Description |
|---|---|---|
| `GET` | `/_tuwunel/oidc/authorize` | Authorization endpoint; starts the code flow |
| `GET`/`POST` | `/_tuwunel/oidc/_complete` | Completes authorization after the provider callback; the POST is the approval form's target |
| `GET`/`POST` | `/_tuwunel/oidc/native` | Native login/registration page and submission (no upstream IdP) |
| `POST` | `/_tuwunel/oidc/token` | Token endpoint; exchanges codes, refresh tokens, and device codes |
| `POST` | `/_tuwunel/oidc/device_authorization` | Device authorization request (RFC 8628) |
| `GET` | `/_tuwunel/oidc/device` | User-code verification page |
| `GET`/`POST` | `/_tuwunel/oidc/device_callback` | Device consent and approval |
| `POST` | `/_tuwunel/oidc/revoke` | Token revocation (RFC 7009) |
| `GET` | `/_tuwunel/oidc/jwks` | JSON Web Key Set; public keys for verifying issued JWTs |
| `GET`/`POST` | `/_tuwunel/oidc/userinfo` | Userinfo claims for a bearer token |
| `POST` | `/_tuwunel/oidc/registration` | Dynamic client registration (RFC 7591) |

### Account management

| Endpoint | Description |
|---|---|
| `GET /_tuwunel/oidc/account` | Account-management page (MSC4191) |

## Grant types

### Authorization code

The default grant for clients that can open a browser, described in
[How a login flows](#how-a-login-flows) above. PKCE with the `S256` method is
required by default; see `oidc_require_pkce`.

### Device authorization grant (RFC 8628)

For clients that cannot easily host a browser redirect (a TV app, a CLI, a
constrained device), Tuwunel supports the device grant. The flow:

1. The device calls `POST /_tuwunel/oidc/device_authorization` with its
   `client_id` (and optional `scope`). Tuwunel responds with a `device_code`, a
   short human-readable `user_code`, a `verification_uri`
   (`/_tuwunel/oidc/device`), a `verification_uri_complete` that embeds the
   user code, an `expires_in`, and a polling `interval`.
2. The device shows the user the `user_code` and asks them to visit the
   verification URI on another device (phone or laptop).
3. The user opens the page, enters the code (which is then verified against the
   upstream identity provider as in any other login), and approves the request
   on the consent page.
4. Meanwhile the device polls `POST /_tuwunel/oidc/token` with
   `grant_type=urn:ietf:params:oauth:grant-type:device_code` and its
   `device_code`. Until the user approves, the token endpoint answers
   `authorization_pending`; once approved it returns the tokens.

User codes are ten characters drawn from a twenty-letter consonant alphabet and
displayed grouped with a single hyphen (for example `BCDFG-HJKLM`). Input is
normalized, so case, spaces, and hyphens entered by the user do not matter. A
grant lives for thirty minutes, the suggested poll interval is five seconds, and
each grant bounds how many times its code may be brought to the consent step
before it self-invalidates.

The device-grant endpoints share the same per-IP throttle as the rest of the
OIDC surface (see [Rate limiting](#rate-limiting)).

## Configuration reference

Every option below is hot-reloadable: changing it in the config and reloading
takes effect without a restart. All default to preserving the current open
behavior, so an existing deployment is unaffected until you opt in.

### Token and session lifetimes

| Option | Default | Description |
|---|---|---|
| `access_token_ttl` | `604800` | Access-token lifetime in seconds (7 days). After expiry a refresh-capable client is soft-logged-out until it refreshes. Shared with legacy refresh-token logins. |
| `refresh_token_ttl` | `0` | Refresh-token lifetime in seconds. `0` disables refresh-token expiry entirely. A typical enabled value is `259200` (three days). |
| `refresh_token_idle_only` | `true` | When `true`, each successful refresh slides the deadline forward, so a session in continuous use never expires. When `false`, the deadline is fixed at first issuance and the user must re-authenticate after `refresh_token_ttl` regardless of activity. |
| `refresh_token_hard_logout` | `false` | When `false`, an expired refresh token is rejected with `soft_logout: true`, letting the client keep its E2EE keys and resume the same device after re-auth. When `true`, expiry deletes the device entirely and signals `soft_logout: false`, so the next session is a fresh device. |
| `refresh_token_reuse_grace` | `15` | Grace window in seconds for a benign double-submit. If a just-rotated refresh token is replayed within this window while its successor is still current, Tuwunel treats it as a client that lost the rotated response and reissues, rather than revoking. Set to `0` to treat every replay strictly. |
| `refresh_token_reuse_revoke` | `true` | When `true`, a refresh token replayed outside the grace window removes the device. When `false`, the replay is rejected but the device is left intact, which an operator fronting another OAuth client may prefer. |

### Native authentication

| Option | Default | Description |
|---|---|---|
| `oidc_native_auth` | `false` | When `true`, the OIDC server serves a local login and registration page for clients that select no upstream provider, authenticating against this server's own accounts. Requires `well_known.client`. Coexists with configured identity providers. |

With native auth enabled, an authorization request that selects no provider (or
carries `prompt=create`) is served `GET /_tuwunel/oidc/native` instead of an SSO
redirect. Registration there enforces the same `allow_registration`,
registration-token, and `registration_terms` policy as the Matrix registration
endpoint, and the metadata document advertises `prompt_values_supported =
["create"]` so clients can offer account creation. Enabling this knob where the
OIDC server was not already running takes effect on the next restart.

### Authorization hardening

| Option | Default | Description |
|---|---|---|
| `oidc_require_pkce` | `true` | Require a PKCE `code_challenge` with the `S256` method on the authorization-code grant. The `plain` method is always rejected regardless of this setting. Disable only as a transition aid for a legacy client that cannot send a challenge. |
| `oidc_require_device_scope` | `false` | When `false`, a client that omits the MSC2967 device scope is assigned a server-generated device id, echoed back in the granted scope. When `true`, the grant is rejected unless the client supplies its own device scope. |
| `oidc_strict_scope` | `false` | When `false`, an unrecognized scope token is dropped and the narrowed scope is echoed back to the client (RFC 6749). When `true`, an unrecognized scope is rejected outright. `openid` and the MSC2967 api and device scopes (both spellings) are always recognized. |

### Dynamic registration controls

| Option | Default | Description |
|---|---|---|
| `oidc_registration_access_token` | (unset) | When set, the registration endpoint requires callers to present this value as an `Authorization: Bearer` credential. Matrix clients have no way to supply one, so any value here blocks next-gen auth for every ordinary client. Leave it unset unless the only callers are ones you register yourself. |
| `oidc_registration_allowed_redirect_hosts` | `[]` | When non-empty, every `redirect_uri` presented at registration must match this list, otherwise the registration is rejected. A `redirect_uri` with a host matches an entry naming that host; a private-use scheme carries no host (RFC 8252, as in `io.element.android:/`) and matches an entry naming the scheme. Matching ignores case. Empty imposes no restriction at registration, and vouches for no client. |
| `oidc_require_client_approval` | `true` | Show the user an approve/deny page naming the client before an authorization code is issued to a client the operator has not vetted. A client is vetted when its redirect host or private-use scheme appears in `oidc_registration_allowed_redirect_hosts`. |

### Rate limiting

A single token-bucket throttle, keyed by client IP, covers the authorize,
token, registration, and device-grant endpoints.

| Option | Default | Description |
|---|---|---|
| `oidc_rc_per_second` | `0` | Token-bucket refill rate in requests per second. `0` disables the throttle, preserving open access. |
| `oidc_rc_burst_count` | `0` | Burst depth: how many requests one IP may make before the refill rate governs. Ignored while `oidc_rc_per_second` is `0`. |

Because the key is the client IP, a rate low enough to bite a brute-force
attempt can also throttle many legitimate users behind a single NAT. Size the
burst with that in mind if you enable it.

## Dynamic client registration

Matrix clients that support next-gen auth register themselves with Tuwunel
before starting the authorization flow, using RFC 7591 at
`POST /_tuwunel/oidc/registration`. By default no pre-configuration of clients
is required: any client that supports dynamic registration can authenticate.

To bound which clients may register, use
`oidc_registration_allowed_redirect_hosts`. It restricts registration to clients
whose `redirect_uri` you list, and ordinary Matrix clients keep working so long
as they appear in it. List a web client by host and a mobile client by its
private-use scheme, so a deployment serving Element X on both platforms reads:

```toml
oidc_registration_allowed_redirect_hosts = ["element.io", "io.element.android"]
```

### Approving an unvetted client

Open registration means anyone can register a client pointing at a redirect
target they control, then send one of your users an authorization link for it.
Nothing in the registration rules can tell that client apart from a legitimate
one: they only require a client's own metadata to be self-consistent, and an
attacker's metadata describes an attacker's domain consistently.

So by default Tuwunel asks. When the redirect target of a sign-in is not vetted,
the user is shown a page naming the client, its website, and where the sign-in
is about to be handed, with Approve and Deny buttons; the authorization code is
minted only on Approve. A client counts as vetted when its redirect host or
private-use scheme appears in `oidc_registration_allowed_redirect_hosts`. An
initial access token deliberately does not vet anything, because closing
registration says nothing about the clients that were already registered when
you closed it.

Listing the clients you serve therefore restores a sign-in with no extra step,
and is the recommended configuration. Understand what an entry vouches for: a
scheme entry covers any app on the device that claims that scheme (RFC 8252
§8.6), and a host entry covers every path on that host, an open redirector or
user-content subdomain included.

The prompt is a step a person takes, inside a window sized for a machine:
`login_token_ttl` defaults to two minutes, and a user who leaves the page
sitting past it has to start the sign-in again. Raise it on a server that shows
the prompt routinely.

Set `oidc_require_client_approval` to `false` only if you accept that any
registered client can be handed a user's authorization code on a single click.
Tuwunel logs a warning naming the option at startup and on every configuration
reload while it is disabled.

### Closing registration entirely

`oidc_registration_access_token` is a stricter control that carries a cost worth
understanding before you enable it. RFC 7591 §3 makes the initial access token
optional and asks the registration endpoint to allow requests that carry none,
and MSC2966 defines no such token, so no current Matrix client sends the
`Authorization` header this gate checks. Set it only where every OAuth client is
registered out of band by something you operate, and expect ordinary client
logins to stop working.

While it is set, Tuwunel logs a warning naming the option at startup and on
every configuration reload, and a client that tries to register is refused with
`M_FORBIDDEN: A valid initial access token is required; this server has
oidc_registration_access_token set`. Element X surfaces that as a failure to get
an OAuth URL and never reaches the login page. Check the environment as well as
the configuration file, since the same option is set by
`TUWUNEL_OIDC_REGISTRATION_ACCESS_TOKEN` and by its `CONDUWUIT_` and `CONDUIT_`
legacy equivalents. `!admin server show-config` masks the value, so the startup
warning is what tells you the option is set.

## Account management

Tuwunel serves a built-in account-management page at `/_tuwunel/oidc/account`
for users authenticated through the OIDC server. From it a user can:

- View their active OIDC sessions and see which client and identity provider
  each belongs to
- End individual sessions
- Edit their profile

The page URL is advertised to clients as `account_management_uri` in the
authorization server metadata (MSC4191).

## Cross-signing protection

Devices created through the OIDC server are tracked as OIDC devices. When such
a device tries to reset cross-signing keys, Tuwunel requires the user to
re-authenticate through the original identity provider using the SSO step of
the interactive-auth flow (MSC4312). A client that has lost control of an access
token therefore cannot reset cross-signing without the user actively logging in
again at the provider.

Administrators can see which devices are OIDC devices through the OAuth admin
query commands.

## Signing keys

On first startup Tuwunel generates and persists an ECDSA (`ES256`) signing key.
The public half is published at `/_tuwunel/oidc/jwks` and is used to verify the
ID tokens issued by the token endpoint. The key is kept across restarts, so the
JWKS URI is stable.

## Admin commands

OAuth sessions, provider associations, and revocations are managed from the
admin room. The `!admin query oauth` command family is documented alongside the
provider configuration on the [Identity Providers](providers.md#admin-commands)
page.
