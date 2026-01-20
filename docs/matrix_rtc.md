This is a basic "step by step" guide to setting up Element Call/MatrixRTC for Tuwunel. The aim is to provide the minimum amount of information necessary to get Element Call working, in a way that can be followed by anyone regardless of technical experience. Each step provides a brief description of what it is doing, followed by a suggested command to complete it.
This guide is inspired by [this blog post](https://sspaeth.de/2024/11/sfu/). For further information on setting up Element Call, please refer to that guide.

***The following is very much based on my own experience of setting up Element Call for Tuwunel. Any amendments or additions are gratefully received.***

## Requirements
- A Linux server.
- A working Tuwunel installation.
- Docker and Docker Compose installed on the server.
- A Caddy or Nginx reverse proxy.
- A basic knowledge of how the above components work.

## Notes
- My installation is on a Debian server. These instructions should work on other distributions, but I may have missed something.
- I use Caddy as a reverse proxy, and my Nginx knowledge is a bit rusty, so there is a chance that there are errors in the Nginx configuration provided.
- `yourdomain.com` is whatever you have set as `server_name` in your tuwunel.toml. This needs to be replaced with the actual domain. It is assumed that you will be hosting MatrixRTC at `matrix-rtc.yourdomain.com`. If you wish to host this service at a different subdomain, this needs to be replaced as well.

## 1. Set Up DNS
Create a DNS record for `matrix-rtc.yourdomain.com` pointing to your server.

e.g. an `A` record for `matrix-rtc` pointing to `your.server.ip.address`.

## 2. Create Docker Containers
1. Create a directory for your MatrixRTC setup: `mkdir /opt/matrix-rtc`.
2. Change directory to your new directory: `cd /opt/matrix-rtc`.
3. Create and open a compose.yaml file for MatrixRTC: `nano compose.yaml`.
4. Add the following. `mrtckey` and `mrtcsecret` should be random strings. It is suggested that `mrtckey` is 20 characters and `mrtcsecret` is 64 characters.
```yaml
services:
  matrix-rtc-jwt:
    image: ghcr.io/element-hq/lk-jwt-service:latest
    container_name: matrix-rtc-jwt
    environment:
      - LIVEKIT_JWT_PORT=8081
      - LIVEKIT_URL=https://matrix-rtc.yourdomain.com/livekit/sfu
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
4. Close the file: `Ctrl+x`.
5. Create and open a livekit.yaml file: `nano livekit.yaml`.
6. Add the following. `mrtckey` and `mrtcsecret` should be the same as those from the compose.yaml. 
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
7. Close the file: `Ctrl+x`.

## 3. Configure .well-known
### 3.1. .well-known served by Tuwunel
***Follow this step if your .well-known configuration is served by tuwunel. Otherwise follow Step 3.2***
1. Open your tuwunel.toml file: e.g. `nano /etc/tuwunel/tuwunel.toml`.
2. Find the line reading `#rtc_transports = []` and edit it to be:
```toml
rtc_transports = [
  { 
    type = "livekit", 
    livekit_service_url = "https://matrix-rtc.yourdomain.com" 
  }
]
```
3. Close the file: `Ctrl+x`.

### 3.2. .well-known served independently
***Follow this step if you serve your .well-known/matrix files directly. Otherwise follow Step 3.1***
1. Open your .well-known/matrix/client file: e.g. `nano /var//www/.well-known/matrix/client`.
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
3. Close the file: `Ctrl+x`.

## 4. Configure Firewall
You will need to allow ports `7881/tcp` and `50100:50200/udp` through your firewall. If you use UFW, the commands are: `ufw allow 7881/tcp` and `ufw allow 50100:50200/udp`.

## 5. Configure Reverse Proxy
As reverse proxies can be installed in different ways, I am not giving step by step instructions for this section.
If you use Caddy as your reverse proxy, follow step 5.1. If you use Nginx, follow step 5.2.

### 5.1. Caddy
1. The following needs to be added to your Caddyfile. If you are running Caddy in Docker, replace `localhost` with `matrix-rtc-jwt` in the first instance, and `matrix-rtc-livekit` in the second.
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

### 5.2. Nginx
1. The following needs to be added to your Nginx configuration:
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
    location /livekit/sfu/ {
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

    # Redirect root / at /livekit/sfu/
    location = / {
        return 301 /livekit/sfu/;
    }
}
```
2. Restart Nginx.

## 6. Start Docker Containers
1. Ensure you are in your matrix-rtc directory: `cd /opt/matrix-rtc`.
2. Start containers: `docker compose up -d`.

Element Call should now be working.
