# Reverse Proxy Setup - Caddy

[<= Back to Generic Deployment Guide](generic.md#setting-up-the-reverse-proxy)

We recommend Caddy as a reverse proxy, as it is trivial to use, handling TLS
certificates, reverse proxy headers, etc. transparently with proper defaults.

## Installation

Install Caddy via your preferred method. Refer to the
[official Caddy installation guide](https://caddyserver.com/docs/install) for your distribution.

## Configuration

After installing Caddy, create `/etc/caddy/conf.d/tuwunel_caddyfile` and enter this (substitute
`your.server.name` with your actual server name):

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
## Caddy and .well-known

Caddy can serve `.well-known/matrix/client` and `.well-known/matrix/server` instead
of `tuwunel`. This can be done by using the `respond` directive in your caddyfile. 

Useful if you want to delegate a domain such as `example.com` -> `matrix.example.com`. 

> [!info]
> Note the use of \` (backtick) in the respond directive to escape JSON that
> contains \" (double quotes).

```caddyfile
your.server.name, your.server.name:8848 {

	@matrix path /.well-known/matrix/*
        #Recommended CORS headers (https://spec.matrix.org/v1.17/client-server-api/#well-known-uris) 
	header @matrix {
                        Access-Control-Allow-Origin: *
                        Access-Control-Allow-Methods: GET, POST, PUT, DELETE, OPTIONS
                        Access-Control-Allow-Headers: X-Requested-With, Content-Type, Authorization
        }
        respond /.well-known/matrix/client `{"m.homeserver": {"base_url":"https://<your.server.name>"}, "org.matrix.msc4143.rtc_foci": [{"type": "livekit", "livekit_service_url": "https://<your.matrix-rtc-jwt.server>"}]} `
        respond /.well-known/matrix/server `{"m.server": "<your.server.name>:443"}`
}
```


---

[=> Continue with "You're Done"](generic.md#you-are-done)
