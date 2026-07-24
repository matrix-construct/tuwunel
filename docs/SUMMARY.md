# Summary

[👋 Help Us Help You](funding.md)

# Installation

- [Introduction](introduction.md)
- [Deployment](deploying.md)
  - [Configuration](configuration.md)
    - [Examples](configuration/examples.md)
  - [Generic](deploying/generic.md)
    - [Reverse Proxy - Caddy](deploying/reverse-proxy-caddy.md)
    - [Reverse Proxy - Nginx](deploying/reverse-proxy-nginx.md)
    - [Reverse Proxy - Traefik](deploying/reverse-proxy-traefik.md)
    - [Example: root domain delegation](deploying/root-domain-delegation.md)
  - [Arch Linux](deploying/arch-linux.md)
  - [Debian](deploying/debian.md)
  - [FreeBSD](deploying/freebsd.md)
  - [NixOS](deploying/nixos.md)
  - [Red Hat](deploying/redhat.md)
  - [Containers](deploying/containers.md)
    - [Docker](deploying/docker.md)
    - [Podman](deploying/podman-systemd.md)
    - [Kubernetes](deploying/kubernetes.md)

# Operation

- [Authentication](authentication.md)
  - [Legacy Registration](authentication/legacy.md)
  - [OIDC Authorization](authentication/oidc-server.md)
  - [QR Code Login](authentication/qr-login.md)
  - [LDAP Delegation](authentication/ldap.md)
  - [Enterprise JWT](authentication/jwt.md)
  - [Identity Providers](authentication/providers.md)
    - [Authelia](authentication/providers/authelia.md)
    - [Authentik](authentication/providers/authentik.md)
    - [Keycloak](authentication/providers/keycloak.md)
- [Multimedia and Storage](media.md)
  - [Storage Providers](media/storage.md)
  - [Management](media/management.md)
- [Video and Voice Conferencing](calls.md)
  - [Matrix RTC (Element Call)](calls/matrix_rtc.md)
  - [Legacy Telephony (TURN)](calls/turn.md)
- [Bridge and Application Services](appservices.md)
- [Push Notifications](pushers.md)
- [Policy and Moderation](moderation.md)

# Servicing

- [Maintenance](maintenance.md)
- [Troubleshooting](troubleshooting.md)

# Engineering

- [Development](development.md)
  - [Contributing](contributing.md)
  - [Protocol Compliance](development/compliance.md)
    - [MSC Implementation](development/compliance/msc.md)
    - [Complement Results](development/compliance/complement.md)
    - [Synapse Admin API](development/compliance/synapse-admin.md)
  - [Testing and Delivery](development/testing.md)
    - [Docker Builder](development/testing/bake.md)
    - [Matrix Selectors](development/testing/matrix.md)
    - [Benchmarks and Performance](development/testing/performance.md)
    - [Pipeline Phases](development/testing/pipeline.md)
    - [Complement Testing](development/testing/complement.md)
  - [Hot Reloading ("Live" Development)](development/hot_reload.md)

---

[💕 Community Code of Conduct](CODE_OF_CONDUCT.md)
