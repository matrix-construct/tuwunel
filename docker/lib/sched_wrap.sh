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

# sched_cpus pins the workload to a CPU list in taskset syntax. The value
# pcores names the performance cores of a hybrid CPU, leaving its efficiency
# cores to the rest of the host; on a host without them it pins nothing.
if test "${sched_cpus:-}" = "pcores"; then
	sched_cpus=""
	if test -r /sys/devices/cpu_core/cpus; then
		sched_cpus=$(< /sys/devices/cpu_core/cpus)
	fi
fi

if test -n "${sched_cpus:-}"; then
	if taskset -c "${sched_cpus}" true 2>/dev/null; then
		sched="${sched} taskset -c ${sched_cpus}"
	else
		echo "sched_wrap: taskset -c ${sched_cpus} rejected, running unpinned" >&2
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
	export "${runner}=${sched# }${!runner:+ ${!runner}}"
	exec "$@"
fi

# Exec the workload under the prefix so its scheduling policy, CPU affinity,
# niceness and IO class are inherited by every process it spawns. The unquoted
# expansion is intentional: $sched splits into the leading words of the exec
# argv.
exec ${sched} "$@"
