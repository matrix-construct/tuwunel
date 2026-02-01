# Matrix RTC/Element Call Setup

## Notes
- `yourdomain.com` is whatever you have set as `server_name` in your tuwunel.toml. This needs to be replaced with the actual domain. It is assumed that you will be hosting MatrixRTC at `matrix-rtc.yourdomain.com`. If you wish to host this service at a different subdomain, this needs to be replaced as well.
- This guide provides example configuration for Caddy and Nginx reverse proxies. Others can be used, but the configuration will need to be adapted.

## Instructions
### 1. Set Up DNS
Create a DNS record for `matrix-rtc.yourdomain.com` pointing to your server.

### 2. Create Docker Containers
1. Create a directory for your MatrixRTC setup e.g. `mkdir /opt/matrix-rtc`.
2. Change directory to your new directory. e.g. `cd /opt/matrix-rtc`.
3. Create and open a compose.yaml file for MatrixRTC. e.g. `nano compose.yaml`.
4. Add the following. `mrtckey` and `mrtcsecret` should be random strings. It is suggested that `mrtckey` is 20 characters and `mrtcsecret` is 64 characters.
```yaml
services:
  matrix-rtc-jwt:
    image: ghcr.io/element-hq/lk-jwt-service:latest
    container_name: matrix-rtc-jwt
    environment:
      - LIVEKIT_JWT_PORT=8081
      - LIVEKIT_URL=wss://matrix-rtc.yourdomain.com
      - LIVEKIT_KEY=mrtckey
      - LIVEKIT_SECRET=mrtcsecret
      - LIVEKIT_FULL_ACCESS_HOMESERVERS=yourdomain.com
    restart: unless-stopped
    ports:
      - "8081:8081"

  matrix-rtc-livekit:
    image: livekit/livekit-server:latest
    container_name: matrix-rtc-livekit
    command: --config /etc/livekit.yaml
    ports:
      - 7880:7880/tcp
      - 7881:7881/tcp
      - 50100-50200:50100-50200/udp
    restart: unless-stopped
    volumes:
      - ./livekit.yaml:/etc/livekit.yaml:ro
```
4. Close the file.
5. Create and open a livekit.yaml file. e.g. `nano livekit.yaml`.
6. Add the following. `mrtckey` and `mrtcsecret` should be the same as those from compose.yaml. 
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
  mrtckey: "mrtcsecret"
```
7. Close the file.

### 3. Configure .well-known
#### 3.1. .well-known served by Tuwunel
***Follow this step if your .well-known configuration is served by tuwunel. Otherwise follow Step 3.2***
1. Open your tuwunel.toml file. e.g. `nano /etc/tuwunel/tuwunel.toml`.
2. Find the line reading `#rtc_transports = []` and replace it with:
```toml
[[global.well_known.rtc_transports]]
type = "livekit"
livekit_service_url = "https://matrix-rtc.yourdomain.com"
```
3. Close the file.

#### 3.2. .well-known served independently
***Follow this step if you serve your .well-known/matrix files directly. Otherwise follow Step 3.1***
1. Open your `.well-known/matrix/client` file. e.g. `nano /var//www/.well-known/matrix/client`.
2. Add the following:
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
3. Close the file.

### 4. Configure Firewall
You will need to allow ports `7881/tcp` and `50100:50200/udp` through your firewall. If you use UFW, the commands are: `ufw allow 7881/tcp` and `ufw allow 50100:50200/udp`.

### 5. Configure Reverse Proxy
As reverse proxies can be installed in different ways, step by step instructions are not given for this section.
If you use Caddy as your reverse proxy, follow step 5.1. If you use Nginx, follow step 5.2.

#### 5.1. Caddy
1. Add the following to your Caddyfile. If you are running Caddy in Docker, replace `localhost` with `matrix-rtc-jwt` in the first instance, and `matrix-rtc-livekit` in the second.
```
matrix-rtc.yourdomain.com {
    # This is matrix-rtc-jwt
    @jwt_service {
        path /sfu/get* /healthz*
    }
    handle @jwt_service {
        reverse_proxy localhost:8081 {
            header_up Host {host}
            header_up X-Forwarded-Server {host}
            header_up X-Real-IP {remote}
            header_up X-Forwarded-For {remote}
            header_up X-Forwarded-Proto {scheme}
        }
    }
    # This is livekit
    handle {
        reverse_proxy localhost:7880 {
            header_up Connection "upgrade"
            header_up Upgrade {http.request.header.Upgrade}
            header_up Host {host}
            header_up X-Forwarded-Server {host}
            header_up X-Real-IP {remote}
            header_up X-Forwarded-For {remote}
            header_up X-Forwarded-Proto {scheme}
        }
    }
}
```
2. Restart Caddy.

#### 5.2. Nginx
1. Add the following to your Nginx configuration. If you are running Nginx in Docker, replace `localhost` with `matrix-rtc-jwt` in the first instance, and `matrix-rtc-livekit` in the second.
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
    location ~ ^(/sfu/get|/healthz) {
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

#### 5.3. Traefik
1. Add your `matrix-rtc-jwt` `matrix-rtc-livekit` to your traefik's network
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
2. Configure with either one of the methods below

2.1 Labels
```yaml
services:
  matrix-rtc-jwt:
    # ...
    labels:
        - "traefik.enable=true"
        - "traefik.http.routers.matrixrtcjwt.entrypoints=websecure"
        - "traefik.http.routers.matrixrtcjwt.rule=Host(`matrix-rtc.yourdomain.com`) && PathPrefix(`/sfu/get`) || PathPrefix(`/healthz`)"
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
2.2 Config file
```yaml
http:
    routers:
        matrixrtcjwt:
            entryPoints:
                - "websecure"
            rule: "Host(`matrix-rtc.yourdomain.com`) && PathPrefix(`/sfu/get`) || PathPrefix(`/healthz`)"
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
### 6. Start Docker Containers
1. Ensure you are in your matrix-rtc directory. e.g. `cd /opt/matrix-rtc`.
2. Start containers: `docker compose up -d`.

Element Call should now be working.

## Additional Configuration
### TURN Integration
If you follow this guide, and also set up Coturn as per the tuwunel documentation, there will be a port clash between the two services. To avoid this, the following must be added to your `coturn.conf`:
```
min-port=50201
max-port=65535
```

If you have Coturn configured, you can use it as a TURN server for Livekit to improve call reliability. As Coturn allows multiple instances of `static-auth-secret`, it is suggested that the secret used for Livekit is different to that used for tuwunel.

1. Create a secret for Coturn. It is suggested that this should be a random 64 character alphanumeric string.
2. Add the following line to the end of your `turnserver.conf`. `AUTH_SECRET` is the secret created in Step 1.
```
static-auth-secret=AUTH_SECRET
```
3. Add the following to the end of the `rtc` block in your `livekit.yaml`. `AUTH_SECRET` is the same as above. `turn.yourdomain.com` should be replaced with your actual TURN domain.
```
  turn_servers:
    - host: turn.yourdomain.com
      port: 5349
      protocol: tls
      secret: "AUTH_SECRET"
```

### Using the Livekit Built In TURN Server
It is also possible to use the built in Livekit TURN server. Getting this to work can be a somewhat involved process, and a TURN server is not usually required for Matrix RTC calls. Consequently, instructions are not provided here at this time. If you would like to configure this, more information can be found [here](https://docs.livekit.io/transport/self-hosting/deployment/#improving-connectivity-with-turn).
