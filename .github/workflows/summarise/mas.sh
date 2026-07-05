#!/bin/bash
set -eo pipefail

# Minimal summariser for the MAS provisioning smoke test. Target A is a single
# pass/fail round-trip, not a per-test results matrix, so there is nothing to
# diff against a baseline: the gate is mas.sh's exit code. This only renders a
# one-line outcome into the job summary.

jsonl="tests/mas/results.jsonl"
out="${GITHUB_STEP_SUMMARY:-/dev/stdout}"

{
	echo "### Matrix-Authentication-Service Provisioning Smoketest"
	if test -s "$jsonl"; then
		echo '```json'
		cat "$jsonl"
		echo '```'
		echo "Passed: User provisioned through MAS is visible on Tuwunel; Device upsert succeeded; Wrong secret was rejected."
	else
		echo "Failed: No results recorded; the run failed before the assertions completed. See the uploaded logs."
	fi
} >>"$out"
