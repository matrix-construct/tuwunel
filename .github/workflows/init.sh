#!/bin/bash

set +e

builder="${GITHUB_ACTOR}"
seed_builder="${seed_builder:-jevolk}"

# Commit-message (or workflow_dispatch) directives controlling the per-actor
# buildx builder, so the runner cache can be reset without an ssh trip:
#   [ci clean]          discard the builder so it is recreated from scratch,
#                       picking up the current nightly toolchain and a fresh
#                       buildkit.
#   [ci clean nocache]  ...and recreate cold, skipping the seed-from-seed_builder
#                       step below.
#   [ci clean-rust]     refresh the rust toolchain. A rust-and-above prune that
#                       keeps the base system is not matchable by buildx, so for
#                       now this settles for the same cold rebuild as nocache.
clean=
nocache=
case "$pipeline" in
*"[ci clean nocache]"*) clean=1; nocache=1 ;;
*"[ci clean-rust]"*)    clean=1; nocache=1 ;;
*"[ci clean]"*)         clean=1 ;;
esac

if test -n "$clean"; then
	docker buildx rm "$builder"
fi

docker buildx inspect "$builder"
if test x"$?" = x"0"; then
	exit 0
fi

set -eux

reserved_space=$(echo -n "$reserved_space" | jq -r ".$runner")
max_used_space=$(echo -n "$max_used_space" | jq -r ".$runner")
min_free_space=$(echo -n "$min_free_space" | jq -r ".$runner")
safety_free_space=$(echo -n "$safety_free_space" | jq -r ".$runner")

cat <<EOF > ./buildkitd.toml
[system]
  platformsCacheMaxAge = "504h"
[worker.oci]
  enabled = true
  rootless = false
  gc = true
  reservedSpace = "${reserved_space}"
  maxUsedSpace = "${max_used_space}"
  minFreeSpace = "${min_free_space}"

# Dependency cache mounts: cargo registry, cargo git, rustup downloads,
# go module cache. Cheap to keep, expensive to refetch. Held by age alone
# (evicted only when untouched for two weeks). No size cap: a policy
# maxUsedSpace is a target for the whole cache rather than this bucket, and
# these total only a few GB. keepDuration tracks lastUsedAt, so caches that
# stay in active use never age out.
[[worker.oci.gcpolicy]]
  filters = ["type==exec.cachemount"]
  keepDuration = "336h"

# Everything else: build layers, sources, frontend. reservedSpace is the
# warm floor GC never prunes below, so the most-recently-used reservedSpace
# of layers (about one full matrix run of foundation and cooked deps) always
# survives even across a multi-day gap. maxUsedSpace is the ceiling GC trims
# the total back to; keepDuration shields anything used in the last few days.
[[worker.oci.gcpolicy]]
  filters = ["type!=exec.cachemount"]
  keepDuration = "72h"
  reservedSpace = "${reserved_space}"
  maxUsedSpace = "${max_used_space}"
  all = true

# Safety floor: under critical disk pressure, evict anything regardless of
# age or type. Last relief valve when the ceiling above has not sufficed.
[[worker.oci.gcpolicy]]
  minFreeSpace = "${safety_free_space}"
  all = true
EOF

# Seed a brand-new builder from seed_builder's cache so it starts warm instead
# of from scratch; buildkit reuses the layers that match and rebuilds the rest
# per the new actor's needs. The buildkit state lives in a docker volume named
# for the builder, so the seed is a volume copy done before bootstrap. When the
# seed builder is absent (its state volume does not exist), or for the seed
# builder itself, [ci clean nocache], and [ci clean-rust], this is skipped and
# the builder is cold-created.
seed_state="buildx_buildkit_${seed_builder}0_state"
this_state="buildx_buildkit_${builder}0_state"
seeded=
if test -z "$nocache" \
	&& test "$builder" != "$seed_builder" \
	&& docker volume inspect "$seed_state" >/dev/null 2>&1
then
	docker volume create "$this_state"

	# The seed source is a live builder whose GC unlinks snapshots while the copy
	# runs, so cp reports vanished-file errors and exits non-zero even when the
	# result is a usable warm cache. Keep whatever copied and let the bootstrap
	# below arbitrate: a cache too torn to open is discarded and rebuilt cold by
	# the fallback, while a few missing snapshots are just cache misses.
	docker run --rm \
		-v "${seed_state}:/seed:ro" \
		-v "${this_state}:/state" \
		busybox sh -c 'cp -a /seed/. /state/' || true
	seeded=1
fi

create_builder() {
	docker buildx create \
		--bootstrap \
		--driver docker-container \
		--buildkitd-config ./buildkitd.toml \
		--name "$builder" \
		--buildkitd-flags "--allow-insecure-entitlement network.host"
}

# A seed copied from a live builder can carry a torn cache.db; if bootstrap
# rejects it, discard the seed and cold-start so a build is never blocked.
if ! create_builder; then
	if test -n "$seeded"; then
		docker buildx rm "$builder" || true
		docker volume rm -f "$this_state" || true
		create_builder
	else
		exit 1
	fi
fi
