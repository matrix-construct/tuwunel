#!/bin/bash
set -eo pipefail

# Assemble an optional scheduling prefix from the environment. Each knob is
# opt-in; unset knobs contribute nothing, so with none set this is a plain exec.
sched=""

# The realtime policies (--rr, --fifo) need CAP_SYS_NICE, which an unprivileged
# build RUN lacks. Probe the policy on a throwaway command and only adopt it when
# permitted, so such a context degrades to normal scheduling instead of failing.
if test -n "${sched_policy:-}"; then
	if chrt "${sched_policy}" "${sched_prio:-0}" true 2>/dev/null; then
		sched="chrt ${sched_policy} ${sched_prio:-0}"
	else
		echo "sched_wrap: chrt ${sched_policy} ${sched_prio:-0} denied, running unscheduled" >&2
	fi
fi

if test -n "${sched_nice:-}"; then
	sched="${sched} nice -n ${sched_nice}"
fi

if test -n "${sched_ionice:-}"; then
	sched="${sched} ionice ${sched_ionice}"
fi

# With sched_scope=runner the prefix becomes cargo's runner for the target, so
# it reaches only the test and bench executables cargo launches while cargo,
# rustc and build scripts keep the default class. A runner already set in the
# environment is kept and runs under the prefix.
if test "${sched_scope:-}" = "runner" && test -n "${sched}"; then
	runner="CARGO_TARGET_$(tr 'a-z.-' 'A-Z__' <<< "${CARGO_TARGET:?sched_scope=runner needs CARGO_TARGET}")_RUNNER"
	export "${runner}=${sched}${!runner:+ ${!runner}}"
	exec "$@"
fi

# Exec the workload under the prefix so its scheduling policy, niceness and IO
# class are inherited by every process it spawns. The unquoted expansion is
# intentional: $sched splits into the leading words of the exec argv.
exec ${sched} "$@"
