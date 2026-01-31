# Reverse Proxy Setup - Traefik

[<= Back to Generic Deployment Guide](generic.md#setting-up-the-reverse-proxy)

## Installation

Install Traefik via your preferred method. You can read the official [docker quickstart guide](https://doc.traefik.io/traefik/getting-started/docker/) or the [in-depth walkthrough](https://doc.traefik.io/traefik/setup/docker/)

## Configuration
### TLS certificates

You can setup auto renewing certificates with different kinds of [acme challenges](https://doc.traefik.io/traefik/reference/install-configuration/tls/certificate-resolvers/acme/).
### Router configurations
Add tuwunel to your traefik's network.

```yaml
services:
    tuwunel:
    # ...
    networks:
        - proxy # your traefik network name
networks:
    proxy: # your traefik network name
        external: true
```

Be sure to change the `your.server.name` to your actual tuwunel domain. and the `yourcertresolver` should be changed to whatever you named it in your traefik config.

You only have to do any one of these methods below.


### Labels
To use labels with traefik you need to configure a [docker provider](https://doc.traefik.io/traefik/reference/install-configuration/providers/docker/).

Then add the labels in your tuwunel's docker compose file.
```yaml
services:
    tuwunel:
        # ...
        labels:
            - "traefik.enable=true"
            - "traefik.http.routers.tuwunel.entrypoints=web"
            - "traefik.http.routers.tuwunel.rule=Host(`your.server.name`)"
            - "traefik.http.routers.tuwunel.middlewares=https-redirect@file"
            - "traefik.http.routers.tuwunel-secure.entrypoints=websecure"
            - "traefik.http.routers.tuwunel-secure.rule=Host(`your.server.name`)"
            - "traefik.http.routers.tuwunel-secure.tls=true"
            - "traefik.http.routers.tuwunel-secure.service=tuwunel"
            - "traefik.http.services.tuwunel.loadbalancer.server.port=6167"
            - "traefik.http.routers.tuwunel-secure.tls.certresolver=yourcertresolver"
            - "traefik.docker.network=proxy"
```
### Config File
To use the config file you need to configure a [file provider](https://doc.traefik.io/traefik/reference/install-configuration/providers/others/file/).

Then add this into your config file.
```yaml
http:
    routers:
        tuwunel:
            entryPoints:
                - "web"
                - "websecure"
            rule: "Host(`your.server.name`)"
            middlewares:
                - https-redirect
            tls:
                certResolver: "yourcertresolver"
            service: tuwunel
    services:
        tuwunel:
            loadBalancer:
                servers:
            # this url should point to your tuwunel installation.
            # this should work if your tuwunel container is named tuwunel and is in the same network as traefik.
                    - url: "http://tuwunel:6167"
                passHostHeader: true
```
### Federation
If you will use a .well-known file you can use traefik to redirect .well-known/matrix to tuwunel built-in .well-known file.

replace the rule in either of the methods from
```
Host(`your.server.name`)
```
to
```
Host(`your.tuwunel.domain`) || Host(`your.server.name`) && PathPrefix(`/.well-known/matrix`)
```
If you are not using a .well-known file you will need to add and expose port 8448 to a [traefik entrypoint](https://doc.traefik.io/traefik/reference/install-configuration/entrypoints/).

You can then add these to your preferred traefik config method.
you should replace `matrixfederationentry` with what you named your entrypoint.

Labels:
```yaml
            - "traefik.http.routers.matrix-federation.entrypoints=matrixfederationentry"
            - "traefik.http.routers.matrix-federation.rule=Host(`your.server.name`)"
            - "traefik.http.routers.matrix-federation.tls=true"
            - "traefik.http.routers.matrix-federation.service=matrix-federation"
            - "traefik.http.services.matrix-federation.loadbalancer.server.port=6167"
            - "traefik.http.routers.matrix-federation.tls.certresolver=yourcertresolver"
```
Config file:
```yaml
        entryPoints:
            - "web"
            - "websecure"
            - "matrixfederationentry"
```
> [!IMPORTANT]
>
> [Encoded Character Filtering](https://doc.traefik.io/traefik/security/request-path/#encoded-character-filtering)
> options must be set to `true`. This only applies to traefik version 3.6.4 to 3.6.6 and 2.11.32 to 2.11.34


## Verification

After starting Traefik, verify it's working by checking:

```bash
curl https://your.server.name/_tuwunel/server_version
curl https://your.server.name:8448/_tuwunel/server_version
```

---

[=> Continue with "You're Done"](generic.md#you-are-done)
