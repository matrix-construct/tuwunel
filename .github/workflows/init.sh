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

# Small helpers shared by the reaper and the seed guard.
#
# Free space is read against the docker data root: every per-actor builder
# volume lives there, so this is the number the whole scheme must protect.
# gib() parses a "###GB"/"###" knob into bytes (treated as GiB) for comparison
# against df. builders_with_state() lists every docker-container builder by its
# state volume, which excludes the built-in "default" builder automatically.
avail_bytes() {
	df -PB1 /var/lib/docker | awk 'NR==2 {print $4}'
}

gib() {
	echo $(( ${1%%[!0-9]*} * 1024 * 1024 * 1024 ))
}

builders_with_state() {
	docker volume ls -q 2>/dev/null \
	| sed -n 's/^buildx_buildkit_\(.*\)0_state$/\1/p'
}

# Per-builder last-seen registry. One empty marker file per builder in a small
# docker volume, touched here on every run and stat'd by the reaper. docker
# mediates access, so no host path or sudo is needed and it survives across
# jobs. Marking the current actor first is what keeps the reaper from ever
# evicting a builder whose actor is actively pushing.
seen_vol="tuwunel_ci_seen"
docker volume create "$seen_vol" >/dev/null 2>&1

mark_seen() {
	docker run --rm -v "${seen_vol}:/seen" busybox \
		touch "/seen/$1" >/dev/null 2>&1
}

mark_seen "$builder"

# Runner-keyed knobs (JSON maps of runner -> value, selected by $runner).
reserved_space=$(echo -n "$reserved_space" | jq -r ".$runner")
max_used_space=$(echo -n "$max_used_space" | jq -r ".$runner")
cachemount_max=$(echo -n "$cachemount_max" | jq -r ".$runner")
min_free_space=$(echo -n "$min_free_space" | jq -r ".$runner")
safety_free_space=$(echo -n "$safety_free_space" | jq -r ".$runner")
reap_idle_hours=$(echo -n "$reap_idle_hours" | jq -r ".$runner")
reap_min_free=$(echo -n "$reap_min_free" | jq -r ".$runner")
seed_budget=$(echo -n "$seed_budget" | jq -r ".$runner")

# Reaper. Per-actor builders accumulate one full-fat state volume each and
# nothing else removes them, so without this a handful of stale contributors
# fill the disk on their own. buildkit's own GC is per-builder and blind to
# the others, so this is the only cross-builder coordinator. It runs on every
# init, before the early-exit, so stale builders are swept even on the common
# path where the current actor's builder already exists.
#
# The seed and the current actor are never touched. A builder is reaped when
# its actor has not run for reap_idle_hours (its last run began that long ago,
# so it has certainly finished); under this age the builder may still be mid
# build for a concurrent run and is left alone.
reap_builder() {
	docker buildx rm "$1" >/dev/null 2>&1
	docker volume rm -f "buildx_buildkit_${1}0_state" >/dev/null 2>&1
	docker run --rm -v "${seen_vol}:/seen" busybox \
		rm -f "/seen/$1" >/dev/null 2>&1

	echo "reaped idle builder: $1"
}

now=$(date +%s)
reap_idle_secs=$(( reap_idle_hours * 3600 ))
markers=$(docker run --rm -v "${seen_vol}:/seen" busybox sh -c '
	cd /seen 2>/dev/null || exit 0
	for f in *; do
		[ -e "$f" ] &&
			echo "$(stat -c %Y "$f") $f"
	done
' 2>/dev/null)

marker_epoch() {
	echo "$markers" | awk -v n="$1" '$2 == n {print $1; exit}'
}

for name in $(builders_with_state); do
	test "$name" = "$seed_builder" && continue
	test "$name" = "$builder" && continue

	epoch=$(marker_epoch "$name")
	if test -z "$epoch"; then
		# First sighting of a pre-existing builder: grant it a grace
		# period rather than reap something that may be in active use.
		mark_seen "$name"
		continue
	fi

	test $(( now - epoch )) -gt "$reap_idle_secs" && reap_builder "$name"
done

# Pressure reap. If routine age-reaping still left free space under the global
# floor, evict idle builders oldest-first until the floor is met. A shorter but
# still-safe idle floor guards against killing a long concurrent run.
free_floor=$(gib "$reap_min_free")
if test "$(avail_bytes)" -lt "$free_floor"; then
	min_idle_secs=$(( 12 * 3600 ))
	for name in $(builders_with_state); do
		test "$name" = "$seed_builder" && continue
		test "$name" = "$builder" && continue
		epoch=$(marker_epoch "$name")
		test -z "$epoch" && continue
		echo "$(( now - epoch )) $name"
	done \
	| sort -rn \
	| while read -r builder_idle_secs name; do
		test -n "$name" || continue
		test "$builder_idle_secs" -gt "$min_idle_secs" || continue
		test "$(avail_bytes)" -ge "$free_floor" && break
		reap_builder "$name"
	done
fi

if test -n "$clean"; then
	docker buildx rm "$builder"
fi

docker buildx inspect "$builder"
if test x"$?" = x"0"; then
	exit 0
fi

set -eux

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

# Dependency cache mounts: cargo registry, cargo git, rustup downloads, the nix
# store, go module cache. Expensive to refetch or rebuild and, crucially, the
# only place the nix store can live (cache exporters cannot carry a cachemount),
# so this bucket alone is what keeps smoke-nix warm. Its own long keepDuration
# and size cap keep it isolated from the layer churn below: shrinking the layer
# ceiling never evicts the nix store, and this cap keeps the store itself from
# growing without bound.
[[worker.oci.gcpolicy]]
  filters = ["type==exec.cachemount"]
  keepDuration = "336h"
  maxUsedSpace = "${cachemount_max}"

# Everything else: build layers, sources, frontend. reservedSpace is the warm
# floor GC never prunes below, so the most-recently-used reservedSpace of layers
# (the foundation and cooked deps, touched at the start of every run) survives
# regardless of age. maxUsedSpace is the ceiling GC trims the total back to, but
# only records older than keepDuration are eligible: on a builder rebuilt many
# times a day a long keepDuration shields nearly the whole cache, the ceiling
# never binds, and it grows until the disk-pressure valve below dumps everything.
# 12h keeps the shielded set under maxUsedSpace so GC can trim the older tail.
[[worker.oci.gcpolicy]]
  filters = ["type!=exec.cachemount"]
  keepDuration = "12h"
  reservedSpace = "${reserved_space}"
  maxUsedSpace = "${max_used_space}"
  all = true

# Safety floor: under critical disk pressure, evict anything regardless of age
# or type. Last relief valve when the ceiling above has not sufficed; the reaper
# and the seed guard exist so this is never reached in normal operation, because
# reaching it evicts the nix store along with everything else.
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
#
# The copy is serialized by a host lock and gated on free space. Two new actors
# whose runs land together would otherwise each copy the seed at once and, as
# happened before, exhaust the disk; the lock makes them take turns, and the
# free-space gate falls back to a cold build when a copy would not fit. Because
# the layer ceiling keeps the seed source lean, each copy carries the foundation,
# cooked deps and the nix store rather than a run's worth of leaf output.
seed_state="buildx_buildkit_${seed_builder}0_state"
this_state="buildx_buildkit_${builder}0_state"
seeded=
exec 200>/tmp/tuwunel-ci-seed.lock
flock -x 200 || true
if test -z "$nocache" \
	&& test "$builder" != "$seed_builder" \
	&& test "$(avail_bytes)" -ge "$(gib "$seed_budget")" \
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
flock -u 200 || true

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
