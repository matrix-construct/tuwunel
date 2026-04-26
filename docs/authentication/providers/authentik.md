# Authentik

[Authentik](https://goauthentik.io/) is a self-hostable identity provider that
speaks OpenID Connect.

> [!NOTE]
> This guide was written against `Authentik 2025.10`; the flow is the same on
> all later versions through at least `2026.2`.

## Authentik configuration

From the admin interface, navigate to **Applications** > **Applications** and
select **Create with Provider**. Review the items below and click **Submit**.

##### Application

- **Application name**: the user-facing name shown to your users on the
  Authentik side.
- **Slug**: appears in the issuer URL on the tuwunel side. Pick something
  short and stable, such as `tuwunel`.

##### Provider

Select **OAuth2/OpenID Provider**.

- **Authorization flow**: any built-in flow is fine if you have not created
  custom ones.
- **Client ID** and **Client Secret**: Authentik generates these. Save both
  values — tuwunel needs them.
- **Redirect URIs/Origins**: set this to
  `https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>`,
  substituting your Matrix server's public hostname and the **Client ID** from
  the previous step.
- All other fields can stay at their defaults.

##### Bindings

Optional. Add policies here to restrict tuwunel access to a subset of your
Authentik users.


## Tuwunel configuration

Add an `[[global.identity_provider]]` entry to your `tuwunel.toml`:

```toml
[[global.identity_provider]]
brand = "Authentik"
client_id = "<client_id>"
client_secret = "<client_secret>"
issuer_url = "https://<your.authentik.example.com>/application/o/<slug>/"
callback_url = "https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>"
```

`issuer_url` is the application's slug-based path with a trailing slash. This
is the value Authentik returns as the `iss` claim in issued tokens, and tuwunel
discovers all other endpoints from
`<issuer_url>.well-known/openid-configuration` automatically.

If your Authentik instance reports a different `iss` — for example when running
behind a path prefix, or with a non-default **Issuer Mode** on the provider —
override discovery directly:

```toml
discovery_url = "https://<your.authentik.example.com>/application/o/<slug>/.well-known/openid-configuration"
```

> [!TIP]
> For the full set of identity provider options see the
> [providers reference](../providers.md). For the Authentik side, see the
> [OAuth2/OpenID Connect provider documentation](https://docs.goauthentik.io/add-secure-apps/providers/oauth2/).

## Customising the Matrix localpart

By default tuwunel derives a new user's Matrix localpart from the
`preferred_username` claim Authentik returns — see
[How tuwunel derives Matrix user IDs from claims][user-ids-from-claims]. The
default Authentik mapping populates `preferred_username` with the user's
Authentik username, so user `foo` becomes `@foo:example.com`.

[user-ids-from-claims]:
    ../providers.md#how-tuwunel-derives-matrix-user-ids-from-claims

To decouple the two — for example to give Authentik user `foo` the localpart
`@bar:example.com` — replace Authentik's default `profile` scope with a custom
property mapping that returns the localpart you want.

### Create a custom property mapping

In the admin interface, navigate to **Customization** > **Property Mappings**
and select **Create**. Choose **Scope Mapping**, then set the **Scope name** to
`profile`.

In **Expression**, return a dictionary that exposes the desired localpart as
`preferred_username`. The example below uses a per-user `matrix_localpart`
attribute when set, falling back to the Authentik username:

```python
if "matrix_localpart" in request.user.attributes:
    return {
        "name": request.user.name,
        "given_name": request.user.name,
        "preferred_username": request.user.attributes["matrix_localpart"],
        "nickname": request.user.attributes["matrix_localpart"],
        "groups": [group.name for group in request.user.ak_groups.all()],
    }
return {
    "name": request.user.name,
    "given_name": request.user.name,
    "preferred_username": request.user.username,
    "nickname": request.user.username,
    "groups": [group.name for group in request.user.ak_groups.all()],
}
```

Note the **Name** you give the mapping, then click **Finish**.

### Replace the default profile mapping

In **Applications** > **Providers**, edit your tuwunel provider, expand
**Advanced protocol settings**, and find the **Scopes** field.

Move your new mapping from **Available Scopes** into **Selected Scopes** with
the right arrow (`>`), then move `authentik default OAuth Mapping: OpenID
'profile'` out with the left arrow (`<`). Click **Update**.

### Set the attribute on a user

In **Directory** > **Users**, edit the user and add to **Attributes**:

```yaml
matrix_localpart: bar
```

Click **Update**.

> [!TIP]
> Users can be allowed to set the attribute themselves through a custom prompt
> in a Stage Configuration flow. See Authentik's documentation for details.

### Verify

In **Applications** > **Providers**, open your provider and click **Preview**.
Select the user under **Preview for user**; the JWT payload should contain the
customised localpart:

```json
{
    "preferred_username": "bar",
    "nickname": "bar"
}
```
