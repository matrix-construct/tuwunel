# Migrating Databases

Tuwunel migrates supported RocksDB databases in place during the first startup.
[Deploying](../deploying.md) covers the supported migration paths, including
the database and media steps. This page describes the additional procedure
required to preserve provider-based sign-in to migrated accounts.

Complete this procedure after the first successful Tuwunel startup and before
migrated users sign in through the provider.

## Applicability

| Database and account type | Required action |
|---|---|
| Database created by Tuwunel | No action is required. |
| Migrated database with only password, LDAP, or JWT authentication | No action is required for this procedure. |
| Migrated database containing OAuth or OIDC accounts | Adopt the stored provider associations before users sign in. |

## Prerequisites

1. Complete the database migration and confirm that Tuwunel starts normally.
   An in-place migration must retain the original Matrix server name. Changing
   the server name is not supported.
2. Configure the same identity provider on Tuwunel as a
   `[[global.identity_provider]]` table. It must represent the upstream identity
   system that issued the stored subjects, including the same issuer and subject
   namespace.
3. List the configured providers and note the correct `provider_id`:

   ```text
   !admin query oauth list-providers
   ```

> [!WARNING]
> The source database stores subjects without identifying the
> provider that issued them. Tuwunel cannot verify your provider selection. If
> you select a different provider, an unrelated identity with the same subject
> value could gain access to a migrated account. Do not use bulk adoption if
> the source provider cannot be confirmed or if the source server changed
> providers during the lifetime of the database. Use the
> [per-user association procedure](providers.md#admin-approved-association-for-untrusted-providers)
> instead.

## Adopt provider associations

Run the following command once, substituting the confirmed provider ID:

```text
!admin query oauth adopt <provider_id>
```

If the database does not contain provider mappings in the recognized source
format, the command makes no changes and reports:

```text
No foreign identity column was found.
```

Otherwise, the command scans the stored subject mappings and reports a set of
counters:

```text
adopted=412 already=0 collision=1 absent=3 invalid=0
```

## Interpret the result

| Counter | Meaning | Next step |
|---|---|---|
| `adopted` | A new association points the stored subject to the intended Matrix account. | None. |
| `already` | An existing association points to the intended Matrix account. | None. This is expected for previously adopted rows. |
| `collision` | An existing association cannot be confirmed as the intended one. | Inspect the warning. Tuwunel preserves the existing association. |
| `absent` | The stored local username does not resolve to a usable account. | Check for a missing account or stale source record. |
| `invalid` | The stored subject or local username is not valid UTF-8. | Inspect the source record. Do not assume an association exists. |

The command never replaces an existing association. It is safe to rerun with
the same confirmed provider after correcting a reported condition. Rows counted
as `adopted` become `already` on later runs; `collision`, `absent`, and `invalid`
rows retain their status until the underlying condition changes.

Running the command with a different provider creates a separate set of
associations. It does not replace or remove associations created by an earlier
run. If the wrong provider was selected, prevent further provider logins and
repair those associations individually before continuing.

Each new association is committed independently. If a database read error
causes the command to fail, associations written earlier in the scan remain in
place. Correct the read error and rerun the command.

## Verify account access

Review every nonzero `collision`, `absent`, or `invalid` count. Then have at
least one migrated user sign in through the provider and confirm that the user
reaches the original Matrix account with its rooms, profile, and history.

After the first successful provider login, the following command can display
the user's OAuth sessions:

```text
!admin query oauth show-user @alice:example.com
```

Adopted associations do not appear in either `query oauth list-users` or
`query oauth show-user` before that first login. Their absence at that stage
does not mean that adoption failed.

## If a user has already signed in

Without a provider association, the next provider login does not reliably
select the migrated account. Depending on the provider configuration, Tuwunel
may create a separate account with a claim-derived or generated local username,
or it may reject the login. The original account and its data remain intact;
existing signed-in clients or configured recovery methods may still provide
access.

Before repairing the association, confirm the separate account and the
original account:

1. Display the separate account's OAuth sessions and identify the session for
   the migrated provider:

   ```text
   !admin query oauth show-user @separate:example.com
   ```

2. Pass that session ID to the destructive `!admin query oauth delete` command
   with its required `force` option. Deleting only the relevant session
   preserves any unrelated provider sessions on the account.
3. Ensure that the affected identity does not begin another provider login
   before adoption completes. A concurrent login can recreate the conflicting
   association.
4. Rerun adoption for the confirmed provider:

   ```text
   !admin query oauth adopt <provider_id>
   ```

   Compare the result with the previous run. The `adopted` count should
   increase, the `collision` count should decrease, and the corresponding
   warning should no longer appear.

This repair changes the provider association only. It does not delete or merge
the separate Matrix account, move activity between accounts, or revoke Matrix
devices and access tokens. After adoption succeeds, the next provider login
reaches the original account. Existing Matrix sessions on the separate account
remain active until they are revoked separately.

If bulk adoption cannot be used safely, remove the conflicting OAuth state and
follow the
[per-user association procedure](providers.md#admin-approved-association-for-untrusted-providers).
The pending per-user association must be consumed by a login before Tuwunel is
restarted.

## Why this procedure is necessary

During the database migration, Tuwunel reads the source subject mappings to
restore the state of provider-authenticated accounts. Those mappings contain a
provider subject and Matrix local username, but no provider identity. Tuwunel's
durable login association combines the configured provider identity with the
subject, so only the operator can select the correct provider.

The adoption command creates that durable association without replacing an
existing one. It does not modify the migrated Matrix account or its data.

## Related documentation

- [Identity Providers](providers.md) covers provider configuration and the
  per-user association procedure.
- [Deploying](../deploying.md) covers the database and media portions of a
  migration from conduwuit or Conduit.
