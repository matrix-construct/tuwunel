# Example: using the root domain as the homeserver name

[<= Back to Generic Deployment Guide](generic.md#quick-overview)

It is possible to host tuwunel on a subdomain such as `matrix.example.com` but delegate from `example.com` as the server name. This means that usernames will be `@user:example.com` rather than `@user:matrix.example.com`.

Federating servers and clients accessing tuwunel at `example.com` will attempt to discover the subdomain by accessing the `example.com/.well-known/matrix/client` and `example.com/.well-known/matrix/server` endpoints. These need to be set up to point back to `matrix.example.com`.

> [!NOTE]  
> In all of the following examples, replace `matrix.example.com` with the subdomain where tuwunel is hosted, `<PORT>` with the external port for federation, and `example.com` with the domain you want to use as the public-facing homeserver.

## Configuration

Make sure the following are set in your [configuration file](../configuration/examples.md#example-configuration) or via [environment variables](../configuration.md#environment-variables):

1. [Server name](../configuration/examples.md#example-configuration): set `TUWUNEL_SERVER_NAME=example.com` or in the configuration file:
    ```toml,hidelines=~
    [global]
    ~
    ~# The server_name is the pretty name of this server. It is used as a
    ~# suffix for user and room IDs/aliases.
    ~#
    ~# See the docs for reverse proxying and delegation:
    ~# https://tuwunel.chat/deploying/generic.html#setting-up-the-reverse-proxy
    ~#
    ~# Also see the `[global.well_known]` config section at the very bottom.
    ~#
    ~# Examples of delegation:
    ~# - https://matrix.org/.well-known/matrix/server
    ~# - https://matrix.org/.well-known/matrix/client
    ~#
    ~# YOU NEED TO EDIT THIS. THIS CANNOT BE CHANGED AFTER WITHOUT A DATABASE
    ~# WIPE.
    ~#
    ~# example: "girlboss.ceo"
    ~#
    server_name = example.com
    ```
2. [Client-server URL](../configuration/examples.md#example-configuration): set `TUWUNEL_WELL_KNOWN__CLIENT=https://matrix.example.com` or in the configuration file:
    ```toml,hidelines=~
    [global.well_known]
    ~
    ~# The server URL that the client well-known file will serve. This should
    ~# not contain a port, and should just be a valid HTTPS URL.
    ~#
    ~# example: "https://matrix.example.com"
    ~#
    client = https://matrix.example.com
    ```
3. [Server-server federation domain and port](../configuration/examples.md#example-configuration): where `<PORT>` is the external port for federation (default 8448, but often 443 when reverse proxying), set `TUWUNEL_WELL_KNOWN__SERVER=matrix.example.com:<PORT>` or in the configuration file:
    ```toml,hidelines=~
    [global.well_known]
    ~
    ~# The server URL that the client well-known file will serve. This should
    ~# not contain a port, and should just be a valid HTTPS URL.
    ~#
    ~# example: "https://matrix.example.com"
    ~#
    ~client = https://matrix.example.com
    ~
    ~# The server base domain of the URL with a specific port that the server
    ~# well-known file will serve. This should contain a port at the end, and
    ~# should not be a URL.
    ~#
    ~# example: "matrix.example.com:443"
    ~#
    server = matrix.example.com:<PORT> # e.g. matrix.example.com:443
    ```

## Serving `.well-known` endpoints

With the above configuration, tuwunel will generate and serve the appropriate `/.well-known/matrix` entries for delegation, so these can be served by reverse proxying `/.well-known/matrix` on `example.com` to tuwunel. Alternatively, if `example.com` is not behind a reverse proxy, static JSON files can be served directly.

### Option 1: Static JSON files

At a minimum, the following JSON files should be created:

1. At `example.com/.well-known/matrix/client`:
    ```json
    {
        "m.homeserver": {
            "base_url": "https://matrix.example.com/"
        }
    }
    ```
2. At `example.com/.well-known/matrix/server` (substituting `<PORT>` as above):
    ```json
    {
        "m.server": "matrix.example.com:<PORT>" // e.g. "matrix.example.com:443"
    }
    ```

### Option 2: Reverse proxy

These are example configurations if `example.com` is reverse-proxied behind Nginx, Caddy or Traefik.

> [!NOTE]  
> Replace `tuwunel` with the URL where tuwunel is listening; this may look like `127.0.0.1:8008`, `matrix.example.com`, or `tuwunel` if you declared an `upstream tuwunel` block.

> [!IMPORTANT]  
> These configurations need to be applied to the reverse proxy for `example.com`, **not** `matrix.example.com`.

#### Caddy

<!-- from https://github.com/spantaleev/matrix-docker-ansible-deploy/blob/c9bb48ff110dfca73946c69780ef8633e87b22f9/docs/configuring-well-known.md?plain=1#L150,L156 -->

```caddy
example.com {
	reverse_proxy /.well-known/matrix/* https://matrix.example.com {
		header_up Host {upstream_hostport}
	}
}
```

#### Nginx

```nginx,hidelines=~
server {
  ~listen 443 ssl http2;
  ~listen [::]:443 ssl http2;
  server_name example.com;

  set $backend "tuwunel";

  location /.well-known/matrix/ {
    proxy_pass http://$backend:6167$request_uri;
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_ssl_server_name on;
  }
  ~
  ~# The remainder of your nginx configuration for example.com including SSL termination, other locations, etc.
}
```

#### Traefik

Change Tuwunel's router rule to:

```
Host(`matrix.example.com`) || Host(`example.com`) && PathPrefix(`/.well-known/matrix`)
```

##### Labels example

```yaml
- "traefik.http.routers.tuwunel.rule=Host(`matrix.example.com`) || Host(`example.com`) && PathPrefix(`/.well-known/matrix`)"
- "traefik.http.routers.tuwunel-secure.rule=Host(`matrix.example.com`) || Host(`example.com`) && PathPrefix(`/.well-known/matrix`)"
```

##### File example

```yaml
rule: "Host(`matrix.example.com`) || Host(`example.com`) && PathPrefix(`/.well-known/matrix`)"
```

## Testing

Navigate to `example.com/.well-known/matrix/client` and `example.com/.well-known/matrix/server`. These should display results similar to the [JSON snippets above](#option-1-static-json-files).

Entering `example.com` in the [Matrix federation tester](https://federationtester.matrix.org/) should also work.

## Additional resources

For a more complete guide, see the Matrix setup with Ansible and Docker [documentation on setting up `.well-known`](https://github.com/spantaleev/matrix-docker-ansible-deploy/blob/master/docs/configuring-well-known.md).
