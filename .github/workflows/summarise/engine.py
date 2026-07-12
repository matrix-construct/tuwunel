#!/usr/bin/env -S python3 -S
"""Shared CI-summary engine: the rolling history buffer and run-stamp helpers
common to the runtime-metrics and job-timings summarisers.

A buffer is a small JSON document, `{"v": 1, "runs": [entry, ...]}`, persisted
per branch in the GitHub Actions cache. Each green run prepends its entry and
truncates to a keep depth; reads tolerate a cold (absent) or corrupt cache. An
entry carries the run's identity plus a front-end payload (the metrics `agg` or
the timings `jobs` map)."""

import json, os, sys, time
from pathlib import Path

BUFFER_VERSION = 1

def load_buffer(path):
	if not path or not Path(path).is_file(): return None
	try: buf = json.loads(Path(path).read_text(encoding="utf-8"))
	except (OSError, json.JSONDecodeError): return None
	return buf if isinstance(buf, dict) else None

def buffer_runs(buf):
	runs = buf.get("runs") if buf else None
	return runs if isinstance(runs, list) else []

# Stamp this run: its identity from the environment plus any front-end payload
# fields (agg=... for metrics, jobs=... for timings) passed as keywords.
def this_entry(**payload):
	env = os.environ
	return {
		"run_id":      env.get("GITHUB_RUN_ID", ""),
		"run_attempt": env.get("GITHUB_RUN_ATTEMPT", ""),
		"sha":         env.get("GITHUB_SHA", ""),
		"branch":      env.get("GITHUB_REF_NAME", ""),
		"ts":          int(time.time()),
		**payload,
	}

def rotate(prev, entry, keep):
	return {"v": BUFFER_VERSION, "runs": ([entry] + buffer_runs(prev))[:max(1, keep)]}

def write_buffer(path, buf):
	p = Path(path)
	p.parent.mkdir(parents=True, exist_ok=True)
	p.write_text(json.dumps(buf), encoding="utf-8")

# Short column label for a prior run: its 7-char sha, else the run id.
def short_label(entry):
	sha = (entry.get("sha") or "")[:7]
	return sha or f"run {entry.get('run_id', '?')}"

# Seconds to a compact HhMMmSSs / MmSSs / Ss string; passes "-" (or None) through.
def hms(secs):
	if secs is None or secs == "-": return "-"
	t = int(secs)
	h, m, s = t // 3600, (t % 3600) // 60, t % 60
	if h: return f"{h}h{m:02d}m{s:02d}s"
	if m: return f"{m}m{s:02d}s"
	return f"{s}s"

def usable(path):
	return bool(path) and Path(path).is_file() and Path(path).stat().st_size > 0

def write_out(path, body):
	if not path or path == "-": sys.stdout.write(body)
	else: open(path, "a", encoding="utf-8").write(body)

###############################################################################
# Comparison table
#
# A markdown table shared by the metrics and timings summarisers. A column is
# (header, kind, data[, fmt]): kind "this" reads curr, "delta" prints the signed
# % of curr vs data with ⚠️/✅ markers, "value" prints a raw value from data; the
# optional fmt overrides the row formatter for that column. A row is
# (key, label, fmt, direction, floor, *lead): the lookup key, the label cell, the
# formatter, the delta sense (True lower-is-better, False higher, None none) with
# a floor below which the percent reads "·", then pre-rendered lead cells. Keys
# need not be unique, so sharded jobs (per-row data in lead cells) stay distinct.

def is_regression(direction, delta):
	if direction is None: return None
	return delta > 0 if direction else delta < 0

def fmt_delta(curr, prev, direction, floor):
	if curr is None or prev is None: return ""
	d = curr - prev
	if abs(d) < floor: return "·"
	pct_d = (d / prev * 100.0) if prev else float("inf")
	regress = is_regression(direction, d)
	marker = ""
	if regress is True and abs(pct_d) >= 5: marker = "⚠️ "
	if regress is False and abs(pct_d) >= 5: marker = "✅ "
	sign = "+" if d >= 0 else ""
	return f"{marker}{sign}{pct_d:.1f}%" if prev else f"{marker}{sign}{d:.0f}"

def table_cell(curr, col, key, fmt, direction, floor):
	kind, data = col[1], col[2]
	col_fmt = col[3] if len(col) > 3 else fmt
	# The formatter owns the absent case: the metrics formatters render None as
	# "n/a", hms renders it as "-".
	if kind == "this":
		return col_fmt(curr.get(key))
	if kind == "delta":
		return fmt_delta(curr.get(key), data.get(key), direction, floor)
	return col_fmt(data.get(key))

def render_table(label_header, lead_headers, curr, cols, rows):
	heads = [label_header, *lead_headers] + [c[0] for c in cols]
	lines = ["| " + " | ".join(heads) + " |", "|" + "---|" * len(heads)]
	for row in rows:
		key, label, fmt, direction, floor = row[:5]
		lead = row[5:]
		body = [table_cell(curr, c, key, fmt, direction, floor) for c in cols]
		lines.append("| " + " | ".join([label, *lead, *body]) + " |")
	return lines
