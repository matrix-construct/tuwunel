# Container security profiles and limits

Tuwunel runs unprivileged under every container runtime's default security
profiles, and needs none of them relaxed. What can stop it is a resource
ceiling rather than a security profile, and the two are worth separating
before changing either, because the advice that circulates for this server
relaxes considerably more than the situation calls for.

## Task limits, the default that does stop the server

Podman limits a container to 2,048 tasks by default. Docker takes its limit
from the daemon configuration, so it varies by platform; the reports behind
this page came from hosts where it was also 2,048.

The database pool sizes itself from the host's core count, and on a host with
many cores the pool alone reaches that ceiling. The server then fails during
startup:

```
Critical error starting server: I/O error: Resource temporarily unavailable (os error 11)
```

That `EAGAIN` comes from thread creation, not from the database. Because
`db_pool_max_workers` defaults to 2,048, a 32 core host asks for 2,048 pool
threads before the tokio workers and the RocksDB background jobs, and all of
them count against the one cgroup limit.

Tuwunel warns when it can see that collision coming, naming both numbers:

```
WARN The database pool may exceed this container's task limit; raise the task limit
(--pids-limit for docker and podman) or lower db_pool_max_workers.
total_workers=2048 max_tasks=2048
```

Either remedy works. Raising the runtime's limit keeps the pool at full size:

| Runtime         | Setting                          |
| --------------- | -------------------------------- |
| Docker, Podman  | `--pids-limit=16384`             |
| Podman quadlet  | `PodmanArgs=--pids-limit=16384`  |
| docker compose  | `pids_limit: 16384`              |
| Kubernetes      | the node's `podPidsLimit`        |

Lowering the pool instead costs some read concurrency and nothing else:

```
-e TUWUNEL_DB_POOL_MAX_WORKERS=512
```

## seccomp, and what it costs io_uring

The default seccomp profiles of both Podman and Docker allow none of the three
io_uring syscalls, so `io_uring_setup` fails inside a container: `ENOSYS` under
Podman, `EPERM` under Docker.

This is harmless. The database engine reads either answer as io_uring being
unavailable and falls back to synchronous reads, per thread, without
interrupting startup. A container that never touches its seccomp profile is a
fully working homeserver that gives up some read performance.

To get io_uring back, add those three syscalls to the runtime's own default
profile rather than switching the filter off. Deriving the profile from the
installed default keeps it from going stale as that default gains syscalls:

```sh
jq '.syscalls += [{"names":["io_uring_enter","io_uring_register","io_uring_setup"],
                   "action":"SCMP_ACT_ALLOW"}]' \
   /usr/share/containers/seccomp.json > tuwunel-seccomp.json

podman run --security-opt seccomp=./tuwunel-seccomp.json ...
```

`docker/seccomp-io-uring.sh` in the source tree is the same command with a
usage message. Docker compiles its default profile in rather than installing
it, so the file above serves Docker too; where `containers-common` is absent,
pass a copy of moby's `profiles/seccomp/default.json` instead.

`--security-opt seccomp=unconfined` also restores io_uring, by discarding the
whole syscall filter along the way. The generated profile differs from the
runtime's default by one allowed syscall group.

## AppArmor and SELinux do not gate io_uring

AppArmor mediates io_uring only for rings created with `SQPOLL`. The database
engine asks for `SINGLE_ISSUER` and `DEFER_TASKRUN` and never for `SQPOLL`, so
`apparmor=unconfined` is neither needed nor sufficient. Measured on Debian 13
with kernel 6.12, under rootful Podman with `containers-default` enforcing and
under Docker with `docker-default` enforcing:

| AppArmor   | seccomp           | ring the engine creates | an `SQPOLL` ring |
| ---------- | ----------------- | ----------------------- | ---------------- |
| enforcing  | runtime default   | fails                   | fails            |
| unconfined | runtime default   | fails                   | fails            |
| enforcing  | the profile above | succeeds                | Podman denies it |

Only the last row distinguishes the two, and only for a ring tuwunel does not
create: `containers-default` grants no `io_uring sqpoll` permission and
`docker-default` does. Nothing in the engine's path depends on it.

Rootless Podman applies no AppArmor profile at all, so the option changes
nothing there either.

An unlimited `memlock` and `CAP_IPC_LOCK` are equally unnecessary. Ring memory
has been charged to the cgroup rather than to `RLIMIT_MEMLOCK` since Linux
5.12, and a ring still initializes with `memlock` set to zero.

Under SELinux, a bind-mounted database directory needs a label the container
can write to, which the `:Z` volume suffix applies:

```sh
podman run -v /srv/tuwunel:/var/lib/tuwunel:Z ...
```

A named volume is labeled correctly without the suffix. Confinement of a host
install is separate from all of this and is documented per package: the RPM
ships an SELinux policy module, and the Debian package ships an AppArmor
profile.
