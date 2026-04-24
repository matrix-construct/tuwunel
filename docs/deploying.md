# Deploying into service

**Read the [Generic guide](deploying/generic.md) first regardless of which
platform you ultimately use.** It explains the universal requirements —
binary selection, database configuration, the system user, and the systemd
unit — that every other guide builds on.

## Choosing a reverse proxy

Tuwunel listens on plain HTTP and requires a reverse proxy to terminate TLS.
All guides assume HTTPS is already handled externally.

**[Caddy](deploying/reverse-proxy-caddy.md)** is the recommended choice. It
handles TLS automatically via Let's Encrypt, and the full Tuwunel configuration
fits in two lines. It also proxies port 8448 (Matrix federation) in the same
block with no extra work.

**[Nginx](deploying/reverse-proxy-nginx.md)** is a solid choice if you are
already running it. The configuration is more verbose but well-documented. Set
`client_max_body_size` to match or exceed Tuwunel's `max_request_size` (default
20 MiB), and never use `$request_uri` in the proxy pass — it causes subtle
breakage.

> [!WARNING]
> Apache and Lighttpd are **not supported** for Matrix federation. Both alter
> the `X-Matrix` authorization header that federation depends on. Nginx and
> Caddy handle it correctly.

**[Traefik](deploying/reverse-proxy-traefik.md)** works well in Docker
environments. Note that Traefik cannot serve `.well-known` files by itself — you
need a companion nginx container for federation discovery, or expose port 8448
directly as a Traefik entrypoint. Also check whether you are running a version
between 3.6.4–3.6.6 or 2.11.32–2.11.34, which contain a bug that requires an
extra workaround flag.

## Root-domain delegation

By default, Tuwunel's server name is the domain that appears in Matrix user IDs
(`@alice:example.com`), which must exactly match the host Tuwunel presents when
federating. If you want to host Matrix under a subdomain (`matrix.example.com`)
while users have addresses on the root domain (`example.com`), configure
**[root-domain delegation](deploying/root-domain-delegation.md)**. This serves
`.well-known/matrix/client` and `.well-known/matrix/server` from the root domain
pointing to the subdomain, and requires no changes to DNS beyond what is already
needed for your web server.

## Things to know before getting started

**Pick the right binary.** Prebuilt binaries for `x86_64` come in `-v1-`, `-v2-`,
and `-v3-` CPU-optimized variants. Running the wrong one produces an
`Illegal Instruction` crash. The generic guide includes a command to check which
variant your CPU supports; `-v2-` or better is recommended for RocksDB's CRC32
performance.

**RocksDB is the only supported database.** SQLite support has been removed. If
you are migrating from Conduit, you will need a migration tool before deploying
Tuwunel.

**Port 8448 matters for federation.** Clients connect on port 443, but other
Matrix homeservers connect on port 8448. Both must be reachable for a fully
functional server. NAT hairpinning or split-horizon DNS may be needed for
internal clients that need to reach the same domain.

**Container images are minimal.** Docker and Podman images contain only the
binary, a minimal init (tini), and CA certificates — no shell. If you need to
inspect a running container you will need to `exec` into it using the binary
directly.

**Rootless Podman requires linger.** Without `loginctl enable-linger`, rootless
containers stop when the user logs out. The [Podman guide](deploying/podman-systemd.md)
uses quadlet files and user-level systemd to handle this correctly.

**NixOS has platform-specific workarounds.** The community `services.matrix-conduit`
NixOS module defaults to SQLite, requires a manual workaround for UNIX socket
support, and conflicts with jemalloc when a hardened profile is enabled. The
[NixOS guide](deploying/nixos.md) covers all three.
