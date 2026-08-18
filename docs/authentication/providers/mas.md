# Matrix Authentication Service (MAS)

Tuwunel can integrate with MAS in two independent directions:

- For login, Tuwunel acts as an OAuth client and MAS is an upstream OpenID
  Connect identity provider.
- For provisioning, MAS acts as a client of Tuwunel's private compatibility API
  and manages Matrix users, profiles, and devices. It can also authorize
  cross-signing key replacement.

They can be enabled independently or together. Each direction has its own
credential: the OAuth client secret authenticates Tuwunel to MAS, while
`mas_secret` authenticates MAS to Tuwunel. Generate separate, high-entropy
values for them.

## Integration model

Tuwunel remains the issuer that Matrix clients use for next-generation
authentication. Its built-in [OIDC Authorization Server](../oidc-server.md)
redirects users to MAS for authentication, then Tuwunel issues the tokens its
clients use.

This differs from MAS's delegated-authentication topology with Synapse. In
particular:

- MAS access tokens cannot be used directly against Tuwunel. Point clients at
  Tuwunel's issuer, not MAS's issuer.
- `mas_secret` does not delegate login or token issuance. It authorizes MAS to
  call Tuwunel's private provisioning API and changes which Synapse-compatible
  account administration routes Tuwunel serves.

Logging in through MAS therefore always requires a
`[[global.identity_provider]]` entry, whether or not provisioning is enabled.

## Configure MAS as a login provider

### Register Tuwunel in MAS

Add a confidential OAuth client to the MAS configuration:

```yaml
clients:
  - client_id: 01J44Q10GR4AMTFZEEF936DTCM
    client_auth_method: client_secret_post
    client_secret: <oauth_client_secret>
    redirect_uris:
      - https://matrix.example.com/_matrix/client/unstable/login/sso/callback/01J44Q10GR4AMTFZEEF936DTCM
```

MAS requires a statically configured `client_id` to be a ULID, which is 26
characters of Crockford base32. Use the same value for MAS's `client_id`,
Tuwunel's `client_id`, and the final path segment of the callback URL.

The redirect URI must match exactly. `client_secret_post` is required for this
integration because Tuwunel sends the client ID and secret in the form-encoded
token request body. MAS supports either `client_secret` or `client_secret_file`.
See the upstream [MAS client configuration](https://element-hq.github.io/matrix-authentication-service/reference/configuration.html#clients)
for the complete client schema.

### Configure Tuwunel

```toml
[[global.identity_provider]]
brand = "MAS"
client_id = "01J44Q10GR4AMTFZEEF936DTCM"
client_secret = "<oauth_client_secret>"
issuer_url = "https://auth.example.com"
callback_url = "https://matrix.example.com/_matrix/client/unstable/login/sso/callback/01J44Q10GR4AMTFZEEF936DTCM"
scope = ["openid"]
```

The `issuer_url` must exactly match the `issuer` value returned by MAS's
`/.well-known/openid-configuration`. This may differ from MAS's
`http.public_base` when a separate `http.issuer` is configured. The MAS HTTP
listener must serve both its `discovery` and `oauth` resources.

Leave `authorization_url` unset. It overrides the authorization endpoint, not
the provider's base address. OIDC discovery fills it from MAS's discovery
document. Setting it to the MAS root sends users to the account-management page
instead of starting an authorization flow.

### Scopes and user information

Current MAS discovery advertises the standard `openid` and `email` scopes, but
not `profile`. Its userinfo endpoint requires `openid` and returns `sub` and
`username`, which is all Tuwunel needs for this login flow.

Tuwunel 1.8.3 defaults an empty `scope` to `["openid"]` when
`brand = "MAS"`. Keeping it explicit also makes the configuration work with
Tuwunel 1.8.2 and earlier, whose default included `profile` and was rejected by
MAS.

## Optional user and device provisioning

MAS can manage its Tuwunel users through the private `/_synapse/mas/*` API.
This is separate from using MAS as an upstream login provider. Login does not
use these routes, and provisioning does not make MAS Tuwunel's token issuer.

Add a high-entropy shared secret to Tuwunel:

```toml
[global]
mas_secret = "<provisioning_secret>"
```

Configure MAS to reach Tuwunel with the same secret:

```yaml
matrix:
  kind: synapse
  homeserver: example.com
  endpoint: "https://matrix.example.com"
  secret: "<provisioning_secret>"
```

Set `homeserver` to Tuwunel's `server_name`. Set `endpoint` to the base URL that
MAS can use to reach Tuwunel. MAS also accepts `secret_file` instead of an inline
secret. See the upstream [MAS homeserver configuration](https://element-hq.github.io/matrix-authentication-service/reference/configuration.html#matrix)
for the full `matrix` schema.

MAS currently documents Synapse as its supported homeserver backend. Use
`kind: synapse` with Tuwunel because Tuwunel implements the same modern private
API that MAS uses for Synapse. Do not add Synapse's
`matrix_authentication_service` configuration to Tuwunel. Tuwunel does not ask
MAS to introspect client access tokens.

If MAS connects through a reverse proxy, forward `/_synapse/mas/*` to Tuwunel.
It is preferable to use a private network path when one is available. Every
request must carry `Authorization: Bearer <provisioning_secret>`; a missing or
incorrect secret receives `403 Forbidden`.

Treat `mas_secret` as an administrator credential. It authorizes MAS to create,
update, lock, unlock, deactivate, reactivate, and erase users; replace verified
email bindings; change profiles; create, rename, delete, and reconcile devices;
and grant bounded authorization for cross-signing key replacement. Keep the API
on a restricted network path where possible and replace the secret if it is
disclosed. The routes remain registered when `mas_secret` is unset, but every
request to them receives `403 Forbidden`.

Provisioning jobs also require a MAS worker. The normal `mas-cli server` command
starts one. Deployments that run the MAS HTTP service without its bundled worker
must run a separate `mas-cli worker` process. See the upstream
[MAS running guide](https://element-hq.github.io/matrix-authentication-service/setup/running.html)
for its process model.

### Provisioning API surface

Tuwunel implements all twelve routes used by MAS's modern homeserver client:

| Area | Method | Endpoint | Operation |
| --- | --- | --- | --- |
| User lifecycle | `GET` | `/_synapse/mas/is_localpart_available` | Check whether a localpart can be assigned. |
| User lifecycle | `GET` | `/_synapse/mas/query_user` | Return the Matrix user ID, profile, and account state. |
| User lifecycle | `POST` | `/_synapse/mas/provision_user` | Create or update an account, including its display name, avatar, email bindings, and lock state. |
| User lifecycle | `POST` | `/_synapse/mas/delete_user` | Deactivate an account and optionally erase its data. |
| User lifecycle | `POST` | `/_synapse/mas/reactivate_user` | Reactivate a deactivated account. |
| Profiles | `POST` | `/_synapse/mas/set_displayname` | Set an account's display name. |
| Profiles | `POST` | `/_synapse/mas/unset_displayname` | Remove an account's display name. |
| Devices | `POST` | `/_synapse/mas/upsert_device` | Create a device, or update its display name when supplied. |
| Devices | `POST` | `/_synapse/mas/update_device_display_name` | Replace an existing device's display name. |
| Devices | `POST` | `/_synapse/mas/delete_device` | Remove a device from the account. |
| Devices | `POST` | `/_synapse/mas/sync_devices` | Reconcile the local device inventory to the set supplied by MAS. |
| Cross-signing | `POST` | `/_synapse/mas/allow_cross_signing_reset` | Grant bounded authorization for cross-signing key replacement. |

These are unversioned implementation endpoints intended only for MAS. Setting
`mas_secret` also removes selected Synapse-compatible account administration
routes that conflict with MAS ownership, matching Synapse's delegated-auth
behavior. See the [Synapse Admin API](../../development/compliance/synapse-admin.md)
page for the affected routes. Only set it when MAS will manage accounts on this
server.

Changing one nonempty `mas_secret` to another takes effect on configuration
reload. Enabling or disabling MAS by crossing between an empty and nonempty
value requires a restart so Tuwunel can rebuild the set of account
administration routes it serves.

## Match MAS-provisioned users

Ensure the MAS worker finishes provisioning each account before that user's
first SSO login. Configure the same self-hosted MAS identity provider as trusted
so its `username` claim can match the existing account:

```toml
trusted = true
userid_claims = ["username"]
```

Only enable trusted mode when `issuer_url` names the same MAS instance that
provisions this server and you operate and fully control it. A trusted provider
can authenticate as any existing Matrix account whose localpart matches a
returned username. The `brand` value selects MAS-specific defaults; it does not
establish trust or link the provider to `mas_secret`.

Do not include `"sub"` in `userid_claims` for this topology. Tuwunel gives it
precedence over `username`, while MAS provisions accounts by localpart. Leaving
`userid_claims` empty also works because Tuwunel considers `username` by
default, but setting it explicitly avoids an accidental future claim mismatch.

If every account must be provisioned by MAS before login, also set:

```toml
registration = false
```

This makes an SSO login fail instead of creating an account when provisioning
has not completed. It only controls registration through this identity
provider. Keep the global `allow_registration` setting disabled as well if every
account creation path must go through MAS. If the provider cannot be trusted,
use an
[admin-approved association](../providers.md#admin-approved-association-for-untrusted-providers)
for each existing user instead.

## Environment variables

The equivalent Tuwunel environment variables are:

```env
TUWUNEL_IDENTITY_PROVIDER__0__BRAND="mas"
TUWUNEL_IDENTITY_PROVIDER__0__CLIENT_ID="01J44Q10GR4AMTFZEEF936DTCM"
TUWUNEL_IDENTITY_PROVIDER__0__CLIENT_SECRET="<oauth_client_secret>"
TUWUNEL_IDENTITY_PROVIDER__0__ISSUER_URL="https://auth.example.com"
TUWUNEL_IDENTITY_PROVIDER__0__CALLBACK_URL="https://matrix.example.com/_matrix/client/unstable/login/sso/callback/01J44Q10GR4AMTFZEEF936DTCM"
TUWUNEL_IDENTITY_PROVIDER__0__SCOPE='["openid"]'
TUWUNEL_MAS_SECRET="<provisioning_secret>"
TUWUNEL_IDENTITY_PROVIDER__0__TRUSTED="true"
TUWUNEL_IDENTITY_PROVIDER__0__USERID_CLAIMS='["username"]'
```

For strict pre-provisioning, also set
`TUWUNEL_IDENTITY_PROVIDER__0__REGISTRATION="false"`. For login without
provisioning, omit `TUWUNEL_MAS_SECRET`. Choose `TRUSTED` and `USERID_CLAIMS`
according to the account-matching policy described above.

## Troubleshooting

- If the browser stops on the MAS account page, remove `authorization_url` and
  let discovery provide the authorization endpoint.
- If MAS reports that `profile` is not allowed, request only `openid`.
- If provisioning calls receive `403 Forbidden`, verify that MAS and Tuwunel
  use the same nonempty provisioning secret and that the request reaches
  Tuwunel's `/_synapse/mas/*` routes.
- If a provisioned user receives a separate account after SSO, verify
  `trusted = true`, `userid_claims = ["username"]`, and that MAS returns the
  provisioned localpart as `username`.
- If MAS never sends provisioning calls, verify that a MAS worker is running.
- If a client fails at registration with an `M_FORBIDDEN` error naming
  `oidc_registration_access_token`, unset that option. The gate belongs to
  Tuwunel's own OIDC server rather than to MAS, it applies to every client
  regardless of how MAS is configured, and no Matrix client can satisfy it. See
  [Dynamic client registration](../oidc-server.md#dynamic-client-registration).
