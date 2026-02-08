# tuwunel for Docker

## Docker

To run tuwunel with Docker you can either build the image yourself or pull it
from a registry.

### Use a registry

OCI images for tuwunel are available in the registries listed below.

| Registry        | Image                                                           | Size                          | Notes                  |
| --------------- | --------------------------------------------------------------- | ----------------------------- | ---------------------- |
| GitHub Registry | [ghcr.io/matrix-construct/tuwunel:latest][gh] | ![Image Size][shield-latest]  | Stable latest tagged image.          |
| Docker Hub      | [docker.io/jevolk/tuwunel:latest][dh]             | ![Image Size][shield-latest]  | Stable latest tagged image.          |
| GitHub Registry | [ghcr.io/matrix-construct/tuwunel:main][gh]   | ![Image Size][shield-main]    | Stable main branch.   |
| Docker Hub      | [docker.io/jevolk/tuwunel:main][dh]               | ![Image Size][shield-main]    | Stable main branch.   |

[dh]: https://hub.docker.com/r/jevolk/tuwunel
[gh]: https://github.com/matrix-construct/tuwunel/pkgs/container/tuwunel
[shield-latest]: https://img.shields.io/docker/image-size/jevolk/tuwunel/latest
[shield-main]: https://img.shields.io/docker/image-size/jevolk/tuwunel/main

### Run

When you have the image you can simply run it with

```bash
docker run -d -p 8448:8008 \
    -v db:/var/lib/tuwunel/ \
    -e TUWUNEL_SERVER_NAME="your.server.name" \
    -e TUWUNEL_ALLOW_REGISTRATION=false \
    --name tuwunel $LINK
```

or you can use [docker compose](#docker-compose).

The `-d` flag lets the container run in detached mode. You may supply an
optional `tuwunel.toml` config file, the example config can be found
[here](../configuration/examples.md). You can pass in different env vars to
change config values on the fly. You can even configure tuwunel completely by
using env vars. For an overview of possible values, please take a look at the
[`docker-compose.yml`](docker-compose.yml) file.

If you just want to test tuwunel for a short time, you can use the `--rm`
flag, which will clean up everything related to your container after you stop
it.

### Docker-compose

If the `docker run` command is not for you or your setup, you can also use one
of the provided `docker-compose` files.

Depending on your proxy setup, you can use one of the following files;

- If you already have a `traefik` instance set up, use
[`docker-compose.for-traefik.yml`](docker-compose.for-traefik.yml)
- If you don't have a `traefik` instance set up and would like to use it, use
[`docker-compose.with-traefik.yml`](docker-compose.with-traefik.yml)
- If you want a setup that works out of the box with `caddy-docker-proxy`, use
[`docker-compose.with-caddy.yml`](docker-compose.with-caddy.yml) and replace all
`example.com` placeholders with your own domain
- For any other reverse proxy, use [`docker-compose.yml`](docker-compose.yml)

When picking the traefik-related compose file, rename it so it matches
`docker-compose.yml`, and rename the override file to
`docker-compose.override.yml`. Edit the latter with the values you want for your
server.

When picking the `caddy-docker-proxy` compose file, it's important to first
create the `caddy` network before spinning up the containers:

```bash
docker network create caddy
```

After that, you can rename it so it matches `docker-compose.yml` and spin up the
containers!

Additional info about deploying tuwunel can be found [here](generic.md).

### Run

If you already have built the image or want to use one from the registries, you
can just start the container and everything else in the compose file in detached
mode with:

```bash
docker compose up -d
```

> **Note:** Don't forget to modify and adjust the compose file to your needs.

### Nix build

Tuwunel's Nix images are built using [`buildLayeredImage`][nix-buildlayeredimage].
This ensures all OCI images are repeatable and reproducible by anyone, keeps the
images lightweight, and can be built offline.

This also ensures portability of our images because `buildLayeredImage` builds
OCI images, not Docker images, and works with other container software.

The OCI images are OS-less with only a very minimal environment of the `tini`
init system, CA certificates, and the tuwunel binary. This does mean there is
not a shell, but in theory you can get a shell by adding the necessary layers
to the layered image. However it's very unlikely you will need a shell for any
real troubleshooting.

The flake file for the OCI image definition is at [`nix/pkgs/oci-image/default.nix`][oci-image-def].

To build an OCI image using Nix, the following outputs can be built:
- `nix build -L .#oci-image` (default features, x86_64 glibc)
- `nix build -L .#oci-image-x86_64-linux-musl` (default features, x86_64 musl)
- `nix build -L .#oci-image-aarch64-linux-musl` (default features, aarch64 musl)
- `nix build -L .#oci-image-x86_64-linux-musl-all-features` (all features, x86_64 musl)
- `nix build -L .#oci-image-aarch64-linux-musl-all-features` (all features, aarch64 musl)

### Use Traefik as Proxy

As a container user, you probably know about Traefik. It is a easy to use
reverse proxy for making containerized app and services available through the
web. With the two provided files,
[`docker-compose.for-traefik.yml`](docker-compose.for-traefik.yml) (or
[`docker-compose.with-traefik.yml`](docker-compose.with-traefik.yml)) and
[`docker-compose.override.yml`](docker-compose.override.yml), it is equally easy
to deploy and use tuwunel, with a little caveat. If you already took a look at
the files, then you should have seen the `well-known` service, and that is the
little caveat. Traefik is simply a proxy and loadbalancer and is not able to
serve any kind of content, but for tuwunel to federate, we need to either
expose ports `443` and `8448` or serve two endpoints `.well-known/matrix/client`
and `.well-known/matrix/server`.

With the service `well-known` we use a single `nginx` container that will serve
those two files.

## Voice communication

See the [TURN](../turn.md) page.

[nix-buildlayeredimage]: https://ryantm.github.io/nixpkgs/builders/images/dockertools/#ssec-pkgs-dockerTools-buildLayeredImage
[oci-image-def]: https://github.com/jevolk/tuwunel/blob/main/nix/pkgs/oci-image/default.nix
