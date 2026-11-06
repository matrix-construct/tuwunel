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

### Registries

| Registry        | Image                                             | Tags                          |
| --------------- | ------------------------------------------------- | ----------------------------- |
| GitHub Registry | [`ghcr.io/matrix-construct/tuwunel`][gh]          | `latest`, `preview`, `main`   |
| Docker Hub      | [`docker.io/jevolk/tuwunel`][dh]                  | `latest`, `preview`, `main`   |

### Images

Three rolling tags trade update frequency for confidence.

| Tag        | Source                                  | Cadence       | Use when                                           |
| ---------- | --------------------------------------- | ------------- | -------------------------------------------------- |
| `:latest`  | Most recent tagged release              | ~monthly      | Production. Default choice.                        |
| `:preview` | Selected higher-confidence updates      | ~weekly       | You want fixes between releases without chasing `main`. |
| `:main`    | Every reviewed merge to the main branch | ~daily        | You track development and accept unknown-risk changes. |

> [!IMPORTANT]
> **We strongly advise tracking the `:latest` tag when automatically updating.**

Tracking `:latest` gives us the necessary discretion to keep you on the appropriate stable release
version. Tracking the `:preview` tag provides select updates of higher confidence between releases.
Tracking the `:main` branch provides the most frequent updates which have been reviewed and tested
with confidence for release, the only remaining risk being the unknown. The publication frequency
for these tags are on average monthly, weekly and daily, respectively.

[gh]: https://github.com/matrix-construct/tuwunel/pkgs/container/tuwunel
[dh]: https://hub.docker.com/r/jevolk/tuwunel
