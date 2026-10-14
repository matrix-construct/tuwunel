#!/bin/bash
set -eo pipefail

# Bound concurrent links across every build step sharing LINKER_LOCK_DIR. This
# is reached through a link named for the real linker (cc, clang) in a
# directory kept out of PATH, so rustc infers the same linker flavor it would
# for the real one; the real linker is that name resolved from PATH. With
# LINKER_JOBS unset this is a plain exec.
name="${0##*/}"
if ! linker=$(type -P "${name}"); then
	echo "linker_wrap: ${name} not found in PATH" >&2
	exit 127
fi

if test "${linker}" -ef "$0"; then
	echo "linker_wrap: ${name} resolves to this wrapper; keep it out of PATH" >&2
	exit 2
fi

jobs="${LINKER_JOBS:-}"
if test -z "${jobs}"; then
	exec "${linker}" "$@"
fi

case "${jobs}" in
*[!0-9]*|0*)
	echo "linker_wrap: LINKER_JOBS must be a positive integer" >&2
	exit 2
	;;
esac

lock_dir="${LINKER_LOCK_DIR:-/var/cache/tuwunel/linker}"
mkdir -p "${lock_dir}"

# A slot is an flock on its file. The linker inherits the descriptor, so the
# slot stays held until the linker and its children exit, however they exit.
# Slots are scanned from the top down: a profile granted fewer slots (the
# heavier LTO links) contends only for the low ones, which lighter links reach
# last. When every slot is busy, block briefly on a random one, then rescan.
while true; do
	for ((slot = jobs - 1; slot >= 0; slot--)); do
		exec {lock}>"${lock_dir}/${slot}"
		if flock -n "${lock}"; then
			exec "${linker}" "$@"
		fi
		exec {lock}>&-
	done

	exec {lock}>"${lock_dir}/$((RANDOM % jobs))"
	if flock -w 1 "${lock}"; then
		exec "${linker}" "$@"
	fi
	exec {lock}>&-
done
