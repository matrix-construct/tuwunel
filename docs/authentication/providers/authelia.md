# Authelia

## Authelia configuration

Add the client in Authelia's config. The client secret in Authelia must be
stored as a hash; use the
[Authelia CLI generator](https://www.authelia.com/integration/openid-connect/frequently-asked-questions/#client-secret)
to produce it (the hash always starts with `$pbkdf2`).

```yaml
identity_providers:
  oidc:
    claims_policies:
      tuwunel:
        id_token: ["email", "name", "groups", "preferred_username"]
    clients:
      - client_id: '<client_id>'
        client_name: 'tuwunel'
        client_secret: '<client_secret_hash>'
        claims_policy: "tuwunel"
        public: false
        redirect_uris:
          - "https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>"
        scopes:
          - 'openid'
          - 'groups'
          - 'email'
          - 'profile'
        grant_types:
          - 'refresh_token'
          - 'authorization_code'
        response_types:
          - 'code'
        response_modes:
          - 'form_post'
        token_endpoint_auth_method: 'client_secret_post'
```

## Tuwunel configuration

> [!NOTE]
> The `client_secret` value here is the **plain-text** password, not the hash
> stored in Authelia. Authelia stores the hash; Tuwunel supplies the password.

```toml
[[global.identity_provider]]
brand = "Authelia"
name = "Authelia"
client_id = "<client_id>"
client_secret = "<client_secret_password>"
issuer_url = "https://<your.authelia.example.com>"
callback_url = "https://<your.matrix.example.com>/_matrix/client/unstable/login/sso/callback/<client_id>"
```

See the [Authelia OIDC documentation](https://www.authelia.com/configuration/identity-providers/openid-connect/clients/)
for full details on the provider side.
