# Keycloak

Keycloak is a self-hostable OpenID Connect provider. 

## Keycloak configuration

1. Create client on your keycloak server:

	- Ensure `Client Authentication` is toggled `on`

	- Root Url = `https://<your.matrix.example.com>`

	- Valid Redirect Urls = `https://<your.matrix.tld.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>`

	- Web Origins = `https://<your.matrix.example.com>`

2. Navigate to the Client Credentials tab, note the value of `client secret`

3. Note the `realm` you are creating the client in.

## Tuwunel configuration

> [!IMPORTANT]
> Ensure your matrix .well-known values are being served correctly before beginning.
> Such as with [matrixtest](../../calls/matrix_rtc.md#troubleshooting)

Add the following identity provider section to you tuwunel.toml config file.
Replace the `< placeholders>` with the values noted in your keycloak `client`.

### tuwunel.toml

```toml

[[global.identity_provider]]
brand = 'keycloak'
client_id = '<client_id_in_keycloak>'
client_secret = '<client_secret_from_credentials_tab_in_keycloak>'
issuer_url = 'https://<your.keycloak.example.com>/realms/<realm_name>'
```

### Environment variables

Example Environment variables that can be added to a `docker-compose.yaml` or podman
`tuwunel.env` if preferred:

```env
TUWUNEL_IDENTITY_PROVIDER__0__BRAND="keycloak"
TUWUNEL_IDENTITY_PROVIDER__0__CLIENT_ID="<client_id>"
TUWUNEL_IDENTITY_PROVIDER__0__CLIENT_SECRET="<secret>"
TUWUNEL_IDENTITY_PROVIDER__0__ISSUER_URL="https://<your.keycloak.example.com>/realm/<realm_name>"
```
