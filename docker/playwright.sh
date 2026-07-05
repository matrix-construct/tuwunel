#!/bin/bash
set -eo pipefail

BASEDIR=$(dirname "$0")

CI="${CI:-false}"
CI_VERBOSE="${CI_VERBOSE_ENV:-false}"
CI_VERBOSE_ENV="${CI_VERBOSE_ENV:-$CI_VERBOSE}"

default_cargo_profile="test"
default_feat_set="all"
default_rust_toolchain="nightly"
default_rust_target="x86_64-unknown-linux-gnu"
default_sys_name="debian"
default_sys_target="x86_64-v1-linux-gnu"
default_sys_version="testing-slim"

default_playwright_run=".*"
default_playwright_skip="Read.receipts|oidc-native|backups-mas|login-sso|soft_logout_oauth|register/email|forgot-password"
default_playwright_shard="1/1"
default_playwright_count=1
default_playwright_workers=1
default_playwright_retries=0

run="${1:-$default_playwright_run}"

# Skip-list: alternation of regexes matched against each test id (its file path
# and title). Mirrors the equivalent block in docker/complement.sh, and like it
# stays space-free so the value survives the unquoted docker arg below.
#   "Read.receipts" holds out the entire read-receipts directory (every spec
# there nests under the "Read receipts" describe; the dot matches that space)
# pending a fix for position-blind notification counts.
#   The trailing stems hold out specs that need a MAS, OAuth, or SMTP sidecar
# the harness does not provision (no-ops in docker/lib/playwright.tuwunel.ts):
# element-web builds those fixtures before its own homeserver-type skip, so
# against tuwunel they error in setup instead of skipping themselves.
skip="${default_playwright_skip}"

set -a
cargo_profile="${cargo_profile:-$default_cargo_profile}"
feat_set="${feat_set:-$default_feat_set}"
rust_target="${rust_target:-$default_rust_target}"
rust_toolchain="${rust_toolchain:-$default_rust_toolchain}"
sys_name="${sys_name:-$default_sys_name}"
sys_target="${sys_target:-$default_sys_target}"
sys_version="${sys_version:-$default_sys_version}"
set +a

# Per-shard static-server port for the element-web CI webServer. The tester
# serves and reaches the app over its own loopback (npx serve + Chromium share
# the tester netns), so this only needs to be stable within a shard; pairs
# SERVE_PORT with BASE_URL (Playwright baseURL and health check).
shard_spec="${playwright_shard:-$default_playwright_shard}"
base_port=$(( 8080 + ${shard_spec%%/*} - 1 ))

envs=""
envs="$envs -e CI=true"
envs="$envs -e playwright_run=${playwright_run:-$run}"
envs="$envs -e playwright_skip=${playwright_skip:-$skip}"
envs="$envs -e playwright_shard=${playwright_shard:-$default_playwright_shard}"
envs="$envs -e playwright_count=${playwright_count:-$default_playwright_count}"
envs="$envs -e playwright_workers=${playwright_workers:-$default_playwright_workers}"
envs="$envs -e playwright_retries=${playwright_retries:-$default_playwright_retries}"
envs="$envs -e BASE_URL=http://localhost:${base_port}"
envs="$envs -e SERVE_PORT=${base_port}"

# The known-failures acceptlist steers run.sh's two-pass split (each known
# failure runs once, dealt round-robin across shards); mounted from the
# checkout so a baseline edit needs no tester rebuild. Without the file the
# tester falls back to a single pass.
vols=""
acceptlist="$(cd "$BASEDIR/.." && pwd)/tests/playwright/known-failures.txt"
if test -f "$acceptlist"; then
	vols="-v $acceptlist:/playwright/known-failures.txt:ro"
fi

set -x
tester_image="playwright-tester--${sys_name}--${sys_version}--${sys_target}"
testee_image="playwright-testee--${cargo_profile}--${rust_toolchain}--${rust_target}--${feat_set}--${sys_name}--${sys_version}--${sys_target}"
shard_slug=$(printf '%s' "${playwright_shard:-$default_playwright_shard}" | tr '/' '_')
name="playwright_tester__${sys_name}__${sys_version}__${sys_target}__${shard_slug}"
# Per-shard user-defined bridge isolating this shard from the others sharing the
# daemon. The tester and the testees it spawns join it; each testee binds the
# container-internal :8008 in its own netns (no host port, no cross-shard port
# contention) and is reached by container-name DNS. Off host networking, the
# tester's Chromium no longer sees sibling veth churn (net::ERR_NETWORK_CHANGED).
net="playwright_net__${sys_name}__${sys_version}__${sys_target}__${shard_slug}"
envs="$envs -e PLAYWRIGHT_NETWORK=$net"
sock="/var/run/docker.sock"
arg="--name $name -v $sock:$sock $vols --network $net $envs $tester_image"
set +x

if test "$CI_VERBOSE_ENV" = "true"; then
	date
	env
fi

docker rm -f "$name" 2>/dev/null || true
docker network rm "$net" 2>/dev/null || true
docker network create "$net" >/dev/null 2>&1 || true

arg="-d $arg"
cid=$(docker run $arg)

if test "$CI" = "true"; then
	echo -n "$cid" > "$name"
fi

result_src="$cid:/playwright/out/results.json"
result_dst="tests/playwright/results.json"
output_src="$cid:/playwright/out/output.log"
output_dst="tests/playwright/output.log"
# Per-failure traces, videos, and error-context.md, which Playwright writes to
# the config outputDir. Only populated when a test fails, so the upload step
# ignores an empty result.
artifacts_src="$cid:/usr/src/element-web/apps/web/playwright/test-results/."
artifacts_dst="tests/playwright/test-results"
mkdir -p tests/playwright "$artifacts_dst"

extract_output() {
	docker cp "$output_src" "$output_dst" 2>/dev/null || true
}
extract_results() {
	docker cp "$result_src" "$result_dst" 2>/dev/null || true
}
extract_artifacts() {
	docker cp "$artifacts_src" "$artifacts_dst" 2>/dev/null || true
}

trap 'extract_output; extract_results; extract_artifacts; set +x; date; echo -e "\033[1;41;37mERROR\033[0m"' ERR
trap 'docker container stop $cid; extract_output; extract_results; extract_artifacts' INT
docker logs -f "$cid"
docker wait "$cid" >/dev/null 2>&1 || true

extract_results
extract_output
extract_artifacts

echo -e "\033[1;42;30mACCEPT\033[0m"
