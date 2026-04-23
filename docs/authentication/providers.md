# Identity Providers

Tuwunel can delegate login to external OAuth/OIDC identity providers. Each
configured provider appears as an option on the client's login page. Users are
redirected to the provider to authenticate, then returned to Tuwunel which
maps their identity to a Matrix account.

### Provider guides

- [Authelia](providers/authelia.md)
- [Keycloak](providers/keycloak.md)
- _Please contribute documentation for yours here!_

## Configuring Tuwunel

Each provider is a `[[global.identity_provider]]` table in your configuration
file. Multiple providers can be configured by repeating the table header.

### Required fields

| Field | Description |
|---|---|
| `brand` | Brand name of the provider: `Apple`, `Facebook`, `GitHub`, `GitLab`, `Google`, `Keycloak`, `MAS`, `Twitter`, or any custom string. Case-insensitive. Known brands get built-in defaults and workarounds. |
| `client_id` | The OAuth application ID issued by the provider. This becomes the provider's unique ID within Tuwunel and **must never change** — Tuwunel associates stored identities to it. |

### Authentication

| Field | Default | Description |
|---|---|---|
| `client_secret` | — | OAuth client secret issued by the provider. |
| `client_secret_file` | — | Path to a file containing the client secret. Takes priority over `client_secret`. Example: `/etc/tuwunel/.github_secret` |

### Discovery

| Field | Default | Description |
|---|---|---|
| `issuer_url` | brand default | Provider's OIDC issuer URL. Pre-supplied for well-known public providers. Required for self-hosted providers. Must match exactly what the provider expects and **must never change**. |
| `base_path` | brand default | Extra path after `issuer_url` leading to the `.well-known` directory. GitHub uses `login/oauth/`, for example. Pre-populated for known brands. |
| `discovery_url` | — | Fully overrides the `.well-known/openid-configuration` location. For developers or non-standard providers. |
| `discovery` | `true` | Whether to perform OIDC discovery at all. |

### Callback

| Field | Description |
|---|---|
| `callback_url` | The callback URL registered with the provider when you created the OAuth application. Must be exactly: `https://<your-matrix-server>/_matrix/client/unstable/login/sso/callback/<client_id>` |

### Login behavior

| Field | Default | Description |
|---|---|---|
| `default` | `false` | Mark this provider as the default for `/_matrix/client/v3/login/sso/redirect` (the endpoint without a provider ID). Required when multiple providers are configured and some clients (e.g. FluffyChat) need a single redirect target. If exactly one provider is configured it is implicitly the default. **(Experimental)** Multiple providers can share `default = true` — all must authorize successfully in sequence. |
| `name` | `brand` | Display name shown on the login page. Useful when multiple providers share the same brand. |
| `icon` | brand default | MXC URI for the provider's icon. Known brands have built-in icons. |
| `scope` | all | List of OAuth scopes to request. Empty array means all scopes configured in the provider application. Users can further restrict scopes during authorization. |

### User ID mapping

| Field | Default | Description |
|---|---|---|
| `userid_claims` | all | Claims used to compute the Matrix localpart for new registrations. When empty, Tuwunel avoids generated IDs where possible. The special value `"unique"` forces generated IDs exclusively. The claim `"sub"` takes precedence over all others when listed. |
| `trusted` | `false` | Inverts user matching: instead of registering a new account when claims conflict with existing users, Tuwunel finds the first matching user and grants access to it. **Only set this for providers you self-host and fully control. Never use with public providers (GitHub, GitLab, Google, etc.) — it enables account takeover.** |
| `unique_id_fallbacks` | `true` | When no claim maps cleanly to an available username, generate a unique random localpart as a fallback. Set to `false` on private servers where random usernames are undesirable — a misconfiguration will produce an error instead. |
| `registration` | `true` | Whether this provider can create new Matrix accounts. Set to `false` to restrict the provider to existing users only. |

### URL overrides

These override endpoints that are normally discovered automatically. Only use
them for non-standard or undiscoverable providers.

| Field | Description |
|---|---|
| `authorization_url` | Override the authorization endpoint. |
| `token_url` | Override the token endpoint. |
| `revocation_url` | Override the token revocation endpoint. |
| `introspection_url` | Override the token introspection endpoint. |
| `userinfo_url` | Override the userinfo endpoint. |

### Session

| Field | Default | Description |
|---|---|---|
| `grant_session_duration` | `300` | Seconds the authorization session stays valid before expiring (default: 5 minutes). |
| `check_cookie` | `true` | Verify the redirect cookie during the callback for CSRF protection. Disable only if a reverse proxy strips cookies. |


## Example configurartions

### GitHub

```toml
[[global.identity_provider]]
brand = "GitHub"
client_id = "Ov23liYourGitHubClientId"
client_secret = "your_github_client_secret"
callback_url = "https://matrix.example.com/_matrix/client/unstable/login/sso/callback/Ov23liYourGitHubClientId"
```

GitHub's `issuer_url` and `base_path` are pre-configured. `client_id` doubles
as the provider ID in the callback URL.

### Google

```toml
[[global.identity_provider]]
brand = "Google"
client_id = "123456789-abc.apps.googleusercontent.com"
client_secret = "GOCSPX-your_secret"
callback_url = "https://matrix.example.com/_matrix/client/unstable/login/sso/callback/123456789-abc.apps.googleusercontent.com"
```

### Self-hosted Keycloak

```toml
[[global.identity_provider]]
brand = "Keycloak"
client_id = "tuwunel"
client_secret = "your_keycloak_secret"
issuer_url = "https://sso.example.com/realms/myrealm"
callback_url = "https://matrix.example.com/_matrix/client/unstable/login/sso/callback/tuwunel"
trusted = true
```

With `trusted = true`, users whose Keycloak username matches an existing Matrix
localpart are granted access to that account. Only use `trusted` when you
control the identity provider.

### Matrix Authentication Service (MAS)

```toml
[[global.identity_provider]]
brand = "MAS"
client_id = "your_mas_client_id"
client_secret = "your_mas_secret"
issuer_url = "https://auth.example.com"
callback_url = "https://matrix.example.com/_matrix/client/unstable/login/sso/callback/your_mas_client_id"
```

## Multiple providers

When multiple providers are configured, each appears separately on the
client's login page (unless `single_sso = true`). The `default` field controls
which provider `/_matrix/client/v3/login/sso/redirect` (without a provider ID)
redirects to:

```toml
[[global.identity_provider]]
brand = "GitHub"
client_id = "github_client_id"
# ...
default = true   # this provider handles the bare SSO redirect

[[global.identity_provider]]
brand = "Google"
client_id = "google_client_id"
# ...
```

If no provider is explicitly `default` and exactly one is configured, it
becomes the implicit default.

## Global SSO options

These top-level options control how SSO providers are presented to clients.

| Option | Default | Description |
|---|---|---|
| `single_sso` | `false` | **(Experimental)** Replace the provider list with a single "Sign in with single sign-on" button at `/_matrix/client/v3/login/sso/redirect`. All providers are attempted in sequence and all must succeed. |
| `sso_custom_providers_page` | `false` | Replace the provider list with a single button and expect a reverse proxy to intercept `/_matrix/client/v3/login/sso/redirect` and serve a custom provider-selection page. Each entry on that page should link to `/_matrix/client/v3/login/sso/redirect/<client_id>`. |
| `oidc_aware_preferred` | `false` | Advertise OIDC as the preferred login method (MSC3824). Clients that support next-gen auth will present it as the only option. |

## Admin commands

These admin room commands help manage OAuth state:

| Command | Description |
|---|---|
| `!admin query oauth list-providers` | List all configured providers and their IDs. |
| `!admin query oauth list-users` | List all users with an active OAuth session. |
| `!admin query oauth list-sessions [--user @user:example.com]` | List session IDs, optionally filtered by user. |
| `!admin query oauth show-provider <id>` | Show the active configuration for a provider. |
| `!admin query oauth show-user @user:example.com` | Show OAuth sessions for a user. |
| `!admin query oauth associate <provider_id> @user:example.com --claim key=value` | Associate an existing Matrix account with future OAuth claims from a provider. Useful for onboarding existing users to SSO. |
| `!admin query oauth revoke <session_id\|@user>` | Revoke tokens for a session or all sessions of a user. |
| `!admin query oauth delete <session_id\|@user>` | Remove OAuth state entirely (destructive). |

## Protocol flow reference

1. The client fetches `/_matrix/client/v3/login` and finds an `m.login.sso`
   entry listing configured providers.
2. The user selects a provider; the client redirects to
   `/_matrix/client/v3/login/sso/redirect/<client_id>`.
3. Tuwunel redirects the user to the provider's authorization endpoint.
4. The provider authenticates the user and redirects back to
   `/_matrix/client/unstable/login/sso/callback/<client_id>`.
5. Tuwunel exchanges the code for tokens, fetches user claims, maps them to a
   Matrix user ID, and issues a login token back to the client.
