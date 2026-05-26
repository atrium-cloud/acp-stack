# Caddy Reverse Proxy

Use Caddy when you want automatic TLS with a small public-edge config. Keep `acps` bound to a private origin and let Caddy terminate HTTPS.

## Example

```caddyfile
agent.example.com {
	reverse_proxy 127.0.0.1:7700 {
		header_up Host {host}
		header_up X-Forwarded-Host {host}
		header_up X-Forwarded-Proto {scheme}
		header_up X-Forwarded-For {remote_host}
		transport http {
			read_timeout 1h
			write_timeout 1h
		}
	}
}
```

Caddy handles WebSocket upgrades automatically for `reverse_proxy`. Tune `read_timeout` / `write_timeout` to the longest streaming session you expect.

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

Use the actual Caddy source IP when Caddy runs on a different host or container network. Do not trust broad public ranges.
