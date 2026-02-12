#!/bin/bash
set -eo pipefail

BASEDIR=$(dirname "$0")

CI="${CI:-false}"
CI_VERBOSE="${CI_VERBOSE:-false}"
CI_VERBOSE_ENV="${CI_VERBOSE_ENV:-$CI_VERBOSE}"
CI_SILENT_BAKE="${CI_SILENT_BAKE:-false}"
CI_PRINT_BAKE="${CI_PRINT_BAKE:-$CI_VERBOSE}"

default_cargo_profiles='["test"]'
default_feat_sets='["all"]'
default_rust_toolchains='["nightly"]'
default_rust_targets='["x86_64-unknown-linux-gnu"]'
default_sys_names='["debian"]'
default_sys_versions='["testing-slim"]'
default_sys_targets='["x86_64-v1-linux-gnu"]'

if test ! -z "$cargo_profile"; then
    env_cargo_profiles="[\"${cargo_profile}\"]"
fi

if test ! -z "$feat_set"; then
    env_feat_sets="[\"${feat_set}\"]"
fi

if test ! -z "$rust_target"; then
    env_rust_targets="[\"${rust_target}\"]"
fi

if test ! -z "$rust_toolchain"; then
    env_rust_toolchains="[\"${rust_toolchain}\"]"
fi

if test ! -z "$sys_name"; then
    env_sys_names="[\"${sys_name}\"]"
fi

if test ! -z "$sys_target"; then
    env_sys_targets="[\"${sys_target}\"]"
fi

if test ! -z "$sys_version"; then
    env_sys_versions="[\"${sys_version}\"]"
fi

set -a
bake_target="${bake_target:-$@}"
cargo_profiles="${env_cargo_profiles:-$default_cargo_profiles}"
feat_sets="${env_feat_sets:-$default_feat_sets}"
rust_targets="${env_rust_targets:-$default_rust_targets}"
rust_toolchains="${env_rust_toolchains:-$default_rust_toolchains}"
sys_names="${env_sys_names:-$default_sys_names}"
sys_targets="${env_sys_targets:-$default_sys_targets}"
sys_versions="${env_sys_versions:-$default_sys_versions}"

docker_dir="$PWD/$BASEDIR"
builder_name="${GITHUB_ACTOR:-owo}"

# Translates 'nightly' in `rust_toolchains` to some other value. Needed for
# github actions to pass some specific nightly. Local users can add the specific
# nightly to the `rust_toolchains` array as intended. see bake.hcl
rust_nightly="${rust_nightly:-nightly}"

# Translates 'stable' in `rust_toolchains` to some specific toolchain. Used
# by default for all callers to ensure the msrv is used instead of latest
# stable. see bake.hcl
toolchain_toml="$docker_dir/../rust-toolchain.toml"
rust_msrv=$(grep "channel = " "$toolchain_toml" | cut -d'=' -f2 | sed 's/\s"\|"$//g')

# override the source position with another ref
git_checkout="${git_checkout:-HEAD}"

# other options
rocksdb_opt_level=3
rocksdb_portable=1
use_chef="true"
set +a

###############################################################################

export DOCKER_BUILDKIT=1
if test "$CI" = "true"; then
    export BUILDKIT_PROGRESS="plain"
fi

args=""
args="$args --provenance=false"
args="$args --builder ${builder_name}"
#args="$args --set *.platform=${sys_platform}"

if test "$CI" = "true"; then
	args="$args --allow=network.host"
fi

if test "$(uname)" = "Darwin"; then
    nprocs=$(sysctl -n hw.logicalcpu)
    args="$args --set *.args.nprocs=${nprocs}"
    :
else
    nprocs=$(nproc)
    args="$args --set *.args.nprocs=${nprocs}"
    :
fi

if test "$CI_SILENT_BAKE" = "true"; then
	args="$args --progress=quiet"
fi

arg="$args -f $BASEDIR/bake.hcl"
trap 'set +x; date; echo -e "\033[1;41;37mERROR\033[0m"' ERR

if test "$CI_VERBOSE_ENV" = "true"; then
	date
	env
fi

if test "$CI_PRINT_BAKE" = "true"; then
    docker buildx bake --print $arg $bake_target
fi

if test "$NO_BAKE" = "1"; then
    exit 0
fi

trap '' ERR
set -ux
docker buildx bake $arg $bake_target
set +x
echo -e "\033[1;42;30mACCEPT\033[0m"
