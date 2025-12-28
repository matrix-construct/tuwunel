# Tuwunel<sup>💕</sup>

![GitHub License](https://img.shields.io/github/license/matrix-construct/tuwunel?style=flat%2Dsquare&color=%238A2BE2)
![GitHub Created At](https://img.shields.io/github/created-at/matrix-construct/tuwunel?style=flat%2Dsquare&color=%238A2BE2)
![GitHub Commit Activity](https://img.shields.io/github/commit-activity/m/matrix-construct/tuwunel?style=flat%2Dsquare&link=https%3A%2F%2Fgithub.com%2Fmatrix-construct%2Ftuwunel%2Fpulse%2Fmonthly&color=%238A2BE2)
![Docker Pulls](https://img.shields.io/docker/pulls/jevolk/tuwunel?style=flat%2Dsquare&color=8A2BE2)
![GitHub Repo Stars](https://img.shields.io/github/stars/matrix-construct/tuwunel?style=flat%2Dsquare&link=https%3A%2F%2Fgithub.com%2Fmatrix-construct%2Ftuwunel&color=%238A2BE2)
[![CI/CD](https://github.com/matrix-construct/tuwunel/actions/workflows/main.yml/badge.svg?branch=main&style=flat%2Dsquare)](https://github.com/matrix-construct/tuwunel/actions/workflows/main.yml)

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
- Arch package available as [tuwunel](https://aur.archlinux.org/packages/tuwunel) or [tuwunel-git](https://aur.archlinux.org/packages/tuwunel-git).
- Nix package available as [`matrix-tuwunel`](https://search.nixos.org/packages?query=matrix-tuwunel) and NixOS module available as [`services.matrix-tuwunel`](https://search.nixos.org/options?query=services.matrix-tuwunel).
- Alpine package available as [tuwunel](https://pkgs.alpinelinux.org/package/edge/testing/x86_64/tuwunel).

**1.** [Configure](https://matrix-construct.github.io/tuwunel/configuration.html) by
copying and editing the `tuwunel-example.toml`. The `server_name` and `database_path` must be
configured. **Most users deploy via docker or a distribution package and should follow the
[appropriate guide](https://matrix-construct.github.io/tuwunel/deploying.html) instead.**
This is just a summary for the impatient. See the full
[documentation](https://matrix-construct.github.io/tuwunel/).

> [!TIP]
> Avoid using a sub-domain for your `server_name`. You can always delegate later with a [`.well-known`](https://github.com/spantaleev/matrix-docker-ansible-deploy/blob/master/docs/configuring-well-known.md)
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

 🤗 Did you find this and other documentation helpful? We would love to hear feedback about setting
 up Tuwunel.


### Migrating to Tuwunel

| Can I migrate from | |
|-----------------|-----------|
| conduwuit? | ✅ Yes. This will be supported at a minimum for one year, but likely indefinitely. |
| Synapse? | ❌ Not yet, but this is planned and an important issue. Subscribe to [#2](https://github.com/matrix-construct/tuwunel/issues/2). |
| Conduit? | ❌ Not right now, but this is planned for the near future. Subscribe to [#41](https://github.com/matrix-construct/tuwunel/issues/41). |
| Any other fork of Conduit? | ❌ No. The migration must be explicitly listed in this table. |
> [!CAUTION]
> **Never switch between different forks of Conduit or you will corrupt your database.**
> All derivatives of Conduit share the same linear database version without any awareness of other
> forks. The database will permanently corrupt and we will not be able to help you.

#### Migrating from conduwuit

Migrating from conduwuit to Tuwunel _just works_. In technical parlance it is a "binary swap."
All you have to do is update to the latest Tuwunel and change the path to the executable from
`conduwuit` to `tuwunel`.

Anything else named "conduwuit" is still recognized, this includes environment variables with prefixes
such as `CONDUWUIT_`. In fact, `CONDUIT_` is still recognized for our legacy users. You may have
noticed that various configs, yamls, services, users, and other items were renamed, but if you
were a conduwuit user we recommend against changing anything at all. This will keep things simple.
If you are not sure please ask. If you found out that something did in fact need to be changed
please open an issue immediately.


### Upgrading & Downgrading Tuwunel

We strive to make moving between versions of Tuwunel safe and easy. Downgrading Tuwunel is always
safe but often prevented by a guard. An error will indicate the downgrade is not possible and a
newer version which does not error must be sought.

#### Branches

The main branch is always _reasonably safe_ to run. We understand the propensity for users to simply clone
the main branch to get up and running, and we're obliged to ensure it's always viable. Nevertheless, only
tagged releases are true releases.

#### Container Tracking

> [!IMPORTANT]
> **We strongly advise tracking the `:latest` tag when automatically updating.**

Tracking `:latest` gives us the necessary discretion to keep you on the appropriate stable version.
We discourage tracking the main branch unless frequent restarts are acceptable. Alternatively,
tracking the `:preview` tag provides the latest release-candidate becoming equivalent to `:latest`
after a release. Tracking the `:preview` tag is a worthy alternative to the main branch, with
turbulence limited to release-time.

### Getting Help & Support

If you are opposed to using github, or if private discussion is required such as for security
disclosures, or for any other reason, I would be happy to receive your DM at
[@jason:tuwunel.me](https://matrix.to/#/@jason:tuwunel.me). This will not be bothering me as it would
be my pleasure to help you when possible. As an emergency contact you can send an email to
jasonzemos@gmail.com.

##### Tuwunel Fanclub

We have an unofficial community-run chat which is publicly accessible at
[#tuwunel:matrix.org](https://matrix.to/#/#tuwunel:matrix.org). The members, content, or moderation
decisions of this room are not in any way related or endorsed by this project or its sponsors,
and not all project staff will be present there. There will be at least some presence by staff to
offer assistance so long as the room remains in minimally good standing.


## Tuwunel<sup>💕</sup>

Tuwunel's theme is **empathy** in communication defined by the works of
[Edith Stein](https://plato.stanford.edu/entries/stein/). Empathy is the basis for how we approach
every message and our responsibility to the other in every conversation.

<!-- ANCHOR_END: body -->

<!-- ANCHOR: footer -->

<!-- ANCHOR_END: footer -->
