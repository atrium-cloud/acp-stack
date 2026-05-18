# Cloudflare Deployment

Cloudflare Tunnel is the preferred public-edge pattern for `acp-stack`. It keeps `acps` bound to loopback, avoids exposing a public origin port, and lets Cloudflare terminate TLS at the edge.

## Tunnel Profile

Generate the runtime and `cloudflared` artifacts during init:

```sh
acps init --edge cloudflare --exposure tunnel --hostname agent.example.com
```

This profile:

- keeps `[api].bind` on `127.0.0.1:7700`;
- sets `[api].public_url` to `https://agent.example.com`;
- sets `allowed_origins` to the Cloudflare hostname;
- trusts only local `cloudflared` peers (`127.0.0.1`, `::1`);
- emits generated `cloudflared` config, systemd snippet, Docker Compose snippet, and an operator checklist under the `acp-stack` config directory.

The runtime does not store Cloudflare API tokens or tunnel tokens in `acp-stack.toml`.

## WebSockets

The public `/v1/ws` endpoint requires WebSocket upgrades to reach the daemon. Cloudflare Tunnel forwards WebSockets by default for normal HTTP services. If zone-level WebSocket support has been disabled, enable it before exposing browser clients.

## Proxied DNS Fallback

Direct public-origin deployment behind Cloudflare proxied DNS is an advanced fallback. Use it only when Tunnel is not available:

1. Terminate TLS at Cloudflare and proxy to an HTTPS origin or a local reverse proxy.
2. Restrict the origin firewall so only Cloudflare can reach it.
3. Configure the reverse proxy to forward WebSocket upgrades.
4. Set `trust_proxy_headers = true` only for the trusted local proxy or known Cloudflare-facing proxy path.

Do not bind `acps` directly to a public interface unless the deployment also has firewalling, TLS termination, and an explicit origin policy.

## Runtime Config

Tunnel-generated config should look like:

```toml
[api]
bind = "127.0.0.1:7700"
public_url = "https://agent.example.com"

[security.http]
allowed_origins = ["https://agent.example.com"]
trust_proxy_headers = true
trusted_proxies = ["127.0.0.1", "::1"]
```

Cloudflare request metadata is accepted only after trusted-proxy validation. Direct requests that bypass the trusted proxy path are treated as direct or unknown origin traffic in security logs.

## Compression And Hardening

Keep public-edge compression conservative. Avoid compressing WebSocket streams and sensitive management responses. Cloudflare reduces exposed attack surface, but runtime HTTP hardening remains enabled behind it: API authentication, CORS/origin validation, body limits, rate limits, auth-failure blocking, and audit logging still apply.
