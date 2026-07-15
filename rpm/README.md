# Tuwunel for Red Hat

Information about downloading and deploying the Red Hat package. This may also be
referenced for other `rpm`-based distros such as CentOS.

### Installation

It is recommended to see the [generic deployment guide](../docs/deploying/generic.md)
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

The package uses the [`tuwunel.service`](../docs/configuration/examples.md#red-hat-systemd-unit-file)
systemd unit file to start and stop Tuwunel. The binary is installed at `/usr/sbin/tuwunel`.

This package assumes by default that Tuwunel will be placed behind a reverse
proxy. The default config options apply (listening on `localhost` and TCP port
`8008`). Matrix federation requires a valid domain name and TLS, so you will
need to set up TLS certificates and renewal for it to work properly if you
intend to federate.

Consult various online documentation and guides on setting up a reverse proxy
and TLS. Caddy is documented at the [generic deployment guide](../docs/deploying/generic.md#setting-up-the-reverse-proxy)
as it's the easiest and most user friendly.
