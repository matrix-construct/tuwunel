# Authentik

> [!IMPORTANT]
> This guide is based on Authentik version 2026.2.2

## Authentik configuration

Create a provider and application for tuwunel. From the Admin interface, select
**Applications** > **Applications** > **Create with Provider**.

Go through each step of the "New application dialog":

### Application

- Application name: select the user-facing name of your server on the Authentik
  side
- Slug: this will be part of the `base_url` in your tuwunel configuration

### Choose a provider

Select **OAuth2/OpenID Provider**.

### Configure provider

1. Provider Name: this can be left to the default value
1. Authorization flow: if you have not created custom flows, either of the two
   built-in flows can be used depending on desired behaviour
1. **Client ID** and **Client secret**: these will be generated for you. Save
   these values as you will need them in your tuwunel configuration
1. **Redirect URIs/Origins**: set this to
   `https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>`
   replacing `<your.matrix.example.com>` with tuwunel's public (sub)domain and
   `<client_id>` with the value from the previous step
1. Other values can be left as default

### Configure bindings

Optional: set policies if you want to restrict access to tuwunel to only certain
of your Authentik users.

### Review and Submit Application

Verify all information is correct and press "Submit".

## Tuwunel configuration

Add the following to your `tuwunel.toml`:

```toml
[[global.identity_provider]]
brand = "Authentik"
client_id = "<client_id>" # Replace with the Client ID from Authentik
client_secret = "<client_secret>" # Replace with the Client secret from Authentik
callback_url = "https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>" # Replace with the same Callback URL you configure in Authentik
issuer_url = "https://<your.authentik.example.com>/application/o/<slug>" # Replace with your Authentik (sub)domain and the slug you selected above
base_path = "/application/o/<slug>" # Replace with the slug you selected above

# Optional items
#name = "Authentik"
#unique_id_fallbacks = false # prevent randomly generated matrix usernames if none are available
```

See the
[Authentik OAuth 2.0/OIDC documentation](https://docs.goauthentik.io/add-secure-apps/providers/oauth2/)
for full details on the provider side.

## Setting up different Authentik and tuwunel usernames

By default, tuwunel will assign the localpart of a username [based on
Authentik's userinfo][user-ids-from-claims]. By default, the user's Authentik
username will be served as the `preferred_username` to tuwunel. For example,
user `foo` would be assigned the account `@foo:example.com` if it is available.

[user-ids-from-claims]:
    ../providers.md#how-tuwunel-derives-matrix-user-ids-from-claims

This behaviour can be modified using a custom Authentik mapping.

> [!NOTE]
> For example, this configuration would allow Authentik user `foo` to receive
> the matrix username `@bar:example.com`.

### Create a custom mapping for tuwunel

From the Authentik Admin interface, select **Customization** > **Property
Mappings** > **Create**.

When prompted to select a type, choose **Scope Mapping**.

In the "Create Scope Mapping" dialog, set the Scope name to `profile`.

In "Expression", provide a Python expression that will return a dictionary of
userinfo entries. For example, to return a custom user attribute
`matrix_localpart` as the preferred username if it is set, enter:

```python
if "matrix_localpart" in request.user.attributes:
  return {
    "name": request.user.name,
    "given_name": request.user.name,
    "preferred_username": request.user.attributes["matrix_localpart"],
    "nickname": request.user.attributes["matrix_localpart"],
    "groups": [group.name for group in request.user.ak_groups.all()],
}
else:
  return {
    "name": request.user.name,
    "given_name": request.user.name,
    "preferred_username": request.user.username,
    "nickname": request.user.username,
    "groups": [group.name for group in request.user.ak_groups.all()],
}
```

Take note of the **Name** you selected and click **Finish**.

### Replace the default profile mapping in your tuwunel provider

From the Admin interface, select **Applications** > **Providers** and click on
the "Edit" icon for your tuwunel provider.

Expand the **Advanced protocol settings** section and scroll to **Scope**.

In **Available Scopes**, search for the **Name** of your custom mapping, select
it, and add it to the **Selected Scopes** with the right arrow (`>`).

In **Selected Scopes**, remove the
`authentik default OAuth Mapping: OpenID 'profile'` by selecting it and moving
it out with the left arrow (`<`).

Complete the dialog by clicking **Update**.

### Assign a custom attribute to a user

From the Admin interface, select **Directory** > **Users** and click on the
desired user. Under **Actions**, click **Edit**.

Under **Attributes**, add a custom attribute:

```yaml
matrix_localpart: bar
```

Complete the dialog by clicking **Update**.

> [!TIP]
> Users can be allowed to set a custom attribute themselves using a custom
> Prompt within a Stage Configuration flow, but this is beyond the scope of this
> guide.

### Test the custom mapping

From the Admin interface, select **Applications** > **Providers**, click on the
link of your tuwunel provider, then click **Preview**.

Under **Preview for user**, select user `foo`.

The JWT payload should contain the custom matrix localpart:

```json
{
    "preferred_username": "bar",
    "nickname": "bar"
}
``
