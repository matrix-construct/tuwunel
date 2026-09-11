#!/bin/sh
# Writes a seccomp profile that adds the io_uring syscalls to a container
# runtime's default profile.
#
# Both Podman and Docker ship a default profile whose allowlist omits every
# io_uring syscall, so io_uring_setup fails inside a container: ENOSYS under
# Podman, EPERM under Docker. The database engine treats either as "io_uring
# unavailable" and falls back to synchronous reads, which costs read
# performance but is otherwise harmless. Passing the profile this writes to
# --security-opt seccomp= restores io_uring while keeping the rest of the
# filter, which seccomp=unconfined gives up entirely.
#
# AppArmor is not involved. It mediates only io_uring rings created with
# SQPOLL, which the engine does not request, so apparmor=unconfined neither
# helps nor is needed.
#
# Usage:
#     docker/seccomp-io-uring.sh > tuwunel-seccomp.json
#     podman run --security-opt seccomp=./tuwunel-seccomp.json ...
#
# The profile is derived from the runtime's own default so it cannot go stale
# as the base gains syscalls. That base defaults to the file containers-common
# installs, which serves Docker equally well; Docker compiles its own default
# in rather than installing it, so pass a path to fetch moby's
# profiles/seccomp/default.json instead when containers-common is absent.
# Requires jq.
set -eu

if ! command -v jq > /dev/null 2>&1; then
    echo "jq is required" >&2
    exit 1
fi

base="${1:-/usr/share/containers/seccomp.json}"

if ! test -r "$base"; then
    echo "no readable base seccomp profile at $base" >&2
    exit 1
fi

jq '.syscalls += [{
        "names": ["io_uring_enter", "io_uring_register", "io_uring_setup"],
        "action": "SCMP_ACT_ALLOW"
    }]' "$base"
