# Tuwunel for Debian

Information about downloading and deploying the Debian package. This may also be
referenced for other `apt`-based distros such as Ubuntu.

### Installation

It is recommended to see the [generic deployment guide](https://matrix-construct.github.io/tuwunel/deploying/generic.html)
for further information if needed as usage of the Debian package is generally
related.

An `apt` repository serves the stable releases for `amd64` and `arm64`. The
package is statically linked, so it works on any current Debian or Ubuntu
release:

```sh
sudo curl -fsSL -o /usr/share/keyrings/tuwunel-archive-keyring.gpg https://apt.f.dog/tuwunel-archive-keyring.gpg
sudo tee /etc/apt/sources.list.d/tuwunel.sources >/dev/null <<EOF
Types: deb
URIs: https://apt.f.dog
Suites: stable
Components: main
Signed-By: /usr/share/keyrings/tuwunel-archive-keyring.gpg
EOF
sudo apt update
sudo apt install tuwunel
```

Previous releases remain available from the repository and can be selected
with, e.g. `apt install tuwunel=1.7.1-1`.

### Migrating from another homeserver

Homeservers of the Conduit lineage (including forks) cannot run alongside
Tuwunel and must be uninstalled first. Remove the old package with
`apt remove`, never `apt purge`, since purging may delete its database:

```sh
sudo apt remove conduwuit
```

Installing the Tuwunel package adopts an existing database from
`/var/lib/conduwuit` or `/var/lib/matrix-conduit` automatically by moving it
to `/var/lib/tuwunel`; nothing is copied or deleted, and the data is migrated
on the next startup. Databases from conduwuit and Conduit are supported; for
other forks of the lineage, compatibility varies with how far the fork has
diverged. If a fork keeps its database somewhere else, stop its service and
move that directory to `/var/lib/tuwunel` before installing.

Port the settings from your old configuration (especially `server_name`) into
`/etc/tuwunel/tuwunel.toml` before starting the service. Uninstalling Tuwunel
never deletes `/var/lib/tuwunel`, even on purge.

### Configuration

When installed, the example config is placed at `/etc/tuwunel/tuwunel.toml`
as the default config. The config mentions things required to be changed before
starting.

You can tweak more detailed settings by uncommenting and setting the config
options in `/etc/tuwunel/tuwunel.toml`.

### Running

The package uses the [`tuwunel.service`](https://matrix-construct.github.io/tuwunel/configuration/examples.html#debian-systemd-unit-file)
systemd unit file to start and stop Tuwunel. The binary is installed at `/usr/sbin/tuwunel`.

This package assumes by default that Tuwunel will be placed behind a reverse
proxy. The default config options apply (listening on `localhost` and TCP port
`6167`). Matrix federation requires a valid domain name and TLS, so you will
need to set up TLS certificates and renewal for it to work properly if you
intend to federate.

Consult various online documentation and guides on setting up a reverse proxy
and TLS. Caddy is documented at the [generic deployment guide](https://matrix-construct.github.io/tuwunel/deploying/generic.html#setting-up-the-reverse-proxy)
as it's the easiest and most user friendly.
