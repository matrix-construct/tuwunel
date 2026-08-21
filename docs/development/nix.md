# Nix Binary Cache

Building Tuwunel from the flake compiles a pinned Rust toolchain, a patched
RocksDB, and liburing before it ever reaches Tuwunel's own crates. The binary
cache holds the results of that work so neither CI nor a contributor has to
repeat it.

Reading from the cache is public and needs no credentials. Writing to it needs
a token that only the main repository holds.

## Cache identity

| | Substituter | Public key | Provider |
|---|---|---|---|
| Self-hosted | `https://cache.tuwunel.chat` | `cache.tuwunel.chat-1:ZafUaXiRMozDa9N2SWim6EdzH0EEjWjwfvlTxXvcjLA=` | Attic behind a caching proxy |
| Cachix | `https://tuwunel.cachix.org` | `tuwunel.cachix.org-1:VRecUeDcaPxtYDA6bnMF3snPM7VYX8K605z4uuG2nWc=` | [Cachix](https://cachix.org) |

Both are public read, authenticated write, and both are configured everywhere so
they run side by side. They differ in what they hold: the self-hosted cache
stores the entire closure including stock nixpkgs paths, while Cachix skips
anything `cache.nixos.org` already serves.

Operators configuring a deployment should follow
[NixOS deployment](../deploying/nixos.md#binary-cache) instead; this page covers
how the cache is filled and maintained.

## Consuming

The flake declares the substituter in its `nixConfig`, so `nix build`,
`nix develop`, and `nix run` against this repository offer it automatically.
Nix applies a flake's `nixConfig` without asking only for accounts in
`trusted-users`; otherwise it prompts. Adding the two values to your own
`nix.conf`, or running `cachix use tuwunel`, avoids the prompt entirely.

A substituter that answers with a 5xx is worse than one that is down. Nix
retries and then **fails the build**, where an unreachable host or a 404 is
just a miss it falls through, so a cache that is unhealthy rather than absent
can break CI. `cache.tuwunel.chat` converts reverse-proxy backend failures to
404 for narinfo and NAR reads for exactly this reason. If some other
substituter ever fails this way, stop listing it until it recovers.

CI does not rely on `nixConfig`. The `nix-base` stage in
`docker/Dockerfile.nix` appends the substituter and key to `/etc/nix/nix.conf`
during image construction, because the stages that realise the tree through
`default.nix` get no effect from a flake's `nixConfig`. The values arrive as
the `nix_substituter` and `nix_public_key` build args, defaulted in
`docker/bake.hcl`.

## Populating

Three producers write to the cache. All are inert without a token, so forks and
pull requests degrade to read-only rather than failing.

| Producer | Trigger | Scope |
|---|---|---|
| `smoke-nix` stage in `docker/Dockerfile.nix` | Every branch push that runs the Smoke NixOS job | `all-features` plus its full build closure |
| `nix-pkg` stage in `docker/Dockerfile.nix` | Tags, `main` and `test`, where distro packaging is enabled | The default package plus its full build closure |
| `.github/workflows/nix.yml` | Version tags and manual dispatch | Every package and devShell the flake exposes for the runner's system |

The in-bake pushes are what keep CI fast: they upload build dependencies
alongside the output, so a later run substitutes the toolchain and RocksDB
instead of rebuilding them. All three call `docker/lib/nix_cache_push.sh`, which
is installed into every layer by `docker/Dockerfile.system` next to
`sched_wrap.sh` and stays a no-op until a token is mounted. A failed upload
never fails the build that produced the paths.

Note which stage runs when. `build-nix` is not a target any workflow invokes,
and `nix-pkg` only runs where `is_fat` holds, so `smoke-nix` is the sole
producer on an ordinary branch push. A push added to `build-nix` alone would
never execute in CI, because bake's `inherits` copies target attributes rather
than creating a stage dependency, and both other stages derive from
`nix-base`.

`nix.yml` is the publishing path. It enumerates attributes with
`nix flake show` rather than hardcoding a list, so outputs added to the flake
are published without editing the workflow. Uploads come from the post-build
hook installed by `cachix-action`, which captures every path realised during
the job.

Unlike the bake targets, it runs Nix directly on the runner rather than inside
a container, so it depends on the runner's own installation. The
`.github/actions/install-nix` action absorbs the two ways that differs from a
hosted runner: it reuses an existing Nix rather than installing over one, and
it points `build-dir` at the runner's temp directory. That second part is not
optional on the self-hosted pool. Nix 2.30 moved build directories under
`/nix/var/nix/builds`, which is root-owned there while the rest of the store
belongs to the runner user, so every build fails with a permission error while
evaluation keeps working and hides the cause.

Nix builds run the unit tests but not the integration ones. The targets under
`src/main/tests` each boot a server, and a Nix builder has no network, denies
`io_uring`, and offers no resolver configuration, so they cannot run there. The
`unit` and `integ` CI jobs cover them with those things available. What the Nix
check phase is for is confirming that the nixpkgs-linked build of our own crates
works at all, and the unit tests do that.

A full pass is expensive. The flake currently exposes 54 packages per system,
including cross-compiled static binaries, OCI images, and debug variants, and
each matrix entry is its own job. Use the `attrs` dispatch input to publish a
subset, and `max-parallel` to bound how much of the runner pool a run takes.

The producers upload build closures rather than only final outputs, and the two
caches then keep different amounts of that. Cachix skips any path already served
by `cache.nixos.org`, so it holds only the Tuwunel-specific subset: the first
`all-features` upload offered 3988 paths and stored 2158. The self-hosted cache
keeps everything, stock nixpkgs paths included, which is deliberate: a complete
closure means a build can be satisfied from `cache.tuwunel.chat` alone without
`cache.nixos.org` being reachable.

## Credentials

Each cache has its own write token, and each uploader is gated on its own, so
either may be absent without disturbing the other.

| Secret | Cache | Also needs |
|---|---|---|
| `CACHIX_AUTH_TOKEN` | Cachix | nothing |
| `ATTIC_TOKEN` | self-hosted | `ATTIC_ENDPOINT`, defaulted to `https://cache.tuwunel.chat` |

`ATTIC_ENDPOINT` is the Attic server the client logs into, which is not the
substituter URL even though the two share a host. The bake path sets it from the
`attic_endpoint` build arg in `nix-base`, so derived stages inherit it.

For `nix.yml` the secrets are read directly. For the bake path they are threaded
explicitly, because reusable workflows do not inherit secrets:

```text
main.yml  ->  test.yml     ->  bake.yml  ->  docker/bake.sh  ->  docker/bake.hcl
          ->  package.yml  ->
```

`bake.sh` never passes a token as a build argument. It only sets `cachix_push`
and `attic_push` from whether each token is present in its environment; the
tokens themselves reach the build as BuildKit secret mounts, declared once on
the `build-nix` bake target and inherited by `nix` and `smoke-nix`, then read
from `/run/secrets` inside each stage.

Both `cachix_push` and `attic_push` deliberately participate in the layer cache
key. Without them, a tokenless build could populate the cache entry for that
layer and suppress the upload on the next tokened build of the same tree.

## Pushing by hand

`nix/pkgs/complement/bin/nix-build-and-cache` builds an installable and
uploads it:

```bash
export CACHIX_AUTH_TOKEN=...
nix/pkgs/complement/bin/nix-build-and-cache just .#all-features
```

`just` builds one installable, `packages` builds everything the flake exposes,
and `ci` builds the tooling CI needs. Set `CACHIX_CACHE` to target a cache
other than `tuwunel`.

The script also carries an [Attic](https://github.com/zhaofengli/attic)
uploader for the self-hosted cache. It stays dormant unless both `ATTIC_TOKEN`
and `ATTIC_ENDPOINT` are set, and it defaults to no endpoint.

## Where the substituter values live

`cache.tuwunel.chat` is live and configured alongside Cachix. Both substituters
and both keys appear in exactly these places, so adding or retiring one is a
known edit:

| File | Purpose |
|---|---|
| `flake.nix` | `nixConfig` offered to anyone building the flake |
| `docker/bake.hcl` | `nix_substituter` and `nix_public_key` defaults for CI |
| `.github/workflows/nix.yml` | `NIX_CONFIG` for the publishing workflow |
| `docs/deploying/nixos.md` | Operator instructions |
| This page | Contributor reference |

`docker/Dockerfile.nix` carries the same values as `ARG` defaults, which the
bake variables override, so a CI-only change needs no Dockerfile edit. Both
`extra-substituters` and `extra-trusted-public-keys` are space-separated lists
in `nix.conf`, so a single bake variable holds both entries and no list plumbing
is required. Nix queries substituters in priority order and falls through on a
miss, so retiring Cachix later is a matter of deleting its entries.
