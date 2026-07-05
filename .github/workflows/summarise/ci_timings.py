#!/usr/bin/env -S python3 -S
"""Per-job wall-clock timing for the current run, rendered into the run summary.

Self-hosted runners are absent from the deprecated /timing billable endpoint, so
durations come from the jobs API's started_at/completed_at wall-clock stamps,
which cover every runner type. Nested reusable-workflow jobs (lint/test/package/
publish, all matrix expansions) share this run_id, so one top-level invocation
captures the whole pipeline.

Prior-run columns and the per-branch digest reuse the shared history engine:
when TIMINGS_HISTORY_OUT is set, the digest is read from the Actions cache
(TIMINGS_HISTORY_IN), this run is prepended, the buffer is truncated to
TIMINGS_KEEP, and up to two prior runs render as raw value columns labelled by
short sha."""

import calendar, json, os, sys, time, urllib.request

from engine import (
	buffer_runs, hms, load_buffer, render_table, rotate, short_label,
	this_entry, write_buffer, write_out,
)

# A job whose wall-clock moved by fewer than this many seconds against the prior
# run shows "·" in the Δ column rather than a noisy sub-second percent.
DELTA_FLOOR_S = 1

# Result glyph: ✅ success, ❌ failure, ⬜ skipped, ⬛ cancelled, ⚠️ other terminal
# state, 🟦 still in progress.
GLYPH = {
	"success":     "✅",
	"failure":     "❌",
	"skipped":     "⬜",
	"cancelled":   "⬛",
	"in_progress": "🟦",
	"queued":      "🟦",
	"waiting":     "🟦",
	"pending":     "🟦",
	"requested":   "🟦",
	"":            "🟦",
}

def glyph(result):
	return GLYPH.get(result, "⚠️")

# Parse a jobs-API timestamp ("2026-06-24T08:48:17Z") to a UTC epoch second, or
# None if it does not parse. Only differences are used, so the absolute value
# only needs to be consistent across stamps.
def epoch(ts):
	try:
		return calendar.timegm(time.strptime(ts, "%Y-%m-%dT%H:%M:%SZ"))
	except (ValueError, TypeError):
		return None

# One page at a time until a short page lands. A hand-rolled request keeps the
# step free of any runner-installed CLI, reachable on every runner type with
# only the token already in scope.
def fetch_jobs():
	repo = os.environ["GITHUB_REPOSITORY"]
	run_id = os.environ["GITHUB_RUN_ID"]
	attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "1")
	base = (f"https://api.github.com/repos/{repo}"
	        f"/actions/runs/{run_id}/attempts/{attempt}/jobs")
	headers = {
		"Authorization":        f"Bearer {os.environ['GH_TOKEN']}",
		"Accept":               "application/vnd.github+json",
		"X-GitHub-Api-Version": "2022-11-28",
	}

	jobs = []
	page = 1
	while True:
		req = urllib.request.Request(f"{base}?per_page=100&page={page}", headers=headers)
		with urllib.request.urlopen(req) as resp:
			chunk = (json.load(resp).get("jobs")) or []
		if not chunk:
			break
		jobs += chunk
		if len(chunk) < 100:
			break
		page += 1
	return jobs

def run(jobs):
	out = os.environ.get("GITHUB_STEP_SUMMARY") or "-"

	# Sortable rows ("key<TAB>label<TAB>result<TAB>dur<TAB>name") plus the timed
	# per-job seconds persisted to the digest, and the run-wide aggregates.
	tmp = []
	timed = {}
	total = done_n = run_n = 0
	longest = -1
	longest_name = ""
	min_start = None
	max_end = 0

	for job in jobs:
		name = job.get("name") or ""
		if not name:
			continue

		status = job.get("status") or ""
		started = job.get("started_at") or ""
		completed = job.get("completed_at") or ""

		# Terminal jobs carry a conclusion; in-flight ones report their status.
		result = (job.get("conclusion") or "") if status == "completed" else status

		s = epoch(started)
		c = epoch(completed)

		if result == "skipped":
			# Never dispatched, so there is no interval to time.
			dur, key = "-", 0
		elif started and completed and s is not None and c is not None:
			secs = max(0, c - s)
			dur, key = hms(secs), secs

			total += secs
			done_n += 1
			if secs > longest:
				longest, longest_name = secs, name
			if min_start is None or s < min_start:
				min_start = s
			if c > max_end:
				max_end = c
			timed[name] = secs
		elif status == "completed":
			# Completed without a recorded interval.
			dur, key = "-", 0
		else:
			dur, key = "(running)", 0
			run_n += 1

		# Zero-pad the key so untimed rows (key 0) sink below timed ones; a
		# fixed-width key makes a plain reverse string sort match `sort -k1 -rn`.
		tmp.append(f"{key:012d}\t{glyph(result)} {name}\t{result}\t{dur}\t{name}")

	if not tmp:
		run_id = os.environ.get("GITHUB_RUN_ID", "")
		attempt = os.environ.get("GITHUB_RUN_ATTEMPT", "1")
		write_out(out, f"No jobs reported for run {run_id} (attempt {attempt}).\n")
		return

	# Prior-run history. Up to two value columns, each a prior run's per-job
	# duration keyed by job name, labelled by short sha. Off unless a digest sink
	# is configured.
	history_out = os.environ.get("TIMINGS_HISTORY_OUT", "")
	prev = load_buffer(os.environ.get("TIMINGS_HISTORY_IN", "")) if history_out else None
	priors = [(short_label(e), e.get("jobs") or {}) for e in buffer_runs(prev)[:2]]

	# Result and duration are this row's own cells, so sharded jobs that share a
	# name keep distinct rows. Each prior run adds a raw value column by short
	# sha, and this run's per-job seconds drive the Δ-vs-prev column.
	rows = []
	for row in sorted(tmp, reverse=True):
		_, label, result, dur, name = row.split("\t")
		rows.append((name, label, hms, True, DELTA_FLOOR_S, result, dur))

	# A duplicated name (sharded jobs) has no per-row prior to compare against,
	# since the digest keeps one figure per name; leave its Δ blank rather than
	# print a collapsed value that contradicts the row's own duration.
	names = [r[0] for r in rows]
	delta_curr = {k: v for k, v in timed.items() if names.count(k) == 1}

	cols = []
	if priors:
		cols.append(("Δ vs prev", "delta", priors[0][1]))
	for i, (label, pjobs) in enumerate(priors, start=1):
		cols.append((f"−{i} `{label}`", "value", pjobs))

	lines = ["### CI Timings", ""]
	lines += render_table("Job", ("Result", "Duration"), delta_curr, cols, rows)

	lines.append("")
	if min_start is not None and max_end > min_start:
		lines.append(f"**Run wall-clock:** {hms(max_end - min_start)}  ")
	if longest >= 0:
		lines.append(f"**Longest job:** {longest_name} ({hms(longest)})  ")
	tail = f"**Total job-time:** {hms(total)} summed across {done_n} job(s)"
	if run_n > 0:
		tail += f"; {run_n} still running"
	lines.append(tail + ".")

	write_out(out, "\n".join(lines) + "\n")

	# Rotate the digest: prepend this run, truncate to keep (>=1). The read above
	# already completed, so an in==out path is safe to overwrite.
	if history_out:
		try:
			keep = int(os.environ.get("TIMINGS_KEEP", "3"))
		except ValueError:
			keep = 3
		if keep < 1:
			keep = 3
		entry = this_entry(run_number=os.environ.get("GITHUB_RUN_NUMBER", ""), jobs=timed)
		write_buffer(history_out, rotate(prev, entry, keep))

def main():
	run(fetch_jobs())
	return 0

if __name__ == "__main__":
	sys.exit(main())
