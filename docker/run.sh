#!/bin/bash
set -eo pipefail

default_uwu_id="jevolk/conduwuit"
uwu_id=${uwu_id:=$default_uwu_id}
uwu_acct=${uwu_acct:=$(echo $uwu_id | cut -d"/" -f1)}
uwu_repo=${uwu_repo:=$(echo $uwu_id | cut -d"/" -f2)}

CI="${CI:-0}"
BASEDIR=$(dirname "$0")

default_cargo_profiles='["test", "bench"]'
default_feat_sets='["none", "default", "all"]'
default_rust_toolchains='["nightly", "stable"]'
default_rust_targets='["x86_64-unknown-linux-gnu"]'
default_sys_targets='["x86_64-linux-gnu"]'
default_sys_versions='["testing-slim"]'
default_sys_names='["debian"]'

if test ! -z "$cargo_profile"; then
	env_cargo_profiles="[\"${cargo_profile}\"]"
fi

if test ! -z "$feat_set"; then
	env_feat_sets="[\"${feat_set}\"]"
fi

if test ! -z "$rust_toolchain"; then
	env_rust_toolchains="[\"${rust_toolchain}\"]"
fi

if test ! -z "$rust_target"; then
	env_rust_targets="[\"${rust_target}\"]"
fi

set -a
bake_target="${bake_target:-$@}"
cargo_profiles="${env_cargo_profiles:-$default_cargo_profiles}"
feat_sets="${env_feat_sets:-$default_feat_sets}"
rust_toolchains="${env_rust_toolchains:-$default_rust_toolchains}"
rust_targets="${env_rust_targets:-$default_rust_targets}"
sys_targets="$default_sys_targets"
sys_versions="$default_sys_versions"
sys_names="$default_sys_names"

runner_name=$(echo $RUNNER_NAME | cut -d"." -f1)
runner_num=$(echo $RUNNER_NAME | cut -d"." -f2)
rocksdb_opt_level=3
rocksdb_portable=1
git_checkout="HEAD"
use_chef="true"
complement_count=1
complement_skip="TestPartialStateJoin.*"
complement_skip="${complement_skip}|TestRoomDeleteAlias/Pa.*/Can_delete_canonical_alias"
complement_skip="${complement_skip}|TestUnbanViaInvite.*"
complement_skip="${complement_skip}|TestRoomDeleteAlias/Pa.*/Regular_users_can_add_and_delete_aliases_when.*"
complement_skip="${complement_skip}|TestToDeviceMessagesOverFederation/stopped_server"
complement_run=".*"
set +a

###############################################################################

tester_image="complement-tester--none--debian--testing-slim--x86_64-linux-gnu"
testee_image="complement-testee--test--nightly--x86_64-unknown-linux-gnu--none--debian--testing-slim--x86_64-linux-gnu"
name="complement_tester_nightly"
sock="/var/run/docker.sock"
arg="--rm --name $name -v $sock:$sock --network=host $tester_image ${testee_image}"

trap 'set +x; date; echo -e "\033[1;41;37mFAIL\033[0m"' ERR
date
env
set -x -e
cid=$(docker run -d $arg)
set +x
trap 'docker container stop $cid; set +x; date; echo -e "\033[1;41;37mFAIL\033[0m"' INT
docker wait "$cid" 2>/dev/null
echo -e "\033[1;42;37mPASS\033[0m"
