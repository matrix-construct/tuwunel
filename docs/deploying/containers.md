# Containers

Tuwunel ships as a small, statically linked OCI image that runs unprivileged
on any container runtime. Pick a deployment style based on how you already
manage services on the host.

- [**Docker**](docker.md). Pull prebuilt images from [GHCR][gh] or
  [Docker Hub][dh], then run standalone or via one of the provided
  [`docker-compose`](docker.md#docker-compose) stacks (with Caddy, Traefik,
  or a bring-your-own reverse proxy).

- [**Kubernetes**](kubernetes.md). Community-maintained Helm chart for
  cluster deployments. Tuwunel itself does not scale horizontally, so the
  chart runs a single replica with persistent storage.

- [**Podman with Quadlets**](podman-systemd.md). Rootless deployment managed
  by systemd user units, suited to single-host setups where containers should
  behave like native services.

Whichever style you pick, [**security profiles and limits**](container-security.md)
covers the two runtime defaults that affect this server: the task limit that
can stop startup outright, and the seccomp profile that decides whether the
database gets io_uring.

## Image registries

| Registry        | Image                                             | Tags                          |
| --------------- | ------------------------------------------------- | ----------------------------- |
| GitHub Registry | [`ghcr.io/matrix-construct/tuwunel`][gh]          | `latest`, `preview`, `main`   |
| Docker Hub      | [`docker.io/jevolk/tuwunel`][dh]                  | `latest`, `preview`, `main`   |

Three rolling tags trade update frequency for confidence.

| Tag        | Source                                  | Cadence       | Use when                                           |
| ---------- | --------------------------------------- | ------------- | -------------------------------------------------- |
| `:latest`  | Most recent tagged release              | ~monthly      | Production. Default choice.                        |
| `:preview` | Selected higher-confidence updates      | ~weekly       | You want fixes between releases without chasing `main`. |
| `:main`    | Every reviewed merge to the main branch | ~daily        | You track development and accept unknown-risk changes. |

**For automated updates we strongly advise tracking `:latest`.**

[gh]: https://github.com/matrix-construct/tuwunel/pkgs/container/tuwunel
[dh]: https://hub.docker.com/r/jevolk/tuwunel
