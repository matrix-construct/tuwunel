> [!NOTE]
> Skip this chapter if you are setting up a fresh install of Tuwunel.

## Migrating an existing server to Tuwunel

> [!CAUTION]
> **Always back up your database before migrating.** Migrating into Tuwunel from a source listed
> above is safe: Tuwunel recognizes a foreign database and reconciles its schema version on first
> boot. Switching a database between two other forks of Conduit can still permanently corrupt it,
> because all derivatives share one linear database version with no awareness of each other.

#### Migrating from conduwuit

Migrating from conduwuit to Tuwunel _just works_. In technical parlance it is a "binary swap."
All you have to do is update to the latest Tuwunel and change the path to the executable from
`conduwuit` to `tuwunel`.

Anything else named "conduwuit" is still recognized, this includes environment variables with prefixes
such as `CONDUWUIT_`. In fact, `CONDUIT_` is still recognized for our legacy users. You may have
noticed that various configs, yamls, services, users, and other items were renamed, but if you
were a conduwuit user we recommend against changing anything at all. This will keep things simple.
If you are not sure please ask. If you found out that something did in fact need to be changed
please open an issue immediately.

#### Migrating from Conduit or a fork of conduwuit

A RocksDB database from Conduit or a fork of conduwuit migrates in place on first boot. Stop the
source server, back up its data directory and media, then start Tuwunel against it. Tuwunel
recognizes the foreign database, reconciles its schema version, and carries over room history,
account data, and media automatically; no flags are required. See the
[deploying guide](deploying.md) for media-layout notes that apply to older Conduit databases.
