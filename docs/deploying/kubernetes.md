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
