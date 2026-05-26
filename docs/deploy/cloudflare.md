# Cloudflare Deployment

Cloudflare Tunnel is the preferred public-edge profile for a self-hosted `acp-stack` instance. It keeps `acps` bound to loopback while `cloudflared` handles public ingress and TLS.

## Tunnel Profile

Initialize with:

```sh
acps init --edge cloudflare --exposure tunnel --hostname agent.example.com
```

This profile:

- keeps the daemon bound to `127.0.0.1`
- sets `[api].public_url`
- allows the Cloudflare hostname as a browser origin
- trusts only the local tunnel peer for forwarded client metadata
- writes local `cloudflared` config snippets for the operator to install

`acp-stack` does not create Cloudflare accounts, tunnels, DNS records, or tokens. Provision those in Cloudflare, then apply the generated local tunnel config.

## Runtime Config

The resulting config should have the same shape as:

```toml
[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"

[security.http]
allowed_origins = ["https://agent.example.com"]
trust_proxy_headers = true
trusted_proxies = ["127.0.0.1", "::1"]
```

Only add proxy addresses that are private to the host or container network.

## WebSockets

The public edge must support WebSocket upgrades for `/v1/ws`. Cloudflare Tunnel does this without extra runtime configuration.

## Security Notes

Cloudflare reduces public exposure, but it does not replace runtime hardening. Keep API authentication, request limits, origin checks, rate limits, and audit logging enabled in `acp-stack`.
