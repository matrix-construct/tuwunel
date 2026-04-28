# Tuwunel for NixOS

Tuwunel can be acquired by Nix from various places:

* The `flake.nix` at the root of the repo
* The `default.nix` at the root of the repo
* From Tuwunel's binary cache

A community maintained NixOS package is available at [`tuwunel`](https://search.nixos.org/packages?channel=unstable&show=tuwunel&from=0&size=50&sort=relevance&type=packages&query=tuwunel)

### NixOS module

A NixOS module ships with Nixpkgs as [`services.matrix-tuwunel`][tuwunel-module],
available in 25.11 and unstable. It generates `tuwunel.toml` from a `settings` attrset
and runs the server under a hardened systemd unit (`DynamicUser`, `ProtectSystem=strict`,
strict `SystemCallFilter`).

Minimal configuration:

```nix
{
  services.matrix-tuwunel = {
    enable = true;
    settings.global = {
      server_name = "example.com";
      address = [ "127.0.0.1" "::1" ];
      port = [ 6167 ];
      allow_federation = true;
    };
  };
}
```

Notable defaults:

* User and group `tuwunel` (override via `services.matrix-tuwunel.user` / `.group`).
* Database under `/var/lib/tuwunel/` (override via `services.matrix-tuwunel.stateDirectory`).
* Listens on `127.0.0.1` and `::1` port `6167`.

Anything placed under `settings.global` is written verbatim into the `[global]` table of
`tuwunel.toml`, so the [configuration reference](../configuration.md) applies directly.

#### UNIX sockets

The module exposes `unix_socket_path` and `unix_socket_perms` directly:

```nix
services.matrix-tuwunel.settings.global = {
  unix_socket_path = "/run/tuwunel/tuwunel.sock";
  unix_socket_perms = 660;
};
```

Leave `address` unset (or `null`) when using a socket. The systemd unit already permits
`AF_UNIX`, so no further overrides are needed.

#### Migrating from `services.matrix-conduit`

`services.matrix-tuwunel` replaces the legacy [`services.matrix-conduit`][conduit-module]
module that older guides reference. Most settings carry over because both render the
same TOML schema. When migrating:

* Disable `services.matrix-conduit` and enable `services.matrix-tuwunel`.
* Confirm the database is RocksDB. Tuwunel dropped SQLite in favor of RocksDB; if you
  ran a SQLite Conduit, migrate first with
  [conduit_toolbox](https://github.com/ShadowJonathan/conduit_toolbox/).
* Either set `services.matrix-tuwunel.stateDirectory` to match your existing
  `database_path`, or move the database under `/var/lib/tuwunel/`.


[tuwunel-module]: https://search.nixos.org/options?channel=unstable&query=services.matrix-tuwunel
[conduit-module]: https://search.nixos.org/options?channel=unstable&query=services.matrix-conduit
