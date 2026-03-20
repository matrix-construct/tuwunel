# Reverse Proxy Setup - Nginx

[<= Back to Generic Deployment Guide](generic.md#setting-up-the-reverse-proxy)

This guide shows you how to configure Nginx as a reverse proxy for Tuwunel with TLS support.

## Installation

Install Nginx via your preferred method. Most distributions include Nginx in their package repositories:

```bash
# Debian/Ubuntu
sudo apt install nginx

# Red Hat/Fedora
sudo dnf install nginx

# Arch Linux
sudo pacman -S nginx
```

## Configuration

Create a new configuration file at `/etc/nginx/sites-available/tuwunel` (or `/etc/nginx/conf.d/tuwunel.conf` on some distributions):

```nginx
upstream tuwunel {
  server 127.0.0.1:8008; # IP and port where tuwunel is listening
}

# Client-Server API over HTTPS (port 443)
server {
  listen 443 ssl http2;
  listen [::]:443 ssl http2;
  server_name matrix.example.com;

  # Nginx standard body size is 1MB, which is quite small for media uploads
  # Increase this to match the max_request_size in your tuwunel.toml
  client_max_body_size 100M;

  # Forward requests to Tuwunel
  location / {
    proxy_pass http://tuwunel;

    # Preserve host and scheme - critical for proper Matrix operation
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_set_header X-Forwarded-Proto https;
  }

  # TLS configuration (Let's Encrypt example using certbot)
  ssl_certificate /etc/letsencrypt/live/matrix.example.com/fullchain.pem;
  ssl_certificate_key /etc/letsencrypt/live/matrix.example.com/privkey.pem;
}

# Matrix Federation over HTTPS (port 8448)
# Only needed if you want to federate with other homeservers
# Don't forget to open port 8448 in your firewall!
server {
  listen 8448 ssl http2;
  listen [::]:8448 ssl http2;
  server_name matrix.example.com;

  # Same body size increase for larger files
  client_max_body_size 100M;

  # Forward to the same local port as client-server API
  location / {
    proxy_pass http://tuwunel;
    proxy_set_header Host $host;
    proxy_set_header X-Forwarded-For $remote_addr;
    proxy_set_header X-Forwarded-Proto https;
  }

  # TLS configuration (same certificates as above)
  ssl_certificate /etc/letsencrypt/live/matrix.example.com/fullchain.pem;
  ssl_certificate_key /etc/letsencrypt/live/matrix.example.com/privkey.pem;
}
```

### Important Notes

- **Replace `matrix.example.com`** with your actual server name
- **`client_max_body_size`**: Must match or exceed `max_request_size` in your `tuwunel.toml`
- **Do NOT use `$request_uri`** in `proxy_pass` - while some guides suggest this, it's not necessary for Tuwunel and can cause issues
- **IPv6**: The `listen [::]:443` and `listen [::]:8448` lines enable IPv6 support. Remove them if you don't need IPv6

### TLS Certificates

The example above uses Let's Encrypt certificates via certbot. To obtain certificates:

```bash
sudo certbot certonly --nginx -d matrix.example.com
```

Certbot will automatically handle renewal. Make sure to reload Nginx after certificate renewal:

```bash
sudo systemctl reload nginx
```

### Optional: Timeout Configuration

The default Nginx timeouts are usually sufficient for Matrix operations. Element's long-polling `/sync` requests typically run for 30 seconds, which is within Nginx's default timeouts.

However, if you experience federation retries or dropped long-poll connections, you can extend the timeouts by adding these lines inside your `location /` blocks:

```nginx
location / {
  proxy_pass http://tuwunel;
  proxy_set_header Host $host;
  proxy_set_header X-Forwarded-For $remote_addr;
  proxy_set_header X-Forwarded-Proto https;

  # Optional: Extend timeouts if experiencing issues
  proxy_read_timeout 300s;
  proxy_send_timeout 300s;
}
```

## Enable the Configuration

If using sites-available/sites-enabled structure:

```bash
sudo ln -s /etc/nginx/sites-available/tuwunel /etc/nginx/sites-enabled/
```

Test the configuration:

```bash
sudo nginx -t
```

If the test passes, reload Nginx:

```bash
sudo systemctl reload nginx
```

Enable Nginx to start on boot:

```bash
sudo systemctl enable nginx
```

## Verification

After configuring Nginx, verify it's working by checking:

```bash
curl https://matrix.example.com/_tuwunel/server_version
curl https://matrix.example.com:8448/_tuwunel/server_version
```

## Troubleshooting

### Apache Compatibility Note

If you're considering Apache instead of Nginx: Apache is not well-suited as a reverse proxy for Matrix homeservers. If you must use Apache, you need to use `nocanon` in your `ProxyPass` directive to prevent httpd from corrupting the `X-Matrix` authorization header, which will break federation.

### Lighttpd is Not Supported

Lighttpd has known issues with the `X-Matrix` authorization header, making federation non-functional. We do not recommend using Lighttpd with Tuwunel.

---

[=> Continue with "You're Done"](generic.md#you-are-done)
