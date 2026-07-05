#!/bin/bash
# Complement adapter for the shared board engine. Sourced by the per-flavour
# drivers; not invoked directly. Drivers set:
#
#   track_name      Heading text (e.g. "Complement", "Complement-Crypto").
#   results         Path to the per-flavour results.jsonl.
#   metrics_tar     Optional. Path to per-run runtime_metrics.tar.zst. When
#                   set, emit_runtime_metrics() renders the metrics table.
#
# Runtime-metrics history is read from and written to small JSON digests the
# workflow restores from / saves to the GitHub Actions cache, passed via env:
#   RUNTIME_DIGEST_OWN   this branch's rolling ring (read for history columns,
#                        rewritten for the green-gated save step).
#   RUNTIME_DIGEST_MAIN  the default branch's anchor (read only; unset on it).
#   RUNTIME_DIGEST_KEEP  ring depth (3 off the default branch, 1 on it).
#
# Drivers then call `summarise_main "$@"`.

set -eo pipefail

# shellcheck source=./grid.sh
. "$(dirname "${BASH_SOURCE[0]}")/grid.sh"

# Some tests emit both pass and fail rows (subtests). Fold by Test, fail wins, and
# emit the shared classified format (status<TAB>id, sorted by id).
classify() {
	jq -sr '
		group_by(.Test)[]
		| { t: .[0].Test, s:
		      (if   any(.[]; .Action == "fail") then "error"
		       elif any(.[]; .Action == "pass") then "accept"
		       else                                  "skip"
		       end) }
		| "\(.s)\t\(.t)"
	' "$1" | sort -t $'\t' -k2,2
}

# Interop only: the top-level tests that deployed a peer homeserver this run,
# i.e. the tests an image override can actually affect. A peer's server name
# (hs2, hs3, ...) appears in the captured output only when that server was
# deployed; tests with only hs1 never mention it and become black board filler.
# Pre-filter the (large) log with grep so jq parses only the matching lines.
affected_tests() {
	test -s "${logs_jsonl:-}" || return 0
	grep -aE 'hs[2-9]' "$logs_jsonl" | jq -r '.Test' 2>/dev/null | sort -u
}

# Distinct leaf tests for the tally. A classified row is a leaf when no other row
# nests beneath it ("<name>/..."); a parent only aggregates its subtests (fail
# wins in classify), so counting parents alongside their leaves double-counts. Go
# nests arbitrarily deep, so leaf-ness is computed over every ancestor prefix.
compute_leaves() {
	awk '
		{ nm[NR] = $2; m = split($2, p, "/"); pre = p[1]
		  for (i = 2; i <= m; i++) { internal[pre] = 1; pre = pre "/" p[i] } }
		END { for (i = 1; i <= NR; i++) if (!(nm[i] in internal)) print nm[i] }
	' "${1:-$curr}"
}

# Fixed column count for every flavour's grid, anchored to the Complement track's
# square width (ceil(sqrt(791)) = 29 at time of writing). Holding the width
# constant lets the smaller Complement-Crypto suite render as a short horizontal
# band instead of a tiny square; grid height then floats with each flavour's test
# count. A driver may override by setting grid_width first.
grid_width="${grid_width:-29}"

# Render the runtime-metrics table. History columns come from the cache digests
# the workflow already restored (this branch's ring and the default branch's
# anchor); the updated ring is written back to RUNTIME_DIGEST_OWN for the
# workflow's green-gated save step. Soft-fails: no metrics tar, no python, no
# digests all degrade to the current-run column alone.
emit_runtime_metrics() {
	# A failed run's timings are unreliable, so neither record nor display them.
	test "${execute_outcome:-success}" = "success" || return 0
	test -n "${metrics_tar:-}" || return 0
	test -s "$metrics_tar" || return 0
	command -v python3 >/dev/null 2>&1 || return 0

	local script="$(dirname "$BASH_SOURCE")/complement_metrics.py"
	test -x "$script" || return 0

	local args=(--tar "$metrics_tar" --out "$out")
	if test -n "${RUNTIME_DIGEST_OWN:-}"; then
		test -s "$RUNTIME_DIGEST_OWN" && args+=(--history-in "$RUNTIME_DIGEST_OWN")
		args+=(--history-out "$RUNTIME_DIGEST_OWN" --keep "${RUNTIME_DIGEST_KEEP:-3}")
	fi
	test -s "${RUNTIME_DIGEST_MAIN:-}" && args+=(--main-in "$RUNTIME_DIGEST_MAIN")

	# Main-branch runs are the baseline; the table carries no diff signal there,
	# so still record the digest but render no table (mirrors the failure board).
	if test "${GITHUB_REF_NAME:-}" = "main"; then
		args+=(--no-render)
	fi
	python3 "$script" "${args[@]}" || :
}
