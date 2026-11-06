# Tuwunel<sup>💕</sup>

![License](https://img.shields.io/badge/license-Apache--2.0-8A2BE2?style=flat-square)
![GitHub Created At](https://img.shields.io/github/created-at/matrix-construct/tuwunel?style=flat%2Dsquare&color=%238A2BE2)
![GitHub Commit Activity](https://img.shields.io/github/commit-activity/m/matrix-construct/tuwunel?style=flat%2Dsquare&link=https%3A%2F%2Fgithub.com%2Fmatrix-construct%2Ftuwunel%2Fpulse%2Fmonthly&color=%238A2BE2)
![Docker Pulls](https://img.shields.io/docker/pulls/jevolk/tuwunel?style=flat%2Dsquare&color=8A2BE2)
![GitHub Repo Stars](https://img.shields.io/github/stars/matrix-construct/tuwunel?style=flat%2Dsquare&link=https%3A%2F%2Fgithub.com%2Fmatrix-construct%2Ftuwunel&color=%238A2BE2)
[![CI/CD](https://github.com/matrix-construct/tuwunel/actions/workflows/main.yml/badge.svg?branch=main&style=flat%2Dsquare)](https://github.com/matrix-construct/tuwunel/actions/workflows/main.yml)
[![Copr build status](https://copr.fedorainfracloud.org/coprs/trapacid/tuwunel/package/tuwunel/status_image/last_build.png)](https://copr.fedorainfracloud.org/coprs/trapacid/tuwunel/package/tuwunel/)

<!-- ANCHOR: catchphrase -->

## High Performance Matrix Homeserver in Rust!

<!-- ANCHOR_END: catchphrase -->

<!-- ANCHOR: body -->

[![Documentation](https://img.shields.io/badge/documentation%2D_?color=%238A2BE2&style=for-the-badge&logo=mdBook&logoColor=FFFFFF)](https://matrix-construct.github.io/tuwunel/)
[![Demo Server](https://img.shields.io/badge/demo%20server%2D_?color=%238A2BE2&style=for-the-badge&logo=Element&logoColor=FFFFFF)](https://try.tuwunel.chat)
[![Support Chat](https://img.shields.io/matrix/tuwunel%3Amatrix.org.svg?color=098A09&style=for-the-badge&label=Support%20Chat&labelColor=8A2BE2&logo=Matrix)](https://matrix.to/#/#tuwunel:grin.hu)

Tuwunel is a featureful [Matrix](https://matrix.org/) homeserver you can use instead of Synapse
with your favorite [client](https://matrix.org/ecosystem/clients/),
[bridge](https://matrix.org/ecosystem/bridges/) or
[bot](https://matrix.org/ecosystem/integrations/). It is written entirely in Rust to be a scalable,
low-cost, enterprise-ready, community-driven alternative, fully implementing the
[Matrix Specification](https://spec.matrix.org/latest/) for all but the most niche uses.

This project is the official successor to [conduwuit](https://github.com/x86pup/conduwuit) after it
reached stability. Tuwunel is now used by many companies with a vested interest in its continued
development by full-time staff. It is primarily sponsored by the government of
Switzerland 🇨🇭 where it is currently deployed for citizens.

### Getting Started

- [GitHub Releases](https://github.com/matrix-construct/tuwunel/releases)
- [Sourcecode](https://github.com/matrix-construct/tuwunel/) `git clone https://github.com/matrix-construct/tuwunel.git`
- [DockerHub](https://hub.docker.com/r/jevolk/tuwunel) or `docker pull jevolk/tuwunel:latest`
- [GHCR](https://github.com/matrix-construct/tuwunel/pkgs/container/tuwunel) or `docker pull ghcr.io/matrix-construct/tuwunel:latest`
- Static binaries available as [releases](https://github.com/matrix-construct/tuwunel/releases) or [build artifacts](https://github.com/matrix-construct/tuwunel/actions?query=branch%3Amain).
- Deb and RPM packages available as [releases](https://github.com/matrix-construct/tuwunel/releases) or [build artifacts](https://github.com/matrix-construct/tuwunel/actions?query=branch%3Amain).
- Apt repository available for Debian and Ubuntu, see the [Debian guide](https://matrix-construct.github.io/tuwunel/deploying/debian.html) for setup.
- [COPR repository](https://copr.fedorainfracloud.org/coprs/trapacid/tuwunel/) available for Fedora, RHEL and other rpm distros, see the [Red Hat guide](https://matrix-construct.github.io/tuwunel/deploying/redhat.html) for setup.
- Arch packages available in the AUR as [tuwunel](https://aur.archlinux.org/packages/tuwunel) to build from source, or [tuwunel-bin](https://aur.archlinux.org/packages/tuwunel-bin) for the prebuilt binary.
- Nix package available as [`matrix-tuwunel`](https://search.nixos.org/packages?query=matrix-tuwunel) and NixOS module available as [`services.matrix-tuwunel`](https://search.nixos.org/options?query=services.matrix-tuwunel).
- Nix binary cache available at `https://cache.tuwunel.chat`, see the [NixOS guide](https://matrix-construct.github.io/tuwunel/deploying/nixos.html#binary-cache) for setup.
- Alpine package available as [tuwunel](https://pkgs.alpinelinux.org/package/edge/testing/x86_64/tuwunel).
- Gentoo ebuild available in Guru as [net-im/tuwunel](https://gitweb.gentoo.org/repo/proj/guru.git/tree/net-im/tuwunel).
- Ansible playbook available as [matrix-docker-ansible-deploy](https://github.com/spantaleev/matrix-docker-ansible-deploy/blob/master/docs/configuring-playbook-tuwunel.md).

### Quick Setup

**1.** [Configure](https://matrix-construct.github.io/tuwunel/configuration.html) by
copying and editing the `tuwunel-example.toml`. The `server_name` and `database_path` must be
configured. **Most users deploy via docker or a distribution package and should follow the
[appropriate guide](https://matrix-construct.github.io/tuwunel/deploying.html) instead.**
This is just a summary for the impatient. See the full
[documentation](https://matrix-construct.github.io/tuwunel/).

> [!TIP]
> Avoid using a sub-domain for your `server_name`. You can always delegate later with a [`.well-known`](https://matrix-construct.github.io/tuwunel/deploying/root-domain-delegation.html)
> file, but you can never change your `server_name`.

**2.** Setup TLS certificates. Most users enjoy the [Caddy](https://caddyserver.com/) reverse-proxy
which automates their certificate renewal. Advanced users can load their own TLS certificates
using the configuration and Tuwunel can be deployed without a reverse proxy. Example
`/etc/caddy/Caddyfile` configuration with [Element](https://github.com/element-hq/element-web/releases)
unzipped to `/var/www/element`:
```
tuwunel.me, tuwunel.me:8448 {
    reverse_proxy localhost:8008
}
web.tuwunel.me {
    root * /var/www/element/
    file_server
}
```
`caddy reload --config /etc/caddy/Caddyfile`

**3.** Start the server, connect your client and register your username. The first registration is
granted server admin.

> [!TIP]
> Configure a secret `registration_token` and set `allow_registration = true`

### Help & Support

Security vulnerabilities have their own reporting process; please see
[SECURITY.md](./SECURITY.md). If you are opposed to using github, or if private discussion is
required for any other reason, I would be happy to receive your DM at
[@jason:tuwunel.me](https://matrix.to/#/@jason:tuwunel.me). This will not be bothering me as it would
be my pleasure to help you when possible. As an emergency contact you can send an email to
jasonzemos@gmail.com.

##### 💜 Tuwunel Support Chat 💜

Our official chat is publicly accessible at
[#tuwunel:matrix.org](https://matrix.to/#/#tuwunel:matrix.org). Project staff are present there to
offer assistance, and everyone is welcome to ask questions or share their experience running
Tuwunel.

### Migrating to Tuwunel

| Can I migrate from... | [More about migrating your existing server](https://matrix-construct.github.io/tuwunel/migrating.html) |
|-----------------|-----------|
| conduwuit? | ✅ Yes. This will be supported at a minimum for one year, but likely indefinitely. |
| A fork of conduwuit? | ✅ Yes. The database migrates in place on first boot. |
| Conduit? | ✅ Yes. The RocksDB database migrates in place on first boot. |
| Synapse? | ❌ Not yet, but this is planned and an important issue. Subscribe to [#2](https://github.com/matrix-construct/tuwunel/issues/2). |
| Any other Conduit fork? | ❌ No. The migration must be explicitly listed in this table. |

## Tuwunel<sup>💕</sup>

Tuwunel's theme is **empathy** in communication defined by the works of
[Edith Stein](https://plato.stanford.edu/entries/stein/). Empathy is the basis for how we approach
every message and our responsibility to the other in every conversation.

<!-- ANCHOR_END: body -->

<!-- ANCHOR: footer -->

<!-- ANCHOR_END: footer -->
