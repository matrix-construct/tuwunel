# Tuwunel for Kubernetes

Tuwunel doesn't support horizontal scalability or distributed loading
natively, however a community maintained Helm Chart is available here to run
Tuwunel on Kubernetes: <https://github.com/AreYouLoco/tuwunel-helm> and the
legacy conduwuit version: <https://gitlab.cronce.io/charts/conduwuit>.

Should changes need to be made, please reach out to the maintainer in our
Matrix room as this is not maintained/controlled by the Tuwunel maintainers.

## Stopping during a migration

The first boot after an upgrade can run a one-time database migration, and the
listener does not open until it finishes. Kubernetes allows 30 seconds by
default, which is not enough on a large database, and a kill part way through
leaves it half migrated. Raise `terminationGracePeriodSeconds` on the pod spec
past the longest migration, and see [Stopping during a
migration](docker.md#stopping-during-a-migration) for the same setting on the
other runtimes.

## Probes during a migration

`tuwunel --health-check` answers whether the server is serving. A migrating
server is not, so a liveness probe with an ordinary failure threshold kills the
container part way through the migration, and under the default restart policy
the replacement is killed the same way on the next attempt. Give the pod a
startup probe whose budget covers the longest migration; while it is failing,
kubelet runs neither the liveness nor the readiness probe, so nothing kills the
pod and nothing routes traffic to it.

```yaml
startupProbe:
  exec: { command: ["tuwunel", "--health-check"] }
  periodSeconds: 10
  failureThreshold: 180        # 30 minutes
livenessProbe:
  exec: { command: ["tuwunel", "--health-check"] }
  periodSeconds: 10
  failureThreshold: 3
readinessProbe:
  exec: { command: ["tuwunel", "--health-check"] }
  periodSeconds: 10
  failureThreshold: 3
```

A pod with no readiness probe at all is marked ready as soon as its container
starts, so a Service will route to a server whose listener has not opened.
Configure all three rather than none.
