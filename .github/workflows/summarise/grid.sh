#!/bin/bash
# Shared board engine for the results-grid summarisers (the Complement flavours
# and Playwright): the rendering primitives plus the driver that orchestrates
# them. Sourced, not invoked directly. Every helper speaks the same classified-
# row format: one row per test, tab-separated as
#
#   <status><TAB><id>
#
# where status is one of accept, error, flaky, skip and id never contains a tab.
# A driver supplies its own classify() that emits this format (sorted by id), then
# dispatches to summarise_main (full board) or gate_main (per-shard gate). Two
# scratch files hold the classified snapshots:
#
#   $curr       this run's rows.
#   $prev       the committed baseline's rows (empty on a first run).
#   $universe   the id list the board is laid over; equals $curr for the
#               homogeneous boards, the baseline list for the interop board.
#
# These scratch paths and the cell knobs ($adv_cell, $acc_active, $square_fill)
# are set by the sourcing driver, so shellcheck cannot see their assignment here.
# shellcheck disable=SC2154

###############################################################################
# Rendering primitives

# Rows of file $1 whose status (field 1) matches any '|'-separated token in $2,
# re-sorted by id so the join in delta() sees ordered input.
filter_status() {
	awk -F'\t' -v p="$2" '
		BEGIN { n = split(p, a, "|"); for (i = 1; i <= n; i++) want[a[i]] = 1 }
		want[$1]
	' "$1" | sort -t $'\t' -k2,2
}

# HEAD-delta: ids whose status flipped from any-of-$1 (in $prev) to any-of-$2 (in
# $curr). Used both ways round to compute regressions and progressions.
delta() {
	join -t $'\t' -j2 \
		<(filter_status "$prev" "$1") \
		<(filter_status "$curr" "$2") \
		| cut -f1
}

# Square-ish board width for n cells: ceil(sqrt(n)), floored at 1.
grid_width() {
	awk -v n="$1" 'BEGIN { w = int(sqrt(n)); if (w * w < n) w++; print (w ? w : 1) }'
}

# Render the emoji board. Statuses are read from $curr; one cell is laid per id in
# $universe, wrapping every $1 cells. Cell vocabulary:
#
#   🟩 / ✅   accept (stable / progressed this run; interop passes ❎ as $adv_cell)
#   🟨        flaky
#   🟧 / 🟥   error (known or acceptlisted / regressed or off the acceptlist)
#   ⬛        skip, or an id with no current result (interop board filler)
#   ⬜        grid filler past the last id
#
# Args: $1 width, $2 nobase (1 = no baseline), $3 regressed ids, $4 progressed ids,
# $5 acceptlisted ids (newline-joined). Knobs read from the environment:
#   $adv_cell    progressed-accept glyph (default ✅).
#   $acc_active  1 = colour errors off the acceptlist ($5) rather than $nobase.
#   $square_fill 1 = pad to a full w×w square, else only complete the last row.
render_grid() {
	awk -F'\t' \
		-v w="$1" \
		-v nobase="$2" \
		-v R="$3" \
		-v P="$4" \
		-v A="$5" \
		-v adv="${adv_cell:-✅}" \
		-v acc_active="${acc_active:-0}" \
		-v square="${square_fill:-0}" '
		BEGIN {
			split(R, x, "\n"); for (i in x) if (x[i]) reg[x[i]] = 1
			split(P, x, "\n"); for (i in x) if (x[i]) pro[x[i]] = 1
			split(A, x, "\n"); for (i in x) if (x[i]) acc[x[i]] = 1
		}
		NR == FNR { st[$2] = $1; next }
		{
			t = $2
			s = (t in st) ? st[t] : ""
			if      (s == "" || s == "skip") c = "⬛"
			else if (s == "flaky")           c = "🟨"
			else if (s == "accept")          c = pro[t] ? adv : "🟩"
			else                             c = (reg[t] || (acc_active ? !acc[t] : nobase)) ? "🟥" : "🟧"

			printf "%s", c
			if (++n % w == 0) printf "<br>"
		}
		END {
			if (square)
				while (n < w * w) { printf "⬜"; if (++n % w == 0) printf "<br>" }
			else
				while (n % w != 0) { printf "⬜"; if (++n % w == 0) printf "<br>" }
		}
	' "$curr" "$universe"
}

# Emit a ```diff block: lines of $1 prefixed '- ' (red), lines of $2 prefixed '+ '
# (green). Blank input lines are dropped, so callers may pass blank-joined lists.
# Nothing is emitted when neither list has content.
emit_diff_lines() {
	local minus plus
	minus=$(printf '%s\n' "$1" | sed -n 's/^./- &/p')
	plus=$( printf '%s\n' "$2" | sed -n 's/^./+ &/p')

	test -n "$minus$plus" || return 0

	echo
	echo '```diff'
	test -n "$minus" && printf '%s\n' "$minus"
	test -n "$plus" && printf '%s\n' "$plus"
	echo '```'
}

###############################################################################
# Board driver
#
# The orchestration shared by every grid summariser. A sourcing adapter sets the
# inputs and may override the hooks, then dispatches to summarise_main (full
# board) or gate_main (per-shard gate). Inputs:
#
#   track_name   Heading text (e.g. "Complement", "Playwright").
#   results      Path to this run's results file.
#   baseline     Path in HEAD to diff against (default $results).
#
# Hooks an adapter defines (all optional but classify):
#   classify()             results file -> "status<TAB>id" rows, sorted by id.
#   compute_leaves()       $curr -> leaf id list for the tally; the default
#                          treats every row as its own leaf.
#   gate_violators()       $curr -> off-acceptlist error ids; defining it turns
#                          the board into a gate (folds into the diff, sets the
#                          exit status, enables the gate subcommand).
#   emit_runtime_metrics() render a metrics table after the board.
#
# Grid knobs read by render_grid: grid_width, adv_cell, acc_active, square_fill,
# render_over_baseline, acclist. An unset grid_width floats with the test count.

out="${GITHUB_STEP_SUMMARY:-/dev/stdout}"

snapshot_current() {
	classify "$results" > "$curr";
}

snapshot_baseline() {
	git show "HEAD:${baseline:-$results}" 2>/dev/null | classify /dev/stdin > "$prev" || :;
}

# Default leaf set: every classified row stands alone. Complement overrides this
# to fold Go's nested subtests onto their deepest leaf, so a parent that only
# aggregates its subtests is not counted alongside them.
compute_leaves() {
	cut -f2 "${1:-$curr}";
}

# Passing rate: accept over the board's test count (accept + errors + flaky +
# skips). Skips stay in the denominator, so a flaky skip holds the rate below
# 100% until it is fixed. An empty board renders "n/a" rather than dividing by
# zero.
pct() {
	awk -v a="$1" -v d="$2" 'BEGIN {
		if (d == 0) print "n/a"; else printf "%.1f%%\n", 100 * a / d
	}'
}

# One flat tally row per board: the leaf counts by status, the diff sizes, and
# the passing rate. Every flavour renders the same column set; flaky reads 0 on
# the boards whose classify never emits it.
emit_header() {
	echo "### $track_name"
	echo
	# A failed execute leaves the results truncated at wherever the run stopped, so
	# the tally below speaks only to what was captured, not the whole suite. Flag it
	# rather than letting a partial board read as a clean pass.
	if test "${execute_outcome:-success}" != "success"; then
		echo "**⚠️ The test run did not finish; this board reflects only the results captured before it stopped.**"
		echo
	fi
	echo "| accept | errors | flaky | skipped | advanced | regressed | passing |"
	echo "|---|---|---|---|---|---|---|"
	echo "| $accept | $error | $flaky | $skip | $nprog | $nreg | $passing |"
	if test -n "$1"; then
		echo
		echo "$1"
	fi
}

# Diff block: gate violators and regressions share the '-' (red) prefix,
# progressions the '+' (green). A regression that also violates the gate is
# listed once, under the violators (the severer signal). Boards without a gate
# pass an empty $gate, leaving plain regress/progress.
emit_diff() {
	local regress_only="$regress"
	if test -n "${gate:-}"; then
		regress_only=$(printf '%s\n' "$regress" | grep -vxFf <(printf '%s\n' "$gate") || :)
	fi

	emit_diff_lines "$(printf '%s\n%s' "${gate:-}" "$regress_only")" "$progress"
}

summarise_main() {
	if test ! -s "$results"; then
		echo "No results produced." >> "$out"
		exit "${noresults_rc:-0}"
	fi

	curr=$(mktemp); prev=$(mktemp); leaves=$(mktemp); universe=$(mktemp)
	trap 'rm -f "$curr" "$prev" "$leaves" "$universe"' EXIT
	snapshot_current
	snapshot_baseline

	# Interop only: keep just the tests that deployed a peer homeserver this run;
	# the rest are inert under the image override and would render as black board
	# filler. The diff and tally then speak only to tests that exercised interop.
	if test -n "${logs_jsonl:-}"; then
		local aff; aff=$(mktemp)
		affected_tests > "$aff"
		if test -s "$aff"; then
			awk 'NR == FNR { a[$1] = 1; next }
			     { t = $2; sub(/\/.*/, "", t); if (t in a) print }' \
				"$aff" "$curr" > "$curr.eff" && mv "$curr.eff" "$curr"
		fi
		rm -f "$aff"
	fi

	regress=$( delta "accept|flaky" "error")
	progress=$(delta "error" "accept|flaky")

	nobase=0
	test -s "$prev" || { regress= progress= nobase=1; }

	# A driver may flag whole test families whose interop failure is a known
	# peer-side cause, not a tuwunel regression (interop_fp_regress, a '|'-joined
	# top-level name alternation). Drop their leaves from the red set so they
	# render as accounted-for (orange) rather than regressions (red). Unset
	# everywhere but the interop board, where it is a no-op.
	if test -n "${interop_fp_regress:-}"; then
		regress=$(printf '%s\n' "$regress" | grep -vE "^(${interop_fp_regress})(\$|/)" || :)
	fi

	# Flat tally over the leaf set. accept/error/flaky/skip come from the
	# classified snapshot; advanced/regressed are the diff lists confined to
	# leaves (a no-op on the boards where every row is already a leaf).
	compute_leaves | sort > "$leaves"
	read -r accept error flaky skip < <(awk -F'\t' '
		NR == FNR { leaf[$0] = 1; next }
		$2 in leaf { c[$1]++ }
		END { printf "%d %d %d %d\n",
		             c["accept"] + 0, c["error"] + 0, c["flaky"] + 0, c["skip"] + 0 }
	' "$leaves" "$curr")
	local total=$((accept + error + flaky + skip))
	passing=$(pct "$accept" "$total")

	nprog=$(printf '%s' "$progress" | grep -Fxcf "$leaves" || :)
	nreg=$( printf '%s' "$regress"  | grep -Fxcf "$leaves" || :)

	# A gate_violators hook turns the board into a gate: off-acceptlist errors
	# fold into the red diff and set the exit status. Absent everywhere else.
	local gate="" ngate=0
	if declare -F gate_violators >/dev/null; then
		gate=$(gate_violators)
		ngate=$(printf '%s' "$gate" | grep -c . || :)
	fi

	# Squares are emitted over the same leaf set the tally counts, so the cell
	# count matches the header rather than over-counting Go's parent aggregates.
	# Interop diffs against a foreign baseline and lays its board over that
	# baseline's leaves, so the two line up square-for-square; every other flavour
	# lays over this run's own (effective) leaves.
	local uni_src="$curr"
	if test -n "${render_over_baseline:-}" && test -s "$prev"; then
		uni_src="$prev"
	fi
	awk -F'\t' 'NR == FNR { leaf[$0] = 1; next } $2 in leaf' \
		<(compute_leaves "$uni_src" | sort) "$uni_src" > "$universe"

	# Main-branch runs are the baseline; the grid carries no diff signal there.
	if test "${GITHUB_REF_NAME:-}" = "main"; then
		grid=
	else
		grid=$(render_grid "${grid_width:-$(grid_width "$total")}" "$nobase" "$regress" "$progress" "${acclist:-}")
	fi

	{ emit_header "$grid"; emit_diff; } >> "$out"

	if declare -F emit_runtime_metrics >/dev/null; then
		emit_runtime_metrics
	fi

	test "$ngate" -eq 0
}

# Per-shard gate: fail the shard on any error not on the acceptlist. The verdict
# rides on the execution job, so a rerun re-executes the tests, not a stale
# summary. Only boards with a gate_violators hook dispatch here.
gate_main() {
	if test ! -s "$results"; then
		echo "No results produced." >> "$out"
		exit "${noresults_rc:-0}"
	fi

	curr=$(mktemp)
	trap 'rm -f "$curr"' EXIT
	snapshot_current

	local gate ngate
	gate=$(gate_violators)
	ngate=$(printf '%s' "$gate" | grep -c . || :)

	if test "$ngate" -ne 0; then
		{
			echo "### $track_name Shard"
			echo
			echo '```diff'
			printf '%s\n' "$gate" | sed -n 's/^./- &/p'
			echo '```'
		} >> "$out"
	fi

	test "$ngate" -eq 0
}
