#!/bin/bash
# Playwright adapter for the shared board engine. Defines the spec-tree classify
# and the per-shard gate, then dispatches: no argument renders the full board,
# `gate` runs the per-shard gate alone.
set -eo pipefail

# shellcheck source=./grid.sh
. "$(dirname "${BASH_SOURCE[0]}")/grid.sh"

track_name="Playwright"
results="tests/playwright/results.json"
acceptlist="tests/playwright/known-failures.txt"

# A missing results.json is a hard failure here: the verdict rides on this job.
noresults_rc=1

# Errors colour off the acceptlist, the board pads to a full square, and known
# failures are passed in for that colouring.
acc_active=1
square_fill=1
acclist=$(cat "$acceptlist" 2>/dev/null || true)

# Spec tree → one row per "file :: title", classified by worst project status.
# Tab-delimited because ids contain spaces.
classify() {
	jq -r '
		[.. | objects | select(has("tests") and has("title") and has("file"))]
		| map({
			id: "\(.file) :: \(.title)",
			s:  ([.tests[].status]
			     | if   any(. == "unexpected") then "error"
			       elif any(. == "flaky")      then "flaky"
			       elif any(. == "expected")   then "accept"
			       else                             "skip"
			       end)
		  })
		| unique_by(.id) | sort_by(.id)[]
		| "\(.s)\t\(.id)"
	' "$1"
}

# Current errors not on the known-failures acceptlist.
gate_violators() {
	awk -F'\t' '$1=="error" {print $2}' "$curr" | sort -u \
		| comm -23 - <(sort -u "$acceptlist" 2>/dev/null) | grep -v '^$' || :
}

case "${1:-}" in
	gate) gate_main ;;
	*)    summarise_main "$@" ;;
esac
