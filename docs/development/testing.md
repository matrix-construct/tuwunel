# Testing and Delivery

Tuwunel's CI is built on [Docker Buildx Bake](https://docs.docker.com/build/bake/),
a recipe tree for multi-stage builds that produces cache-friendly multi-layer
images. Each layer accomplishes a specific task and its outputs are automatically
reused by any subsequent layer that depends on it. The result is a large
combinatorial matrix of build configurations where only the final layer — the
one that actually differs — must be rebuilt.

The `.github/workflows/` directory contains the pipeline *description* actually
used for CI. It is a thin client: switches and patch panels that dispatch to the
docker builder, not the mainframe itself. Every job is a path to an invocation of
`docker buildx bake`. The same builder, with the same targets, is available to
any developer locally — no service lockin required.

All scripts, Dockerfiles, and the bake configuration live in `docker/` at the
project root.

## Delivery Pipeline

The pipeline is organized into four sequential phases that gate on each other.
A failure in any phase blocks all later ones, preventing partially-built
deliverables from reaching users.

| Phase | Access | Description |
|---|---|---|
| **[Lint](testing/pipeline.md#linting-phase)** | everything (unless masked) | format, spelling, security audit, dead links, clippy |
| **[Test](testing/pipeline.md#testing-phase)** | everything (unless masked) | unit, integration, smoke, Complement, Matrix SDK |
| **[Package](testing/pipeline.md#package-phase)** | main, test, releases, PRs (limited) | binaries, containers, distro packages, docs |
| **[Publish](testing/pipeline.md#publish-phase)** | main and tagged releases only  | container registries, GitHub Pages |

## Chapters

- [Docker Builder](testing/bake.md) — `bake.hcl`, target hierarchy, layer caching, `bake.sh`
- [Matrix Selectors](testing/matrix.md) — cargo profiles, feature sets, toolchains, CPU targets
- [Pipeline Phases](testing/pipeline.md) — stages of the pipeline required for delivery
- [Complement Testing](testing/complement.md) — protocol compliance testing, local usage
