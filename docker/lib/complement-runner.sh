#!/bin/bash
set -eo pipefail

# Shared compliance-test runner. The two flavours (complement and
# complement-crypto) differ only in defaults, env-var prefix on the
# script-outer side, image/container names, result paths, and a crypto-only
# mitmproxy bootstrap. Wrappers (`docker/complement.sh`,
# `docker/complement-crypto.sh`) populate the per-flavour config below and
# delegate here.

# Required from caller:
#   flavor                 "complement" | "complement-crypto"
#   tester_image_prefix    "complement-tester" | "complement-crypto-tester"
#   container_name_prefix  "complement_tester" | "complement_crypto_tester"
#   src_root               "/usr/src/complement" | "/usr/src/complement-crypto"
#   results_dir            "tests/complement" | "tests/complement-crypto"
#   envs                   pre-built `-e KEY=VAL ...` string for docker run
#   (optional) addons_path crypto-only host/container mitmproxy_addons path
#   (optional) mitmproxy_image image to pre-pull before the run
#   (optional) interop_images   `hsname=image` lines for per-homeserver overrides
#   (optional) baseline_gate     1 (default) gates on the committed baseline; 0 reports only

CI="${CI:-false}"
CI_VERBOSE="${CI_VERBOSE_ENV:-false}"
CI_VERBOSE_ENV="${CI_VERBOSE_ENV:-$CI_VERBOSE}"

###############################################################################

if test -n "${mitmproxy_image:-}"; then
	docker pull -q "$mitmproxy_image" || true
fi

set -x
tester_image="${tester_image_prefix}--${sys_name}--${sys_version}--${sys_target}"
testee_image="complement-testee--${cargo_profile}--${rust_toolchain}--${rust_target}--${feat_set}--${sys_name}--${sys_version}--${sys_target}"
name="${container_name_prefix}__${sys_name}__${sys_version}__${sys_target}"

# One CI job runs per runner registration, so RUNNER_NAME is unique across jobs
# running at the same time and stable across a re-run on the same registration.
# Hash it (with the cell name) into 12 lowercase hex chars: short, valid inside a
# docker image repository name, and self-cleaning on re-run (the same token
# recomputes, so the prior attempt's resources are reclaimed). This single token
# both uniquifies the outer tester container and namespaces the inner Complement
# resources (the patched harness reads COMPLEMENT_RUN_ID).
run_seed="${RUNNER_NAME:-local-$$}"
run_id=$(printf '%s' "${run_seed}-${name}" | sha1sum | cut -c1-12)
name="${name}__${run_id}"
envs="$envs -e COMPLEMENT_RUN_ID=${run_id}"

# Schedule the testee above the build workload sharing this runner so the suite
# does not time out under contention; the testee entrypoint renices itself and
# CAP_SYS_NICE lets it. With complement_perf the same testee image additionally
# runs the server under `perf stat`: its entrypoint perf wrapper is opt-in on
# TESTEE_PERF (a plain exec otherwise) and CAP_PERFMON unblocks perf_event_open,
# which the default Docker seccomp profile already keys its allowance on. The
# testee env var is delivered through Complement's COMPLEMENT_SHARE_ENV_PREFIX
# passthrough, which strips the prefix and forwards the remainder into each
# spawned testee.
caps="SYS_NICE"
if test "${complement_perf:-0}" = "1"; then
	caps="PERFMON,$caps"
	envs="$envs -e COMPLEMENT_SHARE_ENV_PREFIX=COMPLEMENT_TESTEE_ENV_"
	envs="$envs -e COMPLEMENT_TESTEE_ENV_TESTEE_PERF=1"
fi
envs="$envs -e COMPLEMENT_TESTEE_CAP_ADD=$caps"

# Interop homeserver image overrides, one `hsname=image` per line. Pre-pull
# each image into the host daemon Complement deploys from (it will not pull a
# missing image itself), and forward Complement's native per-homeserver
# COMPLEMENT_BASE_IMAGE_<hsname> override into the tester container.
if test -n "${interop_images:-}"; then
	while IFS='=' read -r hs image; do
		test -n "$hs" || continue
		docker pull -q "$image" || true
		envs="$envs -e COMPLEMENT_BASE_IMAGE_${hs}=${image}"
	done <<< "$interop_images"
fi

sock="/var/run/docker.sock"
mkdir -p "$results_dir"

mounts="-v $sock:$sock"
if test -n "${addons_path:-}"; then
	# mitmproxy_addons lives inside the tester image. testcontainers-go in
	# complement-crypto's RunNewDeployment bind-mounts that path into the
	# mitmproxy sidecar via the host docker daemon, which resolves the path
	# on the host filesystem rather than inside the tester container. The
	# path does not exist on the host, and upstream config has no env-var
	# override, so we make the literal path exist on the host: a throwaway
	# `docker run -v <abs>:/out` creates the missing host directory, then
	# we copy the addons in from the tester image. The same host path is
	# bind-mounted into the tester at the same location so both sides see
	# identical content.
	docker run --rm --entrypoint="" -v "$addons_path:/out" "$tester_image" \
		sh -c "cp -r $addons_path/. /out/"
	mounts="$mounts -v $addons_path:$addons_path"
fi

arg="--name $name $mounts --network=host $envs $tester_image ${testee_image}"
set +x

if test "$CI_VERBOSE_ENV" = "true"; then
	date
	env
fi

# The outer tester container is reclaimed by name, the inner Complement
# containers and networks by label. Complement stamps `complement_run_id` on
# everything it creates, and the run_id token above is deterministic per runner
# and cell, so it recomputes identically across runs and across a re-run
# attempt. Filtering on the current token therefore reclaims exactly the debris
# a cancelled predecessor left behind, which otherwise collides at
# ContainerCreate and fails every test in the suite with no test-level signal,
# and it can never touch a concurrent sibling job, whose different cell or
# container-name prefix hashes to a different token.
#
# Blueprint images carry the same label and are deliberately spared: they are
# what lets a re-run deploy in seconds rather than minutes, so dropping them
# would make every attempt pay for a cold build.
sweep_run_resources() {
	local filter="label=complement_run_id=${run_id}"

	docker ps -aq --filter "$filter" | xargs -r docker rm -f >/dev/null 2>&1 || true
	docker network ls -q --filter "$filter" | xargs -r docker network rm >/dev/null 2>&1 || true

	return 0
}

docker rm -f "$name" 2>/dev/null
sweep_run_resources

arg="-d $arg"
cid=$(docker run $arg)

if test "$CI" = "true"; then
	echo -n "$cid" > "$name"
fi

output_src="$cid:${src_root}/full_output.jsonl"
output_dst="${results_dir}/logs.jsonl"
extract_output() {
	docker cp "$output_src" "$output_dst"
}

result_src="$cid:${src_root}/new_results.jsonl"
result_dst="${results_dir}/results.jsonl"
extract_results() {
	docker cp "$result_src" "$result_dst"
}

metrics_archive="${results_dir}/runtime_metrics.tar.zst"
extract_metrics() {
	rm -f "$metrics_archive"
	docker cp "$cid:/runtime_metrics" - 2>/dev/null \
		| zstd > "$metrics_archive" \
		|| rm -f "$metrics_archive"
}

trap 'extract_output; extract_metrics; set +x; date; echo -e "\033[1;41;37mERROR\033[0m"' ERR
trap 'docker container stop $cid; extract_output; extract_metrics; sweep_run_resources' INT TERM
docker logs -f "$cid"
docker wait "$cid" 2>/dev/null

extract_results
extract_output
extract_metrics
git diff -U0 --color --shortstat "$result_dst" | (grep "$run" || true)

if test "${baseline_gate:-1}" = "1"; then
	git diff --quiet --exit-code "$result_dst"
	echo -e "\033[1;42;30mACCEPT\033[0m"
else
	printf 'interop results: %s pass, %s fail, %s skip -> %s\n' \
		"$(grep -c '"Action":"pass"' "$result_dst" 2>/dev/null || true)" \
		"$(grep -c '"Action":"fail"' "$result_dst" 2>/dev/null || true)" \
		"$(grep -c '"Action":"skip"' "$result_dst" 2>/dev/null || true)" \
		"$result_dst"
	echo -e "\033[1;43;30mINTEROP (report-only)\033[0m"
fi
