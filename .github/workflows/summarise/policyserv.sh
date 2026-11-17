#!/bin/bash
set -eo pipefail

# Minimal summariser for the policyserv integration test. Its checks run in
# order and each passed one writes a row, the last only when all have passed, so
# there is no baseline to diff: the gate is policyserv.sh's exit code. This
# renders the rows into the job summary.

jsonl="tests/policyserv/results.jsonl"
out="${GITHUB_STEP_SUMMARY:-/dev/stdout}"

{
	echo "### Policy Server (policyserv) Integration Test"
	if test -s "$jsonl"; then
		echo '```json'
		cat "$jsonl"
		echo '```'
	fi
	if grep -q '"check":"all"' "$jsonl" 2>/dev/null; then
		echo "Passed: policyserv signed, refused and was waited on as the room's policy server."
	else
		echo "Failed: the run stopped after the checks listed above. See the uploaded logs."
	fi
} >>"$out"
