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

Installing the Tuwunel package adopts an existing database automatically by
moving it to `/var/lib/tuwunel`; nothing is copied or deleted, and the data
is migrated on the next startup. Databases are discovered at
`/var/lib/conduwuit` and `/var/lib/matrix-conduit`, and also under
`/var/lib/private`, where systemd keeps the state of services that ran with
`DynamicUser=`. The old locations are left behind as symlinks into
`/var/lib/tuwunel`, so purging the old package after the adoption removes at
most a symlink and can no longer reach the data.

Adoption is skipped while an old homeserver unit is still active, and a
database kept on its own mounted filesystem is never moved; in those cases
stop the old unit, or mount the filesystem at `/var/lib/tuwunel`, and run
`dpkg-reconfigure tuwunel`. If a filesystem is already mounted at
`/var/lib/tuwunel`, move the old database's contents into it instead. Databases from conduwuit and Conduit are
supported; for other forks of the lineage, compatibility varies with how far
the fork has diverged. If a fork keeps its database somewhere else, stop its
service and move that directory to `/var/lib/tuwunel` before installing.

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

A `tuwunel.socket` unit is installed alongside it, disabled, for deployments
that want systemd to open the listening socket. It is what lets the server
answer on a privileged port such as 443 or 8448 while holding no capability of
its own. See [systemd socket activation](https://matrix-construct.github.io/tuwunel/deploying/socket-activation.html)
before enabling it, since a passed socket is served in addition to the address
in the configuration file rather than replacing it.

This package assumes by default that Tuwunel will be placed behind a reverse
proxy. The default config options apply (listening on `localhost` and TCP port
`6167`). Matrix federation requires a valid domain name and TLS, so you will
need to set up TLS certificates and renewal for it to work properly if you
intend to federate.

Consult various online documentation and guides on setting up a reverse proxy
and TLS. Caddy is documented at the [generic deployment guide](https://matrix-construct.github.io/tuwunel/deploying/generic.html#setting-up-the-reverse-proxy)
as it's the easiest and most user friendly.

### AppArmor

The package installs an AppArmor profile at
`/etc/apparmor.d/usr.sbin.tuwunel` and loads it where AppArmor is enabled,
which is the default on Debian and Ubuntu. It takes effect the next time the
service starts. A host without the `apparmor` parser runs the server
unconfined instead of failing to start.

The profile grants no capabilities, because the packaged unit already clears
the capability bounding set, and it confines writes to the same paths the unit
lists in `ReadWritePaths`, plus the runtime directory: `/var/lib/tuwunel`,
`/etc/tuwunel`, and `/run/tuwunel`. Writing to `/etc/tuwunel` is what
[config regeneration](https://matrix-construct.github.io/tuwunel/configuration/regeneration.html)
needs. Reads are wider than writes so a configured TLS certificate or trust
store keeps working, and the server is allowed to re-exec itself so that
`!admin server restart` still works.

The profile pins the AppArmor 3.0 policy abi so it parses on Debian 12 and
Ubuntu 22.04 as well, whose AppArmor userspace predates 4.0.

Because the profile grants no capabilities, a manual command that has to read
the `tuwunel`-owned configuration runs as that user rather than as root, which
needs no capability to override file permissions:

```sh
sudo -u tuwunel tuwunel -c /etc/tuwunel/tuwunel.toml --regenerate-config
```

Deployments that move the database or read TLS material from an unusual
location add those paths to `/etc/apparmor.d/local/usr.sbin.tuwunel`, which
the profile includes if present. A configured `media_video_thumbnail_command`
needs an execute rule rather than a path rule there, such as
`/usr/bin/ffmpeg ix,`, since the profile grants no execute permission of its
own; its staging directory under the database path is already covered:

```sh
echo '/srv/matrix/** rwkl,' >> /etc/apparmor.d/local/usr.sbin.tuwunel
apparmor_parser -r -T -W /etc/apparmor.d/usr.sbin.tuwunel
systemctl restart tuwunel.service
```

Denials are logged to the audit log. Inspect them with
`journalctl -k | grep apparmor` or `aa-status`, and report them to the
[issue tracker](https://github.com/matrix-construct/tuwunel/issues). As a
temporary measure the profile can be put in complain mode with
`aa-complain /usr/sbin/tuwunel`, which logs what it would have denied and
blocks nothing; `aa-enforce /usr/sbin/tuwunel` restores it.

Confinement inside a container is a separate matter, covered by
[container security profiles and limits](https://matrix-construct.github.io/tuwunel/deploying/container-security.html).
