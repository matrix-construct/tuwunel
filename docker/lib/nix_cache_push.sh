#!/bin/sh
# Uploads store paths and their build closure to the Nix binary caches.
#
# Each uploader is independent and gated on its own token, so a stage can call
# this unconditionally: forks, pull requests and local builds simply skip. A
# failed upload never fails the build that produced the paths, because the
# artifact is still good and only the cache missed it.
#
# Run it with the working directory at the flake root. Both clients come from
# the flake's own inputs rather than an ambient channel, so the flake must
# resolve from the current directory.
#
# usage: nix_cache_push.sh <store-path-or-result-symlink> [...]

set -eu

nix_flags="--extra-experimental-features nix-command --extra-experimental-features flakes"

want_cachix=0
if [ "${cachix_push:-0}" = "1" ] && [ -n "${CACHIX_AUTH_TOKEN:-}" ]; then
    want_cachix=1
fi

want_attic=0
if [ "${attic_push:-0}" = "1" ] && [ -n "${ATTIC_TOKEN:-}" ] && [ -n "${ATTIC_ENDPOINT:-}" ]; then
    want_attic=1
fi

if [ "$want_cachix" = "0" ] && [ "$want_attic" = "0" ]; then
    exit 0
fi

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
    echo "nix_cache_push: no store paths resolved, nothing to upload" >&2
    exit 0
fi

count="$(wc -l < "$paths")"

if [ "$want_cachix" = "1" ]; then
    cache="${cachix_cache:-tuwunel}"
    echo "nix_cache_push: uploading ${count} paths to cachix ${cache}" >&2

    # shellcheck disable=SC2086
    nix $nix_flags shell --inputs-from . cachix \
        -c xargs -r -a "$paths" cachix push "$cache" || true
fi

if [ "$want_attic" = "1" ]; then
    ATTIC_CACHE="${attic_cache:-tuwunel}"
    ATTIC_PATHS="$paths"
    export ATTIC_CACHE ATTIC_PATHS
    echo "nix_cache_push: uploading ${count} paths to attic ${ATTIC_CACHE}" >&2

    # login writes a config the subsequent push reads. Both run inside one
    # shell so the token stays in this process tree and never reaches a
    # command line the caller traces.
    # shellcheck disable=SC2086
    nix $nix_flags shell --inputs-from . attic -c sh -c '
        attic login "$ATTIC_CACHE" "$ATTIC_ENDPOINT" "$ATTIC_TOKEN" > /dev/null &&
        xargs -r -a "$ATTIC_PATHS" attic push "$ATTIC_CACHE"
    ' || true
fi
