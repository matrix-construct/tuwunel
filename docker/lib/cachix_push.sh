#!/bin/sh
# Uploads store paths and their build closure to the Nix binary cache.
#
# A transparent no-op unless cachix_push is 1 and CACHIX_AUTH_TOKEN is set, so
# every nix stage can call it unconditionally and forks, pull requests and
# local builds simply skip. A failed upload never fails the build that
# produced the paths: the artifact is still good, only the cache missed it.
#
# Run it with the working directory at the flake root. cachix comes from the
# flake's own inputs rather than an ambient channel, so the flake must resolve
# from the current directory.
#
# usage: cachix_push.sh <store-path-or-result-symlink> [...]

set -eu

if [ "${cachix_push:-0}" != "1" ] || [ -z "${CACHIX_AUTH_TOKEN:-}" ]; then
    exit 0
fi

cache="${cachix_cache:-tuwunel}"
paths="$(mktemp)"
trap 'rm -f "$paths"' EXIT

for target in "$@"; do
    drv="$(nix-store --query --deriver "$target" 2> /dev/null || true)"
    if [ -n "$drv" ] && [ -e "$drv" ]; then
        # Include build dependencies, so a later run substitutes the toolchain
        # and rocksdb instead of rebuilding them.
        nix-store --query --requisites --include-outputs "$drv" || true
    else
        nix-store --query --requisites "$target" || true
    fi
done | sort -u > "$paths"

if [ ! -s "$paths" ]; then
    echo "cachix_push: no store paths resolved, nothing to upload" >&2
    exit 0
fi

echo "cachix_push: uploading $(wc -l < "$paths") paths to ${cache}" >&2

nix \
    --extra-experimental-features nix-command \
    --extra-experimental-features flakes \
    shell --inputs-from . cachix \
    -c xargs -r -a "$paths" cachix push "$cache" || true
