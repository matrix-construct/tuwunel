# Tuwunel for Red Hat

Information about downloading and deploying the Red Hat package. This may also be
referenced for other `rpm`-based distros such as CentOS.

### Installation

It is recommended to see the [generic deployment guide](https://matrix-construct.github.io/tuwunel/deploying/generic.html)
for further information if needed as usage of the RPM package is generally
related.

A [COPR repository](https://copr.fedorainfracloud.org/coprs/trapacid/tuwunel/)
serves the stable releases for `x86_64` and `aarch64`. Builds are provided for
Fedora, RHEL, CentOS Stream, AlmaLinux, Amazon Linux, Azure Linux, and
openEuler; the full list of targets is on the project page.

```sh
sudo dnf install 'dnf-command(copr)'
sudo dnf copr enable trapacid/tuwunel
sudo dnf install tuwunel
```

On distributions where the `copr` plugin is unavailable, download the `.repo`
file for your release from the
[COPR project page](https://copr.fedorainfracloud.org/coprs/trapacid/tuwunel/)
into `/etc/yum.repos.d/` instead.

### Migrating from another homeserver

Homeservers of the Conduit lineage (including forks) cannot run alongside
Tuwunel and must be uninstalled first. The package declares a conflict with
them, so the transaction is refused rather than leaving two servers claiming
one database directory:

```sh
sudo dnf remove conduwuit
```

Installing the Tuwunel package then adopts the existing database by moving it
to `/var/lib/tuwunel`; nothing is copied or deleted, and the data is migrated
on the next startup. Databases are discovered at `/var/lib/conduwuit` and
`/var/lib/matrix-conduit`, and also under `/var/lib/private`, where systemd
keeps the state of a service that ran with `DynamicUser=`. The locations they
used are left behind as symlinks into `/var/lib/tuwunel`, so a package removed
later reaches a symlink rather than the database.

Adoption is skipped while an old homeserver unit is still active, and a
database on its own filesystem is never moved, since moving it across
filesystems would copy the whole database. Stop the old unit, or mount that
filesystem at `/var/lib/tuwunel`, then finish the migration with:

```sh
sudo /usr/libexec/tuwunel/adopt-legacy-database
```

That command is safe to run at any time and does nothing once the database is
in place. Port the settings from your old configuration (especially
`server_name`) into `/etc/tuwunel/tuwunel.toml` before starting the service.
Removing Tuwunel leaves `/var/lib/tuwunel` and its contents alone.

### Configuration

When installed, the example config is placed at `/etc/tuwunel/tuwunel.toml`
as the default config. The config mentions things required to be changed before
starting.

You can tweak more detailed settings by uncommenting and setting the config
options in `/etc/tuwunel/tuwunel.toml`.

### Running

The package uses the [`tuwunel.service`](https://matrix-construct.github.io/tuwunel/configuration/examples.html#red-hat-systemd-unit-file)
systemd unit file to start and stop Tuwunel. The binary is installed at `/usr/sbin/tuwunel`.

A `tuwunel.socket` unit is installed alongside it, disabled, for deployments
that want systemd to open the listening socket. It is what lets the server
answer on a privileged port such as 443 or 8448 while holding no capability of
its own. See [systemd socket activation](https://matrix-construct.github.io/tuwunel/deploying/socket-activation.html)
before enabling it, since a passed socket is served in addition to the address
in the configuration file rather than replacing it.

This package assumes by default that Tuwunel will be placed behind a reverse
proxy. The default config options apply (listening on `localhost` and TCP port
`8008`). Matrix federation requires a valid domain name and TLS, so you will
need to set up TLS certificates and renewal for it to work properly if you
intend to federate.

Consult various online documentation and guides on setting up a reverse proxy
and TLS. Caddy is documented at the [generic deployment guide](https://matrix-construct.github.io/tuwunel/deploying/generic.html#setting-up-the-reverse-proxy)
as it's the easiest and most user friendly.

### SELinux

On systems with SELinux enabled, the `tuwunel-selinux` subpackage is installed
automatically. It provides the `tuwunel_t` domain together with file contexts
for the binary, `/etc/tuwunel`, `/var/lib/tuwunel`, and `/run/tuwunel`, so no
manual labeling is required. The policy covers the client and federation
listeners and outbound federation.

The domain runs enforcing. If a denial does occur on your setup, inspect it
with `ausearch -m avc -ts recent | grep tuwunel`, report it to the
[issue tracker](https://github.com/matrix-construct/tuwunel/issues), and as a
temporary measure the domain can be switched to permissive with
`semanage permissive -a tuwunel_t` (revert with `semanage permissive -d
tuwunel_t` once resolved).

A reverse proxy running as `httpd_t` (nginx, Apache) may connect to a listener
on a unix socket under `/run/tuwunel` without further configuration. Proxying
to the TCP listener instead is governed by the distribution booleans:

```sh
setsebool -P httpd_can_network_connect 1
```

Paths configured outside the packaged locations, such as a database backup
directory, need a file context of their own:

```sh
semanage fcontext -a -t tuwunel_var_lib_t '/opt/tuwunel-db-backups(/.*)?'
restorecon -R /opt/tuwunel-db-backups
```
