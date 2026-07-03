# MatrixRTC/Element Call Setup

MatrixRTC is the modern Matrix calling framework used by Element Call and
other recent clients. The media itself is carried by a Selective Forwarding
Unit (SFU); Tuwunel does not embed an SFU, so an external one is required.
[Livekit](https://github.com/livekit/livekit) is the SFU implementation
supported here, paired with the
[lk-jwt-service](https://github.com/element-hq/lk-jwt-service) which issues
the access tokens clients use to join Livekit rooms.

This guide shows you how to deploy MatrixRTC/Element Call using Docker and
Docker Compose, as Livekit only provides prebuilt Docker images. It is
possible to run Livekit using their installation script, however this method
is not supported or recommended.

> [!NOTE]
> In the following documentation, `yourdomain.com` is whatever you have set
> as `server_name` in your `tuwunel.toml`. This needs to be replaced with the
> actual domain. It is assumed that you will be hosting MatrixRTC at
> `matrix-rtc.yourdomain.com`. If you wish to host this service at a
> different subdomain, this needs to be replaced as well.

> [!NOTE]
> This guide provides example configuration for Caddy, Nginx, and Traefik
> reverse proxies. Others can be used, but the configuration will need to be
> adapted.

## 1. Set Up DNS

Create a DNS record for `matrix-rtc.yourdomain.com` pointing to your server.

## 2. Initial Setup

1. Create a directory for your MatrixRTC setup,
   e.g. `mkdir /opt/matrix-rtc`.
2. Change into that directory, e.g. `cd /opt/matrix-rtc`.
3. The following steps will require a key and a secret, referred to as
   `MRTCKEY` and `MRTCSECRET` hereafter. It is suggested that `MRTCKEY` is
   20 characters and `MRTCSECRET` is 64 characters. If you have `pwgen`
   installed, you can generate these with `pwgen -s -1 20` for `MRTCKEY`
   and `pwgen -s -1 64` for `MRTCSECRET`. Make a note of these values for
   use in later steps.

### 2.1 Create Docker Compose Containers

Note: If you are using plain Docker rather than Docker Compose, skip to
step 2.2.

1. Create and open a `compose.yaml` file in your MatrixRTC directory,
   e.g. `nano compose.yaml`.
2. Add the following. `MRTCKEY` and `MRTCSECRET` should be replaced with
   the values generated above.

```yaml
services:
  matrix-rtc-jwt:
    image: ghcr.io/element-hq/lk-jwt-service:latest
    container_name: matrix-rtc-jwt
    environment:
      - LIVEKIT_JWT_BIND=:8081
      - LIVEKIT_URL=wss://matrix-rtc.yourdomain.com
      - LIVEKIT_KEY=MRTCKEY # Random 20 character string
      - LIVEKIT_SECRET=MRTCSECRET # Random 64 character string
      - LIVEKIT_FULL_ACCESS_HOMESERVERS=yourdomain.com # Your server_name from tuwunel.toml
    restart: unless-stopped
    ports:
      - "8081:8081"

  matrix-rtc-livekit:
    image: livekit/livekit-server:latest
    container_name: matrix-rtc-livekit
    command: --config /etc/livekit.yaml
    restart: unless-stopped
    volumes:
      - ./livekit.yaml:/etc/livekit.yaml:ro
    network_mode: "host"
    # Uncomment the lines below and comment `network_mode: "host"` above to specify port mappings.
#    ports:
#      - "7880:7880/tcp"
#      - "7881:7881/tcp"
#      - "50100-50200:50100-50200/udp"
```

### 2.2 Create Livekit Configuration

1. Create and open a `livekit.yaml` file in your MatrixRTC directory,
   e.g. `nano livekit.yaml`.
2. Add the following. `MRTCKEY` and `MRTCSECRET` should be replaced with
   the values generated above.

```yaml
port: 7880
bind_addresses:
  - ""
rtc:
  tcp_port: 7881
  port_range_start: 50100
  port_range_end: 50200
  use_external_ip: true
  enable_loopback_candidate: false
keys:
  MRTCKEY: MRTCSECRET
```

> [!NOTE]
> The `enable_loopback_candidate` option above causes Livekit to include
> `127.0.0.1` and `::1` as ICE host candidates. It is intended for deployments
> where the public IP is mapped to the loopback interface on the host. Set it
> to `true` if you have that specific topology. If calls fail only for clients
> on the same LAN as the server, see the Troubleshooting section.

The `use_external_ip` option above makes Livekit discover the server's
public IP via STUN and advertise it to clients. This suits a host whose
public IP is not bound to a local network interface, such as a cloud VM
behind 1:1 NAT (AWS, GCP) or a server behind a router.

If the public IP is bound directly to a network interface, which is
typical for a VPS (Hetzner, OVH, netcup, and similar), prefer setting
the address explicitly. Replace `use_external_ip: true` with the
following, where `EXTERNAL_IP` is the server's public IP:

```yaml
rtc:
  # ...
  use_external_ip: false
  node_ip: "EXTERNAL_IP"
```

This matters because Livekit, running with host networking, gathers
candidate addresses from every interface on the host, including the
Docker bridge gateways (`docker0` and per-network `br-*` addresses,
typically `172.17.x.x` and up). These cannot be reached from outside,
but with `use_external_ip: true` they are still advertised to clients
as ICE host candidates, which can delay or prevent calls from
connecting. Setting `node_ip` advertises only the given address; note
that it only takes effect while `use_external_ip` is `false`. The
built-in TURN server (described further below) also allocates its relay
addresses at the node IP.

If your server is behind NAT with a dynamic public address, keep
`use_external_ip: true` and add `external_ip_only: true` to the `rtc`
block to suppress the bridge addresses instead.

## 3. Configure .well-known

### 3.1. .well-known served by Tuwunel

*Follow this step if your `.well-known` configuration is served by Tuwunel.
Otherwise follow Step 3.2.*

1. Open your `tuwunel.toml` file, e.g. `nano /etc/tuwunel/tuwunel.toml`.
2. Find the line reading `#livekit_url = ""` and replace it with:

```toml
livekit_url = "https://matrix-rtc.yourdomain.com"
```

3. Ensure that you have `[global.well_known]` uncommented above this line.
   `.well-known` will not be served correctly otherwise.

### 3.2. .well-known served independently

*Follow this step if you serve your `.well-known/matrix` files directly.
Otherwise follow Step 3.1.*

1. Open your `.well-known/matrix/client` file,
   e.g. `nano /var/www/.well-known/matrix/client`.
2. Add the following to the end of this file:

```json
  "org.matrix.msc4143.rtc_foci": [
    {
      "type": "livekit",
      "livekit_service_url": "https://matrix-rtc.yourdomain.com"
    }
  ]
```

The final file should look something like this:

```json
{
  "m.homeserver": {
    "base_url":"https://matrix.yourdomain.com"
  },
  "org.matrix.msc4143.rtc_foci": [
    {
      "type": "livekit",
      "livekit_service_url": "https://matrix-rtc.yourdomain.com"
    }
  ]
}
```

## 4. Configure Firewall

You will need to allow ports `7881/tcp` and `50100:50200/udp` through your
firewall. If you use UFW: `ufw allow 7881/tcp` and
`ufw allow 50100:50200/udp`.

If you are behind NAT, you will also need to forward `7880/tcp`, `7881/tcp`,
and `50100:50200/udp` to livekit.

## 5. Configure Reverse Proxy

As reverse proxies can be installed in different ways, step-by-step
instructions are not given for this section. If you use Caddy, follow
step 5.1; Nginx, follow step 5.2; Traefik, follow step 5.3.

### 5.1. Caddy

1. Add the following to your Caddyfile. If you are running Caddy in Docker,
   replace `localhost` with `matrix-rtc-jwt` in the first instance, and
   `matrix-rtc-livekit` in the second.

```
matrix-rtc.yourdomain.com {
    # This is matrix-rtc-jwt
    @jwt_service {
        path /sfu/get* /healthz* /get_token*
    }
    handle @jwt_service {
        reverse_proxy localhost:8081
    }
    # This is livekit
    handle {
        reverse_proxy localhost:7880 {
            header_up Connection "upgrade"
            header_up Upgrade {http.request.header.Upgrade}
        }
    }
}
```

2. Restart Caddy.

### 5.2. Nginx

1. Add the following to your Nginx configuration. If you are running Nginx
   in Docker, replace `localhost` with `matrix-rtc-jwt` in the first
   instance, and `matrix-rtc-livekit` in the second.

```
server {
    listen 443 ssl;
    listen [::]:443 ssl;
    http2 on;
    server_name matrix-rtc.yourdomain.com;

    # Logging
    access_log /var/log/nginx/matrix-rtc.yourdomain.com.log;
    error_log /var/log/nginx/matrix-rtc.yourdomain.com.error;

    # TLS example for certificate obtained from Let's Encrypt.
    ssl_certificate /etc/letsencrypt/live/matrix-rtc.yourdomain.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/matrix-rtc.yourdomain.com/privkey.pem;

    # lk-jwt-service
    location ~ ^/(sfu/get|healthz|get_token) {
        proxy_pass http://localhost:8081;

        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Server $host;
        proxy_set_header X-Real-IP $remote_addr;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
    }
    # livekit
    location / {
       proxy_pass http://localhost:7880;
       proxy_http_version 1.1;

       proxy_set_header Connection "upgrade";
       proxy_set_header Upgrade $http_upgrade;

       proxy_set_header Host $host;
       proxy_set_header X-Forwarded-Server $host;
       proxy_set_header X-Real-IP $remote_addr;
       proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;
       proxy_set_header X-Forwarded-Proto $scheme;

        # Optional timeouts per LiveKit
        proxy_read_timeout 300s;
        proxy_send_timeout 300s;
    }
}
```

2. Restart Nginx.

### 5.3. Traefik

1. Add `matrix-rtc-jwt` and `matrix-rtc-livekit` to your Traefik network:

```yaml
services:
  matrix-rtc-jwt:
    # ...
    networks:
        - proxy # your traefik network name

  matrix-rtc-livekit:
    # ...
    networks:
        - proxy # your traefik network name

networks:
    proxy: # your traefik network name
        external: true
```

2. Configure routing with either of the methods below.

**2.1 Labels**

```yaml
services:
  matrix-rtc-jwt:
    # ...
    labels:
        - "traefik.enable=true"
        - "traefik.http.routers.matrixrtcjwt.entrypoints=websecure"
        - "traefik.http.routers.matrixrtcjwt.rule=Host(`matrix-rtc.yourdomain.com`) && (PathPrefix(`/sfu/get`) || PathPrefix(`/healthz`) || PathPrefix(`/get_token`))"
        - "traefik.http.routers.matrixrtcjwt.tls=true"
        - "traefik.http.routers.matrixrtcjwt.service=matrixrtcjwt"
        - "traefik.http.services.matrixrtcjwt.loadbalancer.server.port=8081"
        - "traefik.http.routers.matrixrtcjwt.tls.certresolver=yourcertresolver" # change to your cert resolver's name
        - "traefik.docker.network=proxy" # your traefik network name

  matrix-rtc-livekit:
    # ...
    labels:
        - "traefik.enable=true"
        - "traefik.http.routers.livekit.entrypoints=websecure"
        - "traefik.http.routers.livekit.rule=Host(`matrix-rtc.yourdomain.com`)"
        - "traefik.http.routers.livekit.tls=true"
        - "traefik.http.routers.livekit.service=livekit"
        - "traefik.http.services.livekit.loadbalancer.server.port=7880"
        - "traefik.http.routers.livekit.tls.certresolver=yourcertresolver" # change to your cert resolver's name
        - "traefik.docker.network=proxy" # your traefik network name
```

**2.2 Config file**

```yaml
http:
    routers:
        matrixrtcjwt:
            entryPoints:
                - "websecure"
            rule: "Host(`matrix-rtc.yourdomain.com`) && (PathPrefix(`/sfu/get`) || PathPrefix(`/healthz`) || PathPrefix(`/get_token`))"
            tls:
                certResolver: "yourcertresolver" # change to your cert resolver's name
            service: matrixrtcjwt
        livekit:
            entryPoints:
                - "websecure"
            rule: "Host(`matrix-rtc.yourdomain.com`)"
            tls:
                certResolver: "yourcertresolver" # change to your cert resolver's name
            service: livekit
    services:
        matrixrtcjwt:
            loadBalancer:
                servers:
                    - url: "http://matrix-rtc-jwt:8081"
                passHostHeader: true
        livekit:
            loadBalancer:
                servers:
                    - url: "http://matrix-rtc-livekit:7880"
                passHostHeader: true
```

## 6. Start Docker Containers

Note: If you are using Docker Compose, follow 6.1. If you are using plain
Docker, follow 6.2.

### 6.1 Using Docker Compose

1. Ensure you are in your matrix-rtc directory, e.g. `cd /opt/matrix-rtc`.
2. Start containers: `docker compose up -d`.

### 6.2 Using Docker Run

1. Start `matrix-rtc-jwt`. `MRTCKEY` and `MRTCSECRET` should be replaced
   with the values generated in Step 2. `matrix-rtc.yourdomain.com` should
   be replaced with your MatrixRTC subdomain. `yourdomain.com` should be
   replaced with what you have set as `server_name` in `tuwunel.toml`.

```
docker run -d \
  --restart unless-stopped \
  --name matrix-rtc-jwt \
  -p 8081:8081 \
  -e LIVEKIT_JWT_BIND=":8081" \
  -e LIVEKIT_URL="wss://matrix-rtc.yourdomain.com" \
  -e LIVEKIT_KEY="MRTCKEY" \
  -e LIVEKIT_SECRET="MRTCSECRET" \
  -e LIVEKIT_FULL_ACCESS_HOMESERVERS="yourdomain.com" \
  ghcr.io/element-hq/lk-jwt-service:latest
```

2. Start `matrix-rtc-livekit`:

```
docker run -d \
  --restart unless-stopped \
  --name matrix-rtc-livekit \
  --network host \
  -v /opt/matrix-rtc/livekit.yaml:/etc/livekit.yaml:ro \
  livekit/livekit-server:latest \
  --config /etc/livekit.yaml
```

Element Call should now be working.

## Additional Configuration

### External TURN Integration

If you follow this guide and also set up Coturn as per the
[TURN](turn.md) documentation, there will be a port clash between the two
services. To avoid this, the following must be added to your `coturn.conf`:

```
min-port=50201
max-port=65535
```

If you have Coturn configured, you can use it as a TURN server for Livekit
to improve call reliability. As Coturn allows multiple instances of
`static-auth-secret`, it is suggested that the secret used for Livekit is
different to that used for Tuwunel.

1. Create a secret for Coturn; a random 64-character alphanumeric string is
   suggested.
2. Add the following line to the end of your `coturn.conf`, where
   `AUTH_SECRET` is the secret created in Step 1:

```
static-auth-secret=AUTH_SECRET
```

3. Add the following to the end of the `rtc` block in your `livekit.yaml`.
   `AUTH_SECRET` is the same as above. `turn.yourdomain.com` should be
   replaced with your actual TURN domain.

```
  turn_servers:
    - host: turn.yourdomain.com
      port: 5349
      protocol: tls
      secret: "AUTH_SECRET"
```

### Using the Livekit Built-In TURN Server

Livekit includes a built-in TURN server which can be used in place of an
external option. This TURN server will only work with Livekit and is not
compatible with traditional Matrix calling. For that, see the
[TURN documentation](turn.md).

The TURN server allocates relay addresses at the node IP, so the
`use_external_ip` and `node_ip` guidance in step 2.2 applies here as
well; if Livekit runs in Docker and TURN fails, revisit that step.

#### Basic Setup

The simplest way to enable this is to add the following to your `livekit.yaml`:

```
turn:
  enabled: true
  udp_port: 3478
  relay_range_start: 50300
  relay_range_end: 65535
  domain: matrix-rtc.yourdomain.com
```

It is strongly recommended that you use `network_mode: "host"`; however if
port mappings are necessary, add the following ports to `matrix-rtc-livekit`
in your `compose.yaml`:

```
ports:
      - 3478:3478/udp
      - 50300-65535:50300-65535/udp
```

You will need to allow ports `3478` and `50300:65535/udp` through your
firewall. If you use UFW: `ufw allow 3478` and `ufw allow 50300:65535/udp`.

#### Setup With TLS

To enable TLS for the TURN server, the process is slightly more complicated.
Some WebRTC software will not accept certificates provided by Let's Encrypt;
it is therefore suggested that you use [ZeroSSL](https://zerossl.com/) as an
alternative.

1. Create a DNS record for e.g. `matrix-turn.yourdomain.com` pointing to
   your server.
2. Get a certificate for this subdomain.
3. Add the certificates as volumes for `matrix-rtc-livekit` in your
   `compose.yaml`. For example:

```
volumes:
      - ./certs/privkey.pem:/certs/privkey.pem:ro
      - ./certs/fullchain.pem:/certs/fullchain.pem:ro
```

4. Add the following to the bottom of your `livekit.yaml`. The values for
   `cert_file` and `key_file` should match where these files are mounted in
   the container.

```
turn:
  enabled: true
  udp_port: 3478
  tls_port: 5349
  relay_range_start: 50300
  relay_range_end: 65535
  external_tls: false
  domain: matrix-turn.yourdomain.com
  cert_file: /certs/fullchain.pem
  key_file: /certs/privkey.pem
```

5. It is strongly recommended that you use `network_mode: "host"`; however
   if port mappings are necessary, add the following ports to
   `matrix-rtc-livekit` in your `compose.yaml`:

```
ports:
      - 3478:3478/udp
      - 5349:5349/tcp
      - 50300-65535:50300-65535/udp
```

6. You will need to allow ports `3478`, `5349`, and `50300:65535/udp`
   through your firewall. If you use UFW: `ufw allow 3478`,
   `ufw allow 5349`, and `ufw allow 50300:65535/udp`.
7. Restart the containers.

## Troubleshooting

The easiest way to test your configuration is using the `testmatrix` utility
[provided by spaetz](https://codeberg.org/spaetz/testmatrix). This can be
installed using `pip install testmatrix` if you have Python and pip installed.

To use this utility to test your call setup, you will need an access token
for your account. This can be most easily found at the bottom of the
"Help & About" section of the Element Web settings, or in the "Developer
Tools" section of the Cinny settings.

Once you have `testmatrix` installed, run the following (`YOUR_TOKEN` must
be replaced with the access token from your client described above):

```
testmatrix -u @your-user:yourdomain.com -t YOUR_TOKEN yourdomain.com
```

The output of this command will give you information on whether calls are
properly set up.

If all tests are successful, you will get credentials that can be used with
the [Livekit Connection Tester](https://livekit.com/webrtc/connection-test).
This can be used to test the ability of your Livekit service to route calls.

If any of these tests fail, further information can be found in the container
logs. Run `docker compose logs --follow` in the directory where your
`compose.yaml` is located.

### Calls connect for some clients but not others

A common failure mode is that calls work for clients outside your local
network but fail for clients on the same LAN as the Livekit server, and
the failure is browser-dependent (Firefox often breaks while Chromium
succeeds, or vice versa).

This is usually a NAT routing issue rather than a Livekit configuration
issue. Livekit advertises the server's public IP as an ICE candidate via
STUN. Clients on the same LAN that try to reach that public IP must
traverse the router back into the LAN, a feature called "NAT loopback"
or "hairpin NAT". Some routers do not support it, and in that case
local clients cannot establish a media path to any candidate that names
the public IP. Browsers differ in how aggressively they retry
alternative candidates and in how they handle host candidates under
mDNS obfuscation, so the same network can break one browser while
another works.

The reliable fix is to make local clients resolve the MatrixRTC
subdomain to the LAN address, so they bypass the router for that
traffic. Most routers expose this as a "local DNS override", "DNS
rewrite", or "host override" setting. Alternatively, run a small local
DNS resolver (dnsmasq, AdGuard Home, Pi-hole) on the network and point
LAN clients at it. External clients continue to resolve the subdomain
to the public IP via public DNS.

### Docker bridge addresses advertised as candidates

With host networking, Livekit gathers candidate addresses from every
interface on the host, including Docker bridge gateways (`docker0` and
per-network `br-*` addresses, typically `172.17.x.x` and up). If calls
fail to connect and the Livekit container log shows bridge addresses in
its `using external IPs` line, clients are being offered unreachable
candidates. See step 2.2 for the `node_ip` and `external_ip_only`
settings that suppress them.
