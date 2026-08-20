# Tuwunel for NixOS

Tuwunel can be acquired by Nix from various places:

* The `flake.nix` at the root of the repo
* The `default.nix` at the root of the repo
* From Tuwunel's [binary cache](#binary-cache)

A community maintained NixOS package is available at [`tuwunel`](https://search.nixos.org/packages?channel=unstable&show=tuwunel&from=0&size=50&sort=relevance&type=packages&query=tuwunel)

### Binary cache

Tuwunel publishes prebuilt store paths, so building from the flake does not
have to compile RocksDB and the Rust toolchain locally. The cache is public
and reading from it needs no account.

| | Substituter | Public key |
|---|---|---|
| Self-hosted | `https://cache.tuwunel.chat` | `cache.tuwunel.chat-1:ZafUaXiRMozDa9N2SWim6EdzH0EEjWjwfvlTxXvcjLA=` |
| Cachix | `https://tuwunel.cachix.org` | `tuwunel.cachix.org-1:VRecUeDcaPxtYDA6bnMF3snPM7VYX8K605z4uuG2nWc=` |

Both are live and either works on its own. Prefer `cache.tuwunel.chat`: it
stores the whole closure, stock nixpkgs paths included, so a build can be
satisfied from it without reaching `cache.nixos.org` at all. The Cachix cache
holds only Tuwunel's own binaries and forked dependencies.

On NixOS, add them to `nix.settings`:

```nix
{
  nix.settings = {
    extra-substituters = [
      "https://cache.tuwunel.chat"
      "https://tuwunel.cachix.org"
    ];
    extra-trusted-public-keys = [
      "cache.tuwunel.chat-1:ZafUaXiRMozDa9N2SWim6EdzH0EEjWjwfvlTxXvcjLA="
      "tuwunel.cachix.org-1:VRecUeDcaPxtYDA6bnMF3snPM7VYX8K605z4uuG2nWc="
    ];
  };
}
```

Everywhere else, put the same two settings in `/etc/nix/nix.conf` and restart
the daemon:

```ini
extra-substituters = https://cache.tuwunel.chat https://tuwunel.cachix.org
extra-trusted-public-keys = cache.tuwunel.chat-1:ZafUaXiRMozDa9N2SWim6EdzH0EEjWjwfvlTxXvcjLA= tuwunel.cachix.org-1:VRecUeDcaPxtYDA6bnMF3snPM7VYX8K605z4uuG2nWc=
```

```bash
sudo systemctl restart nix-daemon
```

Listing a substituter that is unreachable is not fatal. Nix treats it as a
cache miss, warns, and builds from source, so keeping both configured costs
nothing if one is down.

With the `cachix` client installed, `cachix use tuwunel` writes the Cachix half
of that configuration for you.

The repository flake also declares the cache in its `nixConfig`, but do not
rely on that alone. Nix treats a flake's configuration as untrusted: an
interactive build asks whether to accept it, and a non-interactive one skips
it with `ignoring untrusted flake configuration setting`, so an unattended
deployment that configured nothing else would quietly build everything from
source. Set the two values as shown above, or pass `--accept-flake-config`.

`cache.tuwunel.chat` is self-hosted on Tuwunel's own infrastructure, behind a
caching proxy that keeps serving already-fetched paths even if the cache
application itself is down.

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
