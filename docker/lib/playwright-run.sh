#!/bin/bash
# Tester-container entrypoint, pulled into the playwright-tester image from
# the source stage (see docker/Dockerfile.playwright). The CI caller is
# docker/playwright.sh, which threads the playwright_* values through the
# docker run environment and bind-mounts the known-failures acceptlist.
#
# The suite runs in two passes. Pass 1 is the expected-pass set: everything
# except the acceptlist, split across shards by Playwright's own contiguous
# count-based chunking, with retries as configured. Pass 2 is this shard's
# slice of the acceptlist with --retries=0: those tests fail
# deterministically (that is what keeps them on the list), so a retry only
# doubles their timeout burn without adding information, while still running
# each exactly once preserves the flip signal the regression gate and the
# baseline diff rely on. Slices are dealt round-robin by line number, which
# spreads expensive same-file failure clusters (audio-player alone is eight
# 90s-timeout tests) that contiguous chunking would pack onto one shard.
#
# --max-failures=0 overrides element-web's playwright.config.ts CI default
# of 10. With the early-abort active, the suite stopped at the 10th unique
# failure and marked the rest skipped, which made the failing-spec set
# non-deterministic across runs. The regression gate in
# .github/workflows/summarise/playwright.sh needs a stable input set.
set -euxo pipefail

cd /usr/src/element-web/apps/web

out=/playwright/out
acceptlist=/playwright/known-failures.txt

run="${playwright_run:-.*}"
skip="${playwright_skip:-}"
shard="${playwright_shard:-1/1}"
count="${playwright_count:-1}"
workers="${playwright_workers:-1}"
retries="${playwright_retries:-1}"

shard_index="${shard%%/*}"
shard_total="${shard##*/}"

# One "file :: title" acceptlist line becomes a regex over the id Playwright
# greps (project name, file path, describe titles, test title, then any
# "@tags", all space-joined): regex specials escaped, the " :: " joint
# widened to ".*" to span the describe titles between file and title, and
# the end pinned after the title (allowing only a tag tail) so an entry
# cannot swallow a longer sibling it prefixes.
regexify() {
	sed \
		-e 's/[][(){}.?+*^$|\\]/\\&/g' \
		-e 's/ :: /.*/' \
		-e 's/$/( @.*)?$/' \
		"$@"
}

join_lines() {
	paste -sd'|' -
}

knownfail_all=""
knownfail_mine=""
if test -s "$acceptlist"; then
	knownfail_all=$(regexify "$acceptlist" | join_lines)
	knownfail_mine=$(
		awk -v n="$shard_total" -v i="$shard_index" 'NR % n == i % n' "$acceptlist" \
			| regexify \
			| join_lines
	)
fi

run_playwright() {
	local results="$1"
	shift

	PLAYWRIGHT_JSON_OUTPUT_NAME="$results" \
	npx playwright test \
		--project=Tuwunel \
		--repeat-each="$count" \
		--workers="$workers" \
		--max-failures=0 \
		--reporter="list,json" \
		"$@"
}

# Without an acceptlist (no bind-mount), or under a caller-narrowed --grep
# filter (the local single-spec loop), fall back to the single-pass shape.
if test -z "$knownfail_all" || test "$run" != ".*"; then
	invert=(--grep-invert="$skip")
	test -n "$skip" || invert=()

	run_playwright "$out/results.json" \
		--grep="$run" \
		"${invert[@]}" \
		--shard="$shard" \
		--retries="$retries" \
		2>&1 | tee "$out/output.log"

	exit
fi

# Pass 1: the expected-pass set. Failures here are either flakes (the retry
# absorbs them) or the regressions the gate exists to catch.
pass1_invert="$knownfail_all"
if test -n "$skip"; then
	pass1_invert="${skip}|${knownfail_all}"
fi

run_playwright "$out/results-expected.json" \
	--grep="$run" \
	--grep-invert="$pass1_invert" \
	--shard="$shard" \
	--retries="$retries" \
	2>&1 | tee "$out/output.log" \
	|| true

# Pass 2: this shard's slice of the acceptlist, one attempt each. Playwright
# exits non-zero whenever any test fails, which is this pass's steady state,
# so the exit code is advisory only; the per-shard gate is the verdict.
pass2=(--grep="(${knownfail_mine})")
if test -n "$skip"; then
	pass2+=(--grep-invert="$skip")
fi

if test -n "$knownfail_mine"; then
	run_playwright "$out/results-knownfail.json" \
		"${pass2[@]}" \
		--retries=0 \
		2>&1 | tee -a "$out/output.log" \
		|| true
fi

# Merge the pass reports into the results.json the caller extracts. The
# summarisers classify by recursive descent, so a two-report array reads the
# same as one report. A report absent because its pass matched nothing (an
# acceptlist gone stale against upstream) is dropped rather than fatal.
node -e '
	const fs = require("fs");
	const [dest, ...srcs] = process.argv.slice(1);
	const reports = srcs
		.filter((f) => fs.existsSync(f))
		.map((f) => JSON.parse(fs.readFileSync(f, "utf8")));
	if (reports.length) {
		fs.writeFileSync(dest, JSON.stringify(reports.length === 1 ? reports[0] : reports));
	}
' "$out/results.json" "$out/results-expected.json" "$out/results-knownfail.json"
