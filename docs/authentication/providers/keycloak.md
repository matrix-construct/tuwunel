# Keycloak

Keycloak is a self-hostable OpenID Connect provider.

## Keycloak configuration

1. Create a client on your Keycloak server:

   - Enable **Client Authentication**.
   - Set **Root URL** to `https://<your.matrix.example.com>`.
   - Add a **Valid Redirect URI**:
     `https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>`
   - Set **Web Origins** to `https://<your.matrix.example.com>`.

2. Navigate to the **Credentials** tab and note the **Client Secret**.

3. Note the **realm** you created the client in.

## Tuwunel configuration

> [!IMPORTANT]
> Ensure your Matrix `.well-known` values are being served correctly before
> starting. You can verify them with
> [matrixtest](../../calls/matrix_rtc.md#troubleshooting).

Add the following to your `tuwunel.toml`. Replace the placeholders with the
values from your Keycloak client.

```toml
[[global.identity_provider]]
brand = "Keycloak"
client_id = "<client_id_in_keycloak>"
client_secret = "<client_secret_from_credentials_tab>"
issuer_url = "https://<your.keycloak.example.com>/realms/<realm_name>"
callback_url = "https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id_in_keycloak>"
trusted = true
```

Setting `trusted = true` allows users whose Keycloak username matches an
existing Matrix localpart to log in to that account via SSO. Only use this
for identity providers you self-host and fully control — see
[Linking existing users](../providers.md#linking-existing-users-to-an-identity-provider).

## Environment variables

If you prefer environment variables (e.g. in `docker-compose.yaml` or a
`tuwunel.env` file):

```env
TUWUNEL_IDENTITY_PROVIDER__0__BRAND="keycloak"
TUWUNEL_IDENTITY_PROVIDER__0__CLIENT_ID="<client_id>"
TUWUNEL_IDENTITY_PROVIDER__0__CLIENT_SECRET="<client_secret>"
TUWUNEL_IDENTITY_PROVIDER__0__ISSUER_URL="https://<your.keycloak.example.com>/realms/<realm_name>"
TUWUNEL_IDENTITY_PROVIDER__0__CALLBACK_URL="https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>"
TUWUNEL_IDENTITY_PROVIDER__0__TRUSTED="true"
```
