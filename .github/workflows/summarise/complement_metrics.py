#!/usr/bin/env -S python3 -S
"""Aggregate Tuwunel runtime-metrics dumps from a Complement run."""

import argparse, json, re, subprocess, sys, tarfile
from collections import defaultdict
from pathlib import Path

from engine import (
	buffer_runs, load_buffer, render_table, rotate, short_label, this_entry,
	usable, write_buffer, write_out,
)

###############################################################################
# Debug-string parsing
#
# tokio-metrics RuntimeMetrics and nix Usage emit Rust Debug strings in the
# JSON "payload" field because they do not implement Serialize.

DURATION_RE = re.compile(r"(\d+(?:\.\d+)?)(ns|µs|us|ms|s)$")
UNIT_NS     = {"ns": 1, "µs": 1_000, "us": 1_000, "ms": 1_000_000, "s": 1_000_000_000}

RT_INTS = (
	("workers_count",             "workers_count"),
	("total_polls_count",         "total_polls"),
	("total_park_count",          "total_park"),
	("total_steal_count",         "total_steal"),
	("num_remote_schedules",      "remote_schedules"),
	("total_local_schedule_count","local_schedules"),
	("total_overflow_count",      "overflow_count"),
	("budget_forced_yield_count", "budget_yields"),
	("io_driver_ready_count",     "io_driver_ready"),
	("blocking_threads_count",    "blocking_threads"),
	("live_tasks_count",          "live_tasks"),
)
RT_DURS = (
	("total_busy_duration",  "total_busy_ns"),
	("elapsed",              "elapsed_ns"),
	("mean_poll_duration",   "mean_poll_ns"),
	("max_busy_duration",    "max_busy_ns"),
)
USAGE_INTS = (
	("ru_maxrss",   "maxrss_kb"),
	("ru_minflt",   "minflt"),
	("ru_majflt",   "majflt"),
	("ru_inblock",  "inblock"),
	("ru_oublock",  "oublock"),
	("ru_nvcsw",    "nvcsw"),
	("ru_nivcsw",   "nivcsw"),
	("ru_nsignals", "nsignals"),
)

def parse_dur(s):
	m = DURATION_RE.fullmatch(s.strip())
	return int(float(m.group(1)) * UNIT_NS[m.group(2)]) if m else None

def parse_int(s):
	try: return int(s)
	except ValueError: return None

def parse_timeval(body, label):
	m = re.search(rf"{label}: timeval \{{ tv_sec: (-?\d+), tv_usec: (-?\d+) \}}", body)
	return int(m.group(1)) * 1_000_000 + int(m.group(2)) if m else None

def split_kv(text):
	# Flat 'k: v, k: v' parse, treating {}/[]/() as opaque sub-spans.
	out, key, val, in_key, depth = {}, [], [], True, 0
	for ch in text + ",":
		if in_key:
			if ch == ":" and depth == 0: in_key, val = False, []
			else: key.append(ch)
		elif ch in "{[(": depth += 1; val.append(ch)
		elif ch in "}])": depth -= 1; val.append(ch)
		elif ch == "," and depth == 0:
			out["".join(key).strip()] = "".join(val).strip()
			key, val, in_key = [], [], True
		else: val.append(ch)
	return out

def extract_ints(kv, mapping):
	return {dst: v for src, dst in mapping if src in kv for v in [parse_int(kv[src])] if v is not None}

def extract_durs(kv, mapping):
	return {dst: v for src, dst in mapping if src in kv for v in [parse_dur(kv[src])] if v is not None}

def parse_runtime_metrics(payload):
	m = re.fullmatch(r"\s*RuntimeMetrics \{(.*)\}\s*", payload, re.DOTALL)
	if not m: return {}
	kv = split_kv(m.group(1))
	return {**extract_ints(kv, RT_INTS), **extract_durs(kv, RT_DURS)}

def parse_usage(payload):
	m = re.fullmatch(r"\s*Usage\(rusage \{(.*)\}\)\s*", payload, re.DOTALL)
	if not m: return {}
	body = m.group(1)
	out = {}
	for label, dst in (("ru_utime","utime_us"), ("ru_stime","stime_us")):
		v = parse_timeval(body, label)
		if v is not None: out[dst] = v
	body = re.sub(r"ru_[us]time: timeval \{[^}]*\},?\s*", "", body)
	out.update(extract_ints(split_kv(body), USAGE_INTS))
	return out

# `perf stat -ddd -x,` CSV, one event per line:
#   value,unit,event,run_time_ns,run_pct,metric_value,metric_unit
# Hybrid CPUs (e.g. i9-12900K) split each hardware event into cpu_core/<ev>/
# and cpu_atom/<ev>/; we strip the PMU prefix and sum the two. Unmeasured
# events read "<not counted>" / "<not supported>" / empty and are skipped.
# Raw counts outside this allowlist are ignored; derived metrics are read below.
PERF_EVENTS = {
	"instructions":          "perf_instructions",
	"cpu-cycles":            "perf_cycles",
	"branches":              "perf_branches",
	"branch-misses":         "perf_branch_misses",
	"L1-dcache-load-misses": "perf_l1d_misses",
}

# Besides the raw counters, `perf stat` derives a rate for some events and prints
# it in the trailing value/name pair (metric_value, metric_unit = "<unit>  <name>"):
# the cache/TLB miss rates and the TopdownL1 frontend/backend split the hardware
# reports directly. Each is a percentage, kept here as a ratio. The testee runs on
# the P-cores, so we read the cpu_core value (or a non-hybrid line) and drop the
# cpu_atom duplicate, whose tiny counts are background noise.
PERF_METRICS = {
	"llc_miss_rate":      "perf_llc_miss",
	"l1i_miss_rate":      "perf_l1i_miss",
	"dtlb_miss_rate":     "perf_dtlb_miss",
	"tma_frontend_bound": "perf_frontend_bound",
	"tma_backend_bound":  "perf_backend_bound",
}

def perf_event_name(field):
	return re.sub(r"^cpu_(core|atom)/(.*)/$", r"\2", field.strip())

def parse_perf_val(s):
	s = s.strip()
	if not s or s.startswith("<"): return None
	try: return float(s)
	except ValueError: return None

# The PMU behind a metric: a cpu_core/cpu_atom event prefix names it directly; a
# bare topdown node inherits the most recent "TopdownL1 (cpu_<x>)" group instead.
def perf_metric_pmu(event_field, td_pmu):
	m = re.match(r"cpu_(core|atom)/", event_field.strip())
	return m.group(1) if m else td_pmu

def parse_perf_stat(text):
	out, td_pmu = {}, None
	for line in text.splitlines():
		if line.startswith("#") or not line.strip(): continue
		parts = line.split(",")
		if len(parts) < 3: continue
		dst = PERF_EVENTS.get(perf_event_name(parts[2]))
		v = parse_perf_val(parts[0])
		if dst is not None and v is not None:
			out[dst] = out.get(dst, 0.0) + v
		# Trailing derived metric (cpu_core only; cpu_atom is background noise).
		if len(parts) < 7: continue
		tail = parts[6].strip()
		if tail.startswith("TopdownL1"):
			g = re.search(r"cpu_(core|atom)", tail)
			td_pmu = g.group(1) if g else None
		mkey = PERF_METRICS.get(tail.split()[-1]) if tail else None
		mv = parse_perf_val(parts[5])
		if mkey is not None and mv is not None and perf_metric_pmu(parts[2], td_pmu) != "atom":
			out[mkey] = mv / 100.0
	return out

###############################################################################
# Tarball ingestion
#
# Layout: runtime_metrics/by_test/<TestName>/<container_id>/
#             tuwunel.runtime_metrics.<pid>.json
#             tuwunel.runtime_usage.<pid>.json

def open_tar(path):
	# stdlib tarfile lacks zstd until 3.14; pipe through the system tool.
	proc = subprocess.Popen(
		["zstd", "-dc", "--", str(path)],
		stdout=subprocess.PIPE, stderr=subprocess.DEVNULL,
	)
	return tarfile.open(fileobj=proc.stdout, mode="r|"), proc

def is_dump(member):
	return member.isfile() and (member.name.endswith(".json") or "perf_stat" in member.name)

def parse_member_path(name):
	# Returns (test, cid, fname) or None for paths outside by_test/.
	parts = name.strip("/").split("/")
	if "by_test" not in parts: return None
	i = parts.index("by_test")
	return tuple(parts[i+1:i+4]) if len(parts) >= i + 4 else None

def read_dump(tar, member):
	fp = tar.extractfile(member)
	if fp is None: return None
	try: return json.loads(fp.read().decode("utf-8"))
	except (UnicodeDecodeError, json.JSONDecodeError): return None

def read_text(tar, member):
	fp = tar.extractfile(member)
	if fp is None: return None
	try: return fp.read().decode("utf-8")
	except UnicodeDecodeError: return None

def merge_dump(row, fname, dump):
	payload = dump.get("payload", "")
	if "runtime_metrics" in fname: row.update(parse_runtime_metrics(payload))
	elif "runtime_usage" in fname: row.update(parse_usage(payload))

def load_tar(path):
	rows, version = defaultdict(dict), None
	tar, proc = open_tar(path)
	with tar:
		for m in tar:
			if not is_dump(m): continue
			parsed = parse_member_path(m.name)
			if parsed is None: continue
			test, cid, fname = parsed
			row = rows[(test, cid)]
			row["test"], row["cid"] = test, cid
			if "perf_stat" in fname:
				raw = read_text(tar, m)
				if raw: row.update(parse_perf_stat(raw))
				continue
			dump = read_dump(tar, m)
			if dump is None: continue
			version = version or dump.get("meta", {}).get("tuwunel_version")
			merge_dump(row, fname, dump)
	proc.wait()
	return list(rows.values()), version

###############################################################################
# Reduction

def col(rows, key):
	return [r[key] for r in rows if r.get(key) is not None]

def busy_ratio(r):
	b, e, w = r.get("total_busy_ns"), r.get("elapsed_ns"), max(1, r.get("workers_count", 1))
	return b / e / w if b is not None and e else None

def cpu_total_us(r):
	u, s = r.get("utime_us"), r.get("stime_us")
	return u + s if u is not None and s is not None else None

def ratio(r, num, den):
	n, d = r.get(num), r.get(den)
	return n / d if n is not None and d else None

def perf_ipc(r):          return ratio(r, "perf_instructions", "perf_cycles")
def perf_branch_miss(r):  return ratio(r, "perf_branch_misses", "perf_branches")

def derive_col(rows, fn):
	return [v for v in (fn(r) for r in rows) if v is not None]

def pct(xs, q):
	if not xs: return None
	s = sorted(xs)
	if len(s) == 1: return s[0]
	pos = (len(s) - 1) * (q / 100)
	lo, hi = int(pos), min(int(pos) + 1, len(s) - 1)
	return s[lo] + (s[hi] - s[lo]) * (pos - lo)

def safe_max(xs): return max(xs) if xs else None
def safe_sum(xs): return sum(xs) if xs else 0

def aggregate(rows):
	if not rows: return {"testees": 0}
	maxrss = col(rows, "maxrss_kb")
	cpu = derive_col(rows, cpu_total_us)
	busy = derive_col(rows, busy_ratio)
	poll = col(rows, "mean_poll_ns")
	polls = col(rows, "total_polls")
	nvcsw = col(rows, "nvcsw")
	nivcsw = col(rows, "nivcsw")
	inblk = col(rows, "inblock")
	oublk = col(rows, "oublock")
	majflt = col(rows, "majflt")
	minflt = col(rows, "minflt")
	overflow = col(rows, "overflow_count")
	return {
		"testees": len(rows),
		"tests": len({r["test"] for r in rows}),
		"workers_count": rows[0].get("workers_count"),
		"maxrss_kb_median": pct(maxrss, 50),
		"maxrss_kb_p95": pct(maxrss, 95),
		"maxrss_kb_max": safe_max(maxrss),
		"cpu_total_us_median": pct(cpu, 50),
		"cpu_total_us_sum": safe_sum(cpu),
		"busy_ratio_median": pct(busy, 50),
		"busy_ratio_p95": pct(busy, 95),
		"mean_poll_ns_median": pct(poll, 50),
		"mean_poll_ns_p95": pct(poll, 95),
		"polls_median": pct(polls, 50),
		"nvcsw_median": pct(nvcsw, 50),
		"nivcsw_median": pct(nivcsw, 50),
		"inblock_total": safe_sum(inblk),
		"oublock_total": safe_sum(oublk),
		"majflt_total": safe_sum(majflt),
		"minflt_median": pct(minflt, 50),
		"overflow_total": safe_sum(overflow),
		**aggregate_perf(rows),
	}

# Hardware-counter aggregates from `perf stat` (perf testee variant only).
# Absent on ordinary runs, so the perf rows are suppressed there (see render).
def aggregate_perf(rows):
	instr = col(rows, "perf_instructions")
	cyc = col(rows, "perf_cycles")
	l1d = col(rows, "perf_l1d_misses")
	if not (instr or cyc or l1d): return {}
	out = {
		"perf_present": True,
		"perf_instructions_sum": safe_sum(instr),
		"perf_cycles_sum": safe_sum(cyc),
		"perf_ipc_median": pct(derive_col(rows, perf_ipc), 50),
		"perf_branch_miss_median": pct(derive_col(rows, perf_branch_miss), 50),
		"perf_l1d_misses_sum": safe_sum(l1d),
	}
	# Per-testee perf-derived rates reduced to a median; a metric the hardware
	# never reported is left out so its row drops from the table (see render).
	for src in PERF_METRICS.values():
		v = pct(col(rows, src), 50)
		if v is not None: out[f"{src}_median"] = v
	return out

###############################################################################
# Formatters

def fmt_kb(kb):
	if kb is None: return "n/a"
	mib = kb / 1024.0
	return f"{mib/1024:.2f} GiB" if mib >= 1024 else f"{mib:.0f} MiB"

def fmt_us(us):
	if us is None: return "n/a"
	if us >= 1_000_000: return f"{us/1_000_000:.2f} s"
	if us >= 1_000: return f"{us/1_000:.1f} ms"
	return f"{us:.0f} µs"

def fmt_ns(ns):
	if ns is None: return "n/a"
	if ns >= 1_000_000_000: return f"{ns/1_000_000_000:.2f} s"
	if ns >= 1_000_000: return f"{ns/1_000_000:.2f} ms"
	if ns >= 1_000: return f"{ns/1_000:.1f} µs"
	return f"{ns:.0f} ns"

def fmt_ratio(r): return "n/a" if r is None else f"{r:.3f}"
def fmt_int(n): return "n/a" if n is None else (f"{n:,.0f}" if isinstance(n, float) else f"{n:,}")

def fmt_pct(r): return "n/a" if r is None else f"{r * 100:.2f}%"

def fmt_count(n):
	if n is None: return "n/a"
	for div, suf in ((1e9, "G"), (1e6, "M"), (1e3, "k")):
		if abs(n) >= div: return f"{n / div:.2f}{suf}"
	return f"{n:.0f}"

###############################################################################
# Rendering
#
# direction: True  = lower-is-better (RSS, CPU, page faults, ...)
#            False = higher-is-better (busy ratio)
#            None  = neither, no regress/improve marker
# floor:     absolute delta below which we suppress the percent and print "·".

ROWS = (
	("maxrss_kb_median",    "Peak RSS (median)",            fmt_kb,    True,  1024),
	("maxrss_kb_p95",       "Peak RSS (p95)",               fmt_kb,    True,  1024),
	("maxrss_kb_max",       "Peak RSS (max)",               fmt_kb,    True,  1024),
	("cpu_total_us_median", "CPU per testee (median)",      fmt_us,    True,  1_000),
	("cpu_total_us_sum",    "CPU total",                    fmt_us,    True,  1_000_000),
	("busy_ratio_median",   "Runtime busy ratio (median)",  fmt_ratio, False, 0.01),
	("busy_ratio_p95",      "Runtime busy ratio (p95)",     fmt_ratio, False, 0.01),
	("mean_poll_ns_median", "Mean poll duration (median)",  fmt_ns,    True,  1_000),
	("mean_poll_ns_p95",    "Mean poll duration (p95)",     fmt_ns,    True,  1_000),
	("polls_median",        "Polls per testee (median)",    fmt_int,   None,  1),
	("nvcsw_median",        "Voluntary ctxsw (median)",     fmt_int,   None,  1),
	("nivcsw_median",       "Involuntary ctxsw (median)",   fmt_int,   True,  1),
	("inblock_total",       "Block reads (total)",          fmt_int,   True,  1),
	("oublock_total",       "Block writes (total)",         fmt_int,   True,  1),
	("majflt_total",        "Major page faults (total)",    fmt_int,   True,  1),
	("minflt_median",       "Minor page faults (median)",   fmt_int,   None,  1),
	("overflow_total",      "Worker overflow (total)",      fmt_int,   True,  1),
)

# Hardware-counter rows, appended only when a run carries perf-stat data.
PERF_ROWS = (
	("perf_instructions_sum",      "Instructions (total)",         fmt_count, True,  1e9),
	("perf_cycles_sum",            "CPU cycles (total)",           fmt_count, True,  1e9),
	("perf_ipc_median",            "IPC (median)",                 fmt_ratio, False, 0.01),
	("perf_branch_miss_median",    "Branch miss rate (median)",    fmt_pct,   True,  0.001),
	("perf_l1d_misses_sum",        "L1-dcache misses (total)",     fmt_count, True,  1e6),
	("perf_llc_miss_median",       "LLC miss rate (median)",       fmt_pct,   True,  0.001),
	("perf_l1i_miss_median",       "L1-icache miss rate (median)", fmt_pct,   True,  0.001),
	("perf_dtlb_miss_median",      "dTLB miss rate (median)",      fmt_pct,   True,  0.001),
	("perf_frontend_bound_median", "Frontend bound (median)",      fmt_pct,   True,  0.001),
	("perf_backend_bound_median",  "Backend bound (median)",       fmt_pct,   True,  0.001),
)

def header_line(curr, version):
	bits = []
	if version: bits.append(f"tuwunel {version}")
	if curr.get("workers_count"): bits.append(f"{curr['workers_count']}-worker tokio runtime")
	bits.append(f"{curr.get('testees', 0)} testees across {curr.get('tests', 0)} tests")
	return ", ".join(bits) + "."

# Column model. Each column is (header, kind, agg):
#   "this"  this run's value;  "delta"  signed % vs the column's agg (markers);
#   "value" a prior run's raw value (no delta).

def build_columns(curr, main_entry, history):
	cols = [("This run", "this", curr)]
	if main_entry is not None:
		cols.append(("Δ vs main", "delta", main_entry["agg"]))
		cols.append((f"main<br>`{short_label(main_entry)}`", "value", main_entry["agg"]))
	for i, e in enumerate(history, start=1):
		branch = e.get("branch") or "prev"
		cols.append((f"{branch} −{i}<br>`{short_label(e)}`", "value", e["agg"]))
	return cols

def hot_rss(rows, n=5):
	xs = [r for r in rows if r.get("maxrss_kb") is not None]
	xs.sort(key=lambda r: (-r["maxrss_kb"], r["test"]))
	return [(r["test"], fmt_kb(r["maxrss_kb"])) for r in xs[:n]]

def hot_cpu(rows, n=5):
	xs = [(r["test"], cpu_total_us(r)) for r in rows if cpu_total_us(r) is not None]
	xs.sort(key=lambda p: (-p[1], p[0]))
	return [(t, fmt_us(c)) for t, c in xs[:n]]

def render_hot(title, items):
	if not items: return []
	out = ["", title, ""]
	for name, value in items: out.append(f"- `{name}`: {value}")
	return out

# A perf row appears only if some column carries its value; a metric the hardware
# did not report leaves the key absent everywhere and the row is dropped. The full
# set is then sorted lexically by label so every metric reads in one alphabet.
def live_rows(perf_rows, cols):
	return tuple(r for r in perf_rows if any(c[2].get(r[0]) is not None for c in cols))

def render(curr, cols, rows, version):
	out = ["", "#### Runtime metrics", "", header_line(curr, version), ""]
	perf_rows = live_rows(PERF_ROWS, cols) if curr.get("perf_present") else ()
	rowcat = tuple(sorted(ROWS + perf_rows, key=lambda r: r[1].lower()))
	out += render_table("Metric", (), curr, cols, rowcat)
	out += render_hot("Top 5 testees by peak RSS:", hot_rss(rows))
	out += render_hot("Top 5 testees by CPU:", hot_cpu(rows))
	if len(cols) == 1:
		out += ["", "_No baseline yet; history warms up over the next few green runs._"]
	out.append("")
	return "\n".join(out)

###############################################################################
# Entry point

def parse_args():
	ap = argparse.ArgumentParser()
	ap.add_argument("--tar", required=True)
	ap.add_argument("--history-in")
	ap.add_argument("--main-in")
	ap.add_argument("--history-out")
	ap.add_argument("--keep", type=int, default=3)
	ap.add_argument("--out", default="-")
	ap.add_argument("--emit-json")
	ap.add_argument("--no-render", action="store_true")
	return ap.parse_args()

def main():
	args = parse_args()
	if not usable(args.tar): return 0
	rows, version = load_tar(args.tar)
	curr = aggregate(rows)

	prev_own = load_buffer(args.history_in)
	history = buffer_runs(prev_own)[:max(0, args.keep)]
	main_runs = buffer_runs(load_buffer(args.main_in))
	main_entry = main_runs[0] if main_runs else None
	cols = build_columns(curr, main_entry, history)

	if args.emit_json: Path(args.emit_json).write_text(json.dumps(curr, indent=2))
	if args.history_out: write_buffer(args.history_out, rotate(prev_own, this_entry(agg=curr), args.keep))
	# Main is the baseline ring: record the digest for branches to diff against,
	# but render no table (it carries no diff signal, like the suppressed grid).
	if not args.no_render: write_out(args.out, render(curr, cols, rows, version))
	return 0

if __name__ == "__main__":
	sys.exit(main())
