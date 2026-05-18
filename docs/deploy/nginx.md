# Nginx Reverse Proxy

Use Nginx for TLS termination and public routing while keeping `acps` bound to a private origin. In a host install that is usually `127.0.0.1:7700`; in Docker it can be a private container network service.

## Example

```nginx
map $http_upgrade $connection_upgrade {
    default upgrade;
    '' close;
}

server {
    listen 443 ssl http2;
    server_name agent.example.com;

    ssl_certificate /etc/letsencrypt/live/agent.example.com/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/agent.example.com/privkey.pem;

    client_max_body_size 100m;

    location / {
        proxy_pass http://127.0.0.1:7700;
        proxy_http_version 1.1;

        proxy_set_header Host $host;
        proxy_set_header X-Forwarded-Host $host;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-For $proxy_add_x_forwarded_for;

        proxy_set_header Upgrade $http_upgrade;
        proxy_set_header Connection $connection_upgrade;

        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
        proxy_buffering off;
        gzip off;
    }
}
```

## Runtime Config

Set the public URL and allow the browser origin explicitly:

```toml
[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"

[security.http]
allowed_origins = ["https://agent.example.com"]
trust_proxy_headers = true
trusted_proxies = ["127.0.0.1"]
```

Only enable `trust_proxy_headers` for proxy IPs that cannot be reached by untrusted clients. `acp-stack` uses trusted proxy validation before accepting forwarded client IP metadata.

## Compression And WebSockets

Keep compression conservative. Do not compress WebSocket traffic, streamed event responses, or secrets-bearing management responses. The runtime still enforces its own HTTP hardening behind Nginx; the proxy does not replace API authentication, origin checks, rate limits, body limits, or audit logging.
