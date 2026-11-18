#!/bin/bash
#
# Summary board for the policyserv integration test.
#
# The runner (docker/lib/policyserv-runner.sh) writes one JSON row for each
# check it reaches, passed or failed. This turns those rows into the same board
# the Complement and Playwright jobs show: the tally, the grid of boxes, and a
# diff of what changed against the committed baseline.
#
# The verdict itself still comes from the runner's exit code, not from here.

set -eo pipefail

# The shared board engine: the tally, the grid, the diff and summarise_main.
# shellcheck source=./grid.sh
. "$(dirname "${BASH_SOURCE[0]}")/grid.sh"

track_name="Policy Server (policyserv) Integration Test"
results="tests/policyserv/results.jsonl"
baseline=$results


# classify FILE
#
# Turns each result row into the engine's "status<TAB>id" form. The id is the
# check's letter and name, for example:
#
#   error	E: a refused message is refused with policyserv's answer
#
classify() {
	local file=$1

	jq -r '
		# pass and fail map onto the engine words; anything else is a skip.
		(
			if   .outcome == "pass" then "accept"
			elif .outcome == "fail" then "error"
			else                         "skip"
			end
		) as $status

		| "\($status)\t\(.check): \(.name)"
	' "$file" \
	| sort -t $'\t' -k2,2
}


# snapshot_current
#
# Takes the place of the engine's version, which classifies this run's results
# and nothing more.
#
# A failed check ends the run, so the checks after it never write a row, and
# they would quietly drop off the board. Instead they are taken from the
# baseline and scored as skipped, so the board always shows the whole suite.
snapshot_current() {
	classify "$results" > "$curr"

	# The stand-in for a run with no rows, made below, is read only here.
	if test -n "${unrun_results:-}"; then
		rm -f "$unrun_results"
	fi

	# The baseline as committed, classified the same way. Before a baseline
	# exists this is empty, and there is nothing to fill in.
	local committed
	committed=$(git show "HEAD:$baseline" 2>/dev/null | classify /dev/stdin || true)

	# Every check in the baseline that this run has no row for.
	local unrun
	unrun=$(
		printf '%s\n' "$committed" | awk -F'\t' '
			# The first file is this run: remember which checks it reached.
			NR == FNR {
				ran[$2] = 1
				next
			}

			# The second is the baseline: keep the checks that were not reached.
			$2 != "" && !($2 in ran) {
				print "skip\t" $2
			}
		' "$curr" -
	)

	if test -n "$unrun"; then
		printf '%s\n' "$unrun" >> "$curr"
		sort -t $'\t' -k2,2 -o "$curr" "$curr"
	fi

	# Lay the grid out as a single row, one box per check.
	grid_width=$(wc -l < "$curr")
}


# A run that fails before check A, for example when the TLS front never comes
# up, writes no rows at all, and the engine would stop at "No results
# produced." Give it the committed checks marked as not run instead, so the
# board still shows the failure banner and every check as skipped.
if test ! -s "$results" && test "${execute_outcome:-success}" != "success"; then
	unrun_results=$(mktemp)
	git show "HEAD:$baseline" 2>/dev/null \
	| jq -c '.outcome = "unrun"' > "$unrun_results" || :
	results=$unrun_results
fi

summarise_main "$@"
