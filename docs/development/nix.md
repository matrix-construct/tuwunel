# Nix Binary Cache

Building Tuwunel from the flake compiles a pinned Rust toolchain, a patched
RocksDB, and liburing before it ever reaches Tuwunel's own crates. The binary
cache holds the results of that work so neither CI nor a contributor has to
repeat it.

Reading from the cache is public and needs no credentials. Writing to it needs
a token that only the main repository holds.

## Cache identity

| | |
|---|---|
| Substituter | `https://tuwunel.cachix.org` |
| Public key | `tuwunel.cachix.org-1:VRecUeDcaPxtYDA6bnMF3snPM7VYX8K605z4uuG2nWc=` |
| Provider | [Cachix](https://cachix.org) |
| Visibility | Public read, authenticated write |

Operators configuring a deployment should follow
[NixOS deployment](../deploying/nixos.md#binary-cache) instead; this page covers
how the cache is filled and maintained.

## Consuming

The flake declares the substituter in its `nixConfig`, so `nix build`,
`nix develop`, and `nix run` against this repository offer it automatically.
Nix applies a flake's `nixConfig` without asking only for accounts in
`trusted-users`; otherwise it prompts. Adding the two values to your own
`nix.conf`, or running `cachix use tuwunel`, avoids the prompt entirely.

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
instead of rebuilding them. All three call `docker/lib/cachix_push.sh`, which
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

A full pass is expensive. The flake currently exposes 54 packages per system,
including cross-compiled static binaries, OCI images, and debug variants, and
each matrix entry is its own job. Use the `attrs` dispatch input to publish a
subset, and `max-parallel` to bound how much of the runner pool a run takes.

The producers upload build closures rather than only final outputs, but cachix
skips any path already served by `cache.nixos.org`, so what actually lands is
the Tuwunel-specific subset: our binaries and our forked dependencies. The
first `all-features` upload offered 3988 paths and stored 2158 of them, and a
closure still resolves completely because Nix queries both substituters. Stock
nixpkgs dependencies stay upstream and are never duplicated here.

## Credentials

Pushing requires a token with write access to the cache, generated from the
Cachix dashboard and stored as the repository secret `CACHIX_AUTH_TOKEN`.

For `nix.yml` the secret is read directly. For the bake path it is threaded
explicitly, because reusable workflows do not inherit secrets:

```text
main.yml  ->  test.yml     ->  bake.yml  ->  docker/bake.sh  ->  docker/bake.hcl
          ->  package.yml  ->
```

`bake.sh` never passes the token as a build argument. It only sets
`cachix_push` when the token is present in its environment; the token itself
reaches the build as a BuildKit secret mount, declared once on the `build-nix`
bake target and inherited by `nix` and `smoke-nix`, then read from
`/run/secrets` inside each stage.

`cachix_push` deliberately participates in the layer cache key. Without it, a
tokenless build could populate the cache entry for that layer and suppress the
upload on the next tokened build of the same tree.

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

The script also retains an [Attic](https://github.com/zhaofengli/attic)
uploader for the planned self-hosted cache. It stays dormant unless both
`ATTIC_TOKEN` and `ATTIC_ENDPOINT` are set, and it no longer defaults to any
endpoint.

## Moving to a self-hosted cache

A self-hosted cache at `cache.tuwunel.chat` is planned. The substituter URL and
public key appear in exactly these places:

| File | Purpose |
|---|---|
| `flake.nix` | `nixConfig` offered to anyone building the flake |
| `docker/bake.hcl` | `nix_substituter` and `nix_public_key` defaults for CI |
| `.github/workflows/nix.yml` | `NIX_CONFIG` for the publishing workflow |
| `docs/deploying/nixos.md` | Operator instructions |
| This page | Contributor reference |

`docker/Dockerfile.nix` carries the same values as `ARG` defaults, which the
bake variables override, so a CI-only switch needs no Dockerfile edit. Serving
both caches during a transition means listing both substituters and both keys;
Nix queries them in priority order and falls through on a miss.
