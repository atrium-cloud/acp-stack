# Nginx Reverse Proxy

Use Nginx for TLS termination while keeping `acps` on a private origin such as `127.0.0.1:7700`.

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

```toml
[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"

[security.http]
allowed_origins = ["https://agent.example.com"]
trust_proxy_headers = true
trusted_proxies = ["127.0.0.1"]
```

Set `client_max_body_size` to match (or exceed) the runtime's `max_request_bytes` cap so uploads aren't truncated at the proxy.

Only trust proxy IPs that untrusted clients cannot reach directly.
