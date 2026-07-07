#!/bin/bash
set -eo pipefail

BASEDIR=$(dirname "$0")

default_cargo_profile="test"
default_feat_set="all"
default_rust_toolchain="nightly"
default_rust_target="x86_64-unknown-linux-gnu"
default_sys_name="debian"
default_sys_target="x86_64-v1-linux-gnu"
default_sys_version="testing-slim"

default_complement_verbose=0
default_complement_dirty=0
default_complement_count=1
default_complement_parallel=1
default_complement_shuffle=0
default_complement_timeout="1h"
default_complement_run=".*"

run="${1:-$default_complement_run}"

skip=""
skip="${skip}TestThreadReceiptsInSyncMSC4102"
skip="${skip}|TestToDeviceMessagesOverFederation/stopped_server"
skip="${skip}|TestRestrictedRoomsRemoteJoinFailOver"
skip="${skip}|TestRestrictedRoomsRemoteJoinFailOverInMSC3787Room"
skip="${skip}|TestToDeviceMessagesOverFederation/interrupted_connectivity"
skip="${skip}|TestDeviceListsUpdateOverFederation/stopped_server"
skip="${skip}|TestJumpToDateEndpoint/parallel/federation"
skip="${skip}|TestJumpToDateEndpoint/parallel/should_find_next_event_topologically"

set -a
cargo_profile="${cargo_profile:-$default_cargo_profile}"
feat_set="${feat_set:-$default_feat_set}"
rust_target="${rust_target:-$default_rust_target}"
rust_toolchain="${rust_toolchain:-$default_rust_toolchain}"
sys_name="${sys_name:-$default_sys_name}"
sys_target="${sys_target:-$default_sys_target}"
sys_version="${sys_version:-$default_sys_version}"

runner_name=$(echo $RUNNER_NAME | cut -d"." -f1)
runner_num=$(echo $RUNNER_NAME | cut -d"." -f2)
set +a

# The debug build (the `test` cargo profile, compiled with debug-assertions and
# overflow checks) runs the whole suite slower than the optimized build,
# including the state-resolution conformance test which backfills a large event
# graph and resolves it. Give the run more headroom (still honoring an explicit
# complement_timeout override).
if test "$cargo_profile" = "test"; then
	default_complement_timeout="2h"
fi

envs=""
envs="$envs -e complement_verbose=${complement_verbose:-$default_complement_verbose}"
envs="$envs -e complement_count=${complement_count:-$default_complement_count}"
envs="$envs -e complement_dirty=${complement_dirty:-$default_complement_dirty}"
envs="$envs -e complement_parallel=${complement_parallel:-$default_complement_parallel}"
envs="$envs -e complement_shuffle=${complement_shuffle:-$default_complement_shuffle}"
envs="$envs -e complement_timeout=${complement_timeout:-$default_complement_timeout}"
envs="$envs -e complement_skip=${complement_skip:-$skip}"
envs="$envs -e complement_run=${1:-$default_complement_run}"
envs="$envs -e COMPLEMENT_ALWAYS_PRINT_SERVER_LOGS=1"
envs="$envs -e COMPLEMENT_DESTROY_HS_TIMEOUT_SECS=10"

# Interop runs assign a different homeserver image to one or more homeservers
# so federation is exercised between heterogeneous implementations (Synapse vs
# tuwunel, or two tuwunel builds). This drives Complement's native
# COMPLEMENT_BASE_IMAGE_<hsname> override; the runner pre-pulls each image and
# forwards the override into the tester container. Selectors, by precedence:
#
#   interop=synapse                   hs2 := the published Synapse image
#   interop_image=<ref>               hs2 := <ref>
#   interop_hs="hs2 hs4"              which homeservers the foreign image owns
#   COMPLEMENT_BASE_IMAGE_hs2=<ref>   raw per-homeserver passthrough
#
# Any interop selector switches the run to report-only: the heterogeneous
# result set does not match the homogeneous baseline in
# tests/complement/results.jsonl, so results land under
# tests/complement/interop and the baseline gate is off.
default_interop_hs="hs2"
default_synapse_image="ghcr.io/element-hq/synapse/complement-synapse:latest"

interop_images=""
add_interop() {
	local image="$1" hs
	for hs in ${interop_hs:-$default_interop_hs}; do
		interop_images="${interop_images}${hs,,}=${image}"$'\n'
	done
}

case "${interop:-}" in
	synapse) add_interop "${synapse_image:-$default_synapse_image}" ;;
	"") ;;
	*) echo "complement: unknown interop peer '${interop}'" >&2; exit 1 ;;
esac

if test -n "${interop_image:-}"; then
	add_interop "$interop_image"
fi

# Raw passthrough: forward any COMPLEMENT_BASE_IMAGE_<hsname> already exported.
# The suffix is lowercased because Complement looks the override up by the
# literal homeserver name (`hs1`, `hs2`), so a conventional uppercase env var
# would otherwise be silently ignored.
while IFS='=' read -r _name _value; do
	case "$_name" in
	COMPLEMENT_BASE_IMAGE_?*)
		hs="${_name#COMPLEMENT_BASE_IMAGE_}"
		interop_images="${interop_images}${hs,,}=${_value}"$'\n'
		;;
	esac
done < <(env)

flavor="complement"
tester_image_prefix="complement-tester"
container_name_prefix="complement_tester"
src_root="/usr/src/complement"
results_dir="tests/complement"

if test -n "$interop_images"; then
	results_dir="tests/complement/interop"
	baseline_gate=0
	envs="$envs -e COMPLEMENT_SPAWN_HS_TIMEOUT_SECS=${complement_spawn_timeout:-60}"
else
	# Both the optimized and debug runs gate the same homogeneous baseline in
	# tests/complement/results.jsonl; the two produce identical results.
	baseline_gate=1
fi

export flavor tester_image_prefix container_name_prefix src_root results_dir
export envs run interop_images baseline_gate

exec "$BASEDIR/lib/complement-runner.sh"
