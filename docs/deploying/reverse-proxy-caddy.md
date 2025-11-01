# Reverse Proxy Setup - Caddy

[<= Back to Generic Deployment Guide](generic.md#setting-up-the-reverse-proxy)

We recommend Caddy as a reverse proxy, as it is trivial to use, handling TLS certificates, reverse proxy headers, etc. transparently with proper defaults.

## Installation

Install Caddy via your preferred method. Refer to the [official Caddy installation guide](https://caddyserver.com/docs/install) for your distribution.

## Configuration

After installing Caddy, create `/etc/caddy/conf.d/tuwunel_caddyfile` and enter this (substitute `your.server.name` with your actual server name):

```caddyfile
your.server.name, your.server.name:8448 {
    # TCP reverse_proxy
    reverse_proxy localhost:8008
    # UNIX socket (alternative - comment out the line above and uncomment this)
    #reverse_proxy unix//run/tuwunel/tuwunel.sock
}
```

### What this does

- Handles both port 443 (HTTPS) and port 8448 (Matrix federation) automatically
- Automatically provisions and renews TLS certificates via Let's Encrypt
- Sets all necessary reverse proxy headers correctly
- Routes all traffic to Tuwunel listening on `localhost:8008`

That's it! Just start and enable the service and you're set.

```bash
sudo systemctl enable --now caddy
```

## Verification

After starting Caddy, verify it's working by checking:

```bash
curl https://your.server.name/_tuwunel/server_version
curl https://your.server.name:8448/_tuwunel/server_version
```

---

[=> Continue with "You're Done"](generic.md#youre-done)
