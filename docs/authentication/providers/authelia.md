# Authelia

Add the client in Authelia's config:

> [!NOTE]
> The client_secret Hash must be generated using [Authelia cli generator](https://www.authelia.com/integration/openid-connect/frequently-asked-questions/#client-secret). Always start as `$pbkdf2`
```yaml
identity_providers:
  oidc:
    claims_policies:
      tuwunel:
        id_token: ["email", "name", "groups", "preferred_username"]
    clients:
      - client_id: '<client_id>'
        client_name: 'tuwunel'
        client_secret: '<client_secret:Hash>'
        claims_policy: "tuwunel"
        public: false
        redirect_uris:
          - "<domain of authelia>/_matrix/client/unstable/login/sso/callback/<client_id>"
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

The Tuwunel Config will look like this:
> [!NOTE]
> The client_secret Password must be generated using [Authelia cli generator](https://www.authelia.com/integration/openid-connect/frequently-asked-questions/#client-secret).
```yaml
[[global.identity_provider]]
brand = "Authelia"
name = "Authelia"
default = true # Check the docs relative to it before copy-paste.
client_id = "<client_id>"
client_secret = "<client_secret:Password>"
issuer_url = "<domain of authelia>"
callback_url = "<domain of authelia>/_matrix/client/unstable/login/sso/callback/<client_id>"
```

See the [Authelia OIDC documentation](https://www.authelia.com/configuration/identity-providers/openid-connect/clients/) for full details.
