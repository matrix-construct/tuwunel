# Generic deployment documentation

> ### Getting help
>
> If you run into any problems while setting up Tuwunel [open an issue on
> GitHub](https://github.com/matrix-construct/tuwunel/issues/new).

## Installing Tuwunel

### Static prebuilt binary

You may simply download the binary that fits your machine architecture (x86_64
or aarch64). Run `uname -m` to see what you need.

Prebuilt fully static binaries can be downloaded from the latest tagged
release [here](https://github.com/matrix-construct/tuwunel/releases/latest) or
`main` CI branch workflow artifact output. These also include `.deb` packages
for Debian or Ubuntu and `.rpm` packages for Red Hat or Fedora.

For the **best** performance; if using an `x86_64` CPU made in the last ~10 years,
we recommend using the `-v3-` optimised packages. See below for a command to check
what your system supports. If the server refuses to start or exits with an "Illegal
Instruction" error you will need `-v2-` or `-v1-` packages instead. The database
backend, RocksDB, benefits from `-v2-` or greater as it features performance
critical hardware accelerated CRC32 hashing/checksumming.

Linux users can run this script to display which optimization levels they may
choose:
```
cat /proc/cpuinfo | grep -Po '(avx|sse)[235]' | sort -u | sed 's/avx5/v4/;s/avx2/v3/;s/sse3/v2/;s/sse2/v1/' | sort
```

### Compiling

Alternatively, you may compile the binary yourself. We recommend using
Nix to build tuwunel as this has the most
guaranteed reproducibiltiy and easiest to get a build environment and output
going. This also allows easy cross-compilation.

You can run the `nix build -L .#static-x86_64-linux-musl-all-features` or
`nix build -L .#static-aarch64-linux-musl-all-features` commands based
on architecture to cross-compile the necessary static binary located at
`result/bin/tuwunel`. This is reproducible with the static binaries produced
in our CI.

If wanting to build using standard Rust toolchains, make sure you install:
- `liburing-dev` on the compiling machine, and `liburing` on the target host
- LLVM and libclang for RocksDB

You can build Tuwunel using `cargo build --release --all-features`

## Adding a Tuwunel user

While Tuwunel can run as any user it is better to use dedicated users for
different services. This also allows you to make sure that the file permissions
are correctly set up.

In Debian, you can use this command to create a Tuwunel user:

```bash
sudo adduser --system tuwunel --group --disabled-login --no-create-home
```

For distros without `adduser` (or where it's a symlink to `useradd`):

```bash
sudo useradd -r --shell /usr/bin/nologin --no-create-home tuwunel
```

## Forwarding ports in the firewall or the router

Matrix's default federation port is port 8448, and clients must be using port 443.
If you would like to use only port 443, or a different port, you will need to setup
delegation. Tuwunel has config options for doing delegation, or you can configure
your reverse proxy to manually serve the necessary JSON files to do delegation
(see the `[global.well_known]` config section).

If Tuwunel runs behind a router or in a container and has a different public
IP address than the host system these public ports need to be forwarded directly
or indirectly to the port mentioned in the config.

Note for NAT users; if you have trouble connecting to your server from the inside
of your network, you need to research your router and see if it supports "NAT
hairpinning" or "NAT loopback".

If your router does not support this feature, you need to research doing local
DNS overrides and force your Matrix DNS records to use your local IP internally.
This can be done at the host level using `/etc/hosts`. If you need this to be
on the network level, consider something like NextDNS or Pi-Hole.

## Setting up a systemd service

Two example systemd units for Tuwunel can be found
[on the configuration page](../configuration/examples.md#debian-systemd-unit-file).
You may need to change the `ExecStart=` path to where you placed the Tuwunel
binary if it is not `/usr/bin/tuwunel`.

On systems where rsyslog is used alongside journald (i.e. Red Hat-based distros
and OpenSUSE), put `$EscapeControlCharactersOnReceive off` inside
`/etc/rsyslog.conf` to allow color in logs.

If you are using a different `database_path` other than the systemd unit
configured default `/var/lib/tuwunel`, you need to add your path to the
systemd unit's `ReadWritePaths=`. This can be done by either directly editing
`tuwunel.service` and reloading systemd, or running `systemctl edit tuwunel.service`
and entering the following:

```
[Service]
ReadWritePaths=/path/to/custom/database/path
```

## Creating the Tuwunel configuration file

Now we need to create the Tuwunel's config file in
`/etc/tuwunel/tuwunel.toml`. The example config can be found at
[tuwunel-example.toml](../configuration/examples.md).

**Please take a moment to read the config. You need to change at least the
server name.**

RocksDB is the only supported database backend.

## Setting the correct file permissions

If you are using a dedicated user for Tuwunel, you will need to allow it to
read the config. To do that you can run this:

```bash
sudo chown -R root:root /etc/tuwunel
sudo chmod -R 755 /etc/tuwunel
```

If you use the default database path you also need to run this:

```bash
sudo mkdir -p /var/lib/tuwunel/
sudo chown -R tuwunel:tuwunel /var/lib/tuwunel/
sudo chmod 700 /var/lib/tuwunel/
```

## Setting up the Reverse Proxy

We recommend Caddy as a reverse proxy, as it is trivial to use, handling TLS certificates, reverse proxy headers, etc transparently with proper defaults.
For other software, please refer to their respective documentation or online guides.

### Caddy

After installing Caddy via your preferred method, create `/etc/caddy/conf.d/tuwunel_caddyfile`
and enter this (substitute for your server name).

```caddyfile
your.server.name, your.server.name:8448 {
    # TCP reverse_proxy
    reverse_proxy localhost:8008
    # UNIX socket
    #reverse_proxy unix//run/tuwunel/tuwunel.sock
}
```

That's it! Just start and enable the service and you're set.

```bash
sudo systemctl enable --now caddy
```

### Other Reverse Proxies

As we would prefer our users to use Caddy, we will not provide configuration files for other proxies.

You will need to reverse proxy everything under following routes:
- `/_matrix/` - core Matrix C-S and S-S APIs
- `/_tuwunel/` - ad-hoc Tuwunel routes such as `/local_user_count` and
`/server_version`

You can optionally reverse proxy the following individual routes:
- `/.well-known/matrix/client` and `/.well-known/matrix/server` if using
Tuwunel to perform delegation (see the `[global.well_known]` config section)
- `/.well-known/matrix/support` if using Tuwunel to send the homeserver admin
contact and support page (formerly known as MSC1929)
- `/` if you would like to see `hewwo from tuwunel woof!` at the root

See the following spec pages for more details on these files:
- [`/.well-known/matrix/server`](https://spec.matrix.org/latest/client-server-api/#getwell-knownmatrixserver)
- [`/.well-known/matrix/client`](https://spec.matrix.org/latest/client-server-api/#getwell-knownmatrixclient)
- [`/.well-known/matrix/support`](https://spec.matrix.org/latest/client-server-api/#getwell-knownmatrixsupport)

Examples of delegation:
- <https://matrix.org/.well-known/matrix/server>
- <https://matrix.org/.well-known/matrix/client>

For Apache and Nginx there are many examples available online.

Lighttpd is not supported as it seems to mess with the `X-Matrix` Authorization
header, making federation non-functional. If a workaround is found, feel free to share to get it added to the documentation here.

If using Apache, you need to use `nocanon` in your `ProxyPass` directive to prevent httpd from messing with the `X-Matrix` header (note that Apache isn't very good as a general reverse proxy and we discourage the usage of it if you can).

If using Nginx, you need to give Tuwunel the request URI using `$request_uri`, or like so:
- `proxy_pass http://127.0.0.1:6167$request_uri;`
- `proxy_pass http://127.0.0.1:6167;`

Nginx users need to increase `client_max_body_size` (default is 1M) to match
`max_request_size` defined in tuwunel.toml.

## You're done

Now you can start Tuwunel with:

```bash
sudo systemctl start tuwunel
```

Set it to start automatically when your system boots with:

```bash
sudo systemctl enable tuwunel
```

## How do I know it works?

You can open [a Matrix client](https://matrix.org/ecosystem/clients), enter your
homeserver and try to register.

You can also use these commands as a quick health check (replace
`your.server.name`).

```bash
curl https://your.server.name/_tuwunel/server_version

# If using port 8448
curl https://your.server.name:8448/_tuwunel/server_version

# If federation is enabled
curl https://your.server.name:8448/_matrix/federation/v1/version
```

- To check if your server can talk with other homeservers, you can use the
[Matrix Federation Tester](https://federationtester.matrix.org/). If you can
register but cannot join federated rooms check your config again and also check
if the port 8448 is open and forwarded correctly.

# What's next?

## Audio/Video calls

For Audio/Video call functionality see the [TURN Guide](../turn.md).

## Appservices

If you want to set up an appservice, take a look at the [Appservice
Guide](../appservices.md).
