# Authentication systems

Tuwunel gives you fine-grained control over who can register and how users
authenticate. This chapter covers everything from basic password login and
token-based invitations to the built-in OAuth 2.0 / OpenID Connect server that
powers next-generation Matrix auth.

- [**Legacy Authentication**](authentication/legacy.md): control who can register,
  token-based invitations, guest access, and the basic password and token login
  options.

- [**OIDC Authorization Server**](authentication/oidc-server.md): Tuwunel's built-in
  OAuth 2.0 / OIDC server for next-generation Matrix clients. It fronts the
  identity providers below and adds refresh-token, device-grant, and
  account-management support.

- [**Identity Providers**](authentication/providers.md): single sign-on through
  GitHub, Google, Keycloak, and other upstream OAuth/OIDC providers. These also
  supply the human authentication step for the OIDC server above.

- [**LDAP Delegation**](authentication/ldap.md): delegate user management and password
  authentication to an LDAP directory.

- [**Enterprise JWT**](authentication/jwt.md): an operator-controlled signing key that
  can mint a token authenticating as any user.

- [**Migrating Databases**](authentication/migrating.md): preserve provider
  sign-in to migrated accounts after a supported database migration. The
  one-time association procedure does not apply to databases created by
  Tuwunel.
