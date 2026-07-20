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

### Configuration

When installed, the example config is placed at `/etc/tuwunel/tuwunel.toml`
as the default config. The config mentions things required to be changed before
starting.

You can tweak more detailed settings by uncommenting and setting the config
options in `/etc/tuwunel/tuwunel.toml`.

### Running

The package uses the [`tuwunel.service`](https://matrix-construct.github.io/tuwunel/configuration/examples.html#red-hat-systemd-unit-file)
systemd unit file to start and stop Tuwunel. The binary is installed at `/usr/sbin/tuwunel`.

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
