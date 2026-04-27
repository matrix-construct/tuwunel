# Identity Providers

Tuwunel can delegate login to external OAuth/OIDC identity providers. Each
configured provider appears as an option on the client's login page. Users are
redirected to the provider to authenticate, then returned to Tuwunel which
maps their identity to a Matrix account.

### Provider guides

- [Authelia](providers/authelia.md)
- [Authentik](providers/authentik.md)
- [Keycloak](providers/keycloak.md)
- _Please contribute documentation for yours here!_

## Configuring Tuwunel

Each provider is a `[[global.identity_provider]]` table in your configuration
file. Multiple providers can be configured by repeating the table header. Container users
please refer to the section on [environment variables](#configuring-via-environment-variables) instead.

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


## Configuring via environment variables

For container deployments (Docker Compose, Podman, Kubernetes) where mounting a
configuration file is inconvenient, every provider field can be set via
environment variables instead.

The variable name is built from three parts joined by `__` (double underscore):

```
TUWUNEL_IDENTITY_PROVIDER__<index>__<FIELD>
```

- **`TUWUNEL_IDENTITY_PROVIDER`** — fixed prefix that maps to the
  `[[global.identity_provider]]` table array.
- **`<index>`** — an arbitrary string (typically `0`, `1`, `2`, …) that groups
  variables belonging to the same provider. All variables sharing the same
  index are treated as a single `[[global.identity_provider]]` entry. Indexes
  are sorted lexicographically, so numeric indexes give a predictable order.
- **`<FIELD>`** — the field name from the tables below, uppercased.

Multiple providers are expressed by using different indexes:

```env
# First provider — GitHub
TUWUNEL_IDENTITY_PROVIDER__0__BRAND="github"
TUWUNEL_IDENTITY_PROVIDER__0__CLIENT_ID="Ov23liYourGitHubClientId"
TUWUNEL_IDENTITY_PROVIDER__0__CLIENT_SECRET="your_github_secret"
TUWUNEL_IDENTITY_PROVIDER__0__CALLBACK_URL="https://matrix.example.com/_matrix/client/unstable/login/sso/callback/Ov23liYourGitHubClientId"

# Second provider — Google (marked as default)
TUWUNEL_IDENTITY_PROVIDER__1__BRAND="google"
TUWUNEL_IDENTITY_PROVIDER__1__CLIENT_ID="123456789-abc.apps.googleusercontent.com"
TUWUNEL_IDENTITY_PROVIDER__1__CLIENT_SECRET="GOCSPX-your_secret"
TUWUNEL_IDENTITY_PROVIDER__1__CALLBACK_URL="https://matrix.example.com/_matrix/client/unstable/login/sso/callback/123456789-abc.apps.googleusercontent.com"
TUWUNEL_IDENTITY_PROVIDER__1__DEFAULT="true"
```

Every field listed in the tables below has a matching environment variable. For
example, `trusted = true` in TOML becomes
`TUWUNEL_IDENTITY_PROVIDER__0__TRUSTED="true"`.

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

## Common setup patterns

### Linking existing users to an identity provider

When SSO is added to a server that already has password-based accounts, the
central question is: how does Tuwunel know which provider identity belongs to
which existing Matrix account?

The most direct approach is to set `trusted = true` and list `"sub"` in
`userid_claims`. The `sub` claim is the stable, globally unique user
identifier that every OIDC provider is required to maintain. Listing it in
`userid_claims` tells Tuwunel to use it as the authoritative match key.
Marking the provider as trusted then inverts Tuwunel's normal logic: instead
of registering a new account when it finds no existing match, it looks for an
existing Matrix account whose localpart equals the `sub` value and grants
access to it. The result is that logging in through the provider seamlessly
picks up the user's existing account:

```toml
[[global.identity_provider]]
brand = "Keycloak"
client_id = "tuwunel"
# ...
trusted = true
userid_claims = ["sub"]
```

For this to work, the `sub` value the provider returns for each user must
match that user's Matrix localpart exactly. If your provider allows you to
set `sub` to an arbitrary value, aligning it with the Matrix localpart is
the cleanest path. If it does not — for example, if `sub` is an opaque UUID
— you can either use a different claim (such as `preferred_username`) as the
match key, or link accounts individually using the admin command:

```
!admin query oauth associate <provider_id> @alice:example.com \
  --claim sub=550e8400-e29b-41d4-a716-446655440000
```

This records a permanent association between that provider identity and the
given Matrix account without requiring the claim values to match.

**Only ever set `trusted = true` for identity providers you self-host and
fully control.** In trusted mode, anyone who can present a matching provider
identity gains access to the corresponding Matrix account. Public providers
such as GitHub and Google must never be trusted.

### How Tuwunel derives Matrix user IDs from claims

When a user authenticates through a provider for the first time and no
existing account is linked, Tuwunel must compute a Matrix localpart from the
claims in the provider's userinfo response. It tries each of the following in
order, using the first claim that is present and yields a valid, available
username:

1. `preferred_username`
2. `username`
3. `nickname`
4. `login`
5. `email` — only the portion before the `@` is used

The `sub` claim is deliberately excluded from this default sequence because
it is typically an opaque identifier rather than a human-readable name. It is
only consulted when explicitly listed in `userid_claims`, where it always
takes precedence over every other claim regardless of list order.

If none of these five claims appear in the userinfo response, or if every
derived candidate is already taken by another account, Tuwunel falls back to
a randomly generated localpart. This fallback is controlled by
`unique_id_fallbacks`, which defaults to `true`. On private servers where
silent random assignment is unacceptable, set it to `false` — Tuwunel will
return an error instead.

**Make sure user profiles on your identity provider contain at least one of
the five claims above.** If a profile has none of them, Tuwunel has nothing
to work with and will resort to the random fallback. The safest choice is to
ensure `preferred_username` is populated on every account, as it is the
first claim Tuwunel checks and tends to hold a recognisable, human-readable
name that also makes a reasonable Matrix localpart.

The `userid_claims` field lets you restrict which claims Tuwunel considers
and in what order. For example, to use only `preferred_username` and fall
back to the local part of the email address, and never silently generate a
random ID:

```toml
userid_claims = ["preferred_username", "email"]
unique_id_fallbacks = false
```

Listing `"sub"` anywhere in `userid_claims` elevates it to the highest
priority, overriding all other entries. The special value `"unique"` used
alone instructs Tuwunel to always generate a unique random localpart and
never attempt to derive one from claims at all.

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
| `!admin query oauth list-providers` | List all configured providers and their `provider_id`. |
| `!admin query oauth list-users` | List all users with an active OAuth session. |
| `!admin query oauth list-sessions [--user @user:example.com]` | List `session_id`, optionally filtered by user. |
| `!admin query oauth show-provider <provider_id>` | Show the active configuration for a provider. |
| `!admin query oauth show-user @user:example.com` | Show OAuth sessions for a user. |
| `!admin query oauth associate <provider_id> @user:example.com --claim key=value` | Associate an existing Matrix account with future OAuth claims from a provider. Useful for onboarding existing users to SSO. |
| `!admin query oauth revoke <session_id\|@user:example.com>` | Revoke tokens for a session or all sessions of a user. |
| `!admin query oauth delete <session_id\|@user:example.com>` | Remove OAuth state entirely (destructive). |

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
