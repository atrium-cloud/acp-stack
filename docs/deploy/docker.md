# Docker Deployment

`acp-stack` ships a production Dockerfile for the `acps` daemon. The image builds the Rust binaries in a builder stage, copies `acps` and `acpctl` into a slim Debian runtime image, and runs as the unprivileged `acp` user by default.

## Image Behavior

- Runtime user: `acp` (`uid 1000`, `gid 1000`).
- Working directory: `/workspace`.
- Exposed port: `7700`.
- Default command: `acps serve --bind 0.0.0.0:7700`.
- Included binaries: `acps`, `acpctl`.

Persistent paths should be mounted explicitly:

```text
/workspace
/home/acp/.config/acp-stack
/home/acp/.local/share/acp-stack
```

`/workspace` stores runtime workspace files and should be writable by uid 1000. The config and state mounts preserve `acp-stack.toml`, `age.key`, `state.sqlite`, and `secrets.age`.

## First Run

Build the image:

```sh
docker build -t acp-stack:local .
```

Create volumes:

```sh
docker volume create acp-stack-workspace
docker volume create acp-stack-config
docker volume create acp-stack-state
```

Initialize config, state, and API keys:

```sh
docker run --rm \
  -v acp-stack-workspace:/workspace \
  -v acp-stack-config:/home/acp/.config/acp-stack \
  -v acp-stack-state:/home/acp/.local/share/acp-stack \
  acp-stack:local \
  acps init --no-install-agent
```

Save both printed API keys immediately. The session key is used for routine API calls such as `GET /v1/status`; the admin key is used for management routes and is shown only when generated.

Start the daemon:

```sh
docker run -d \
  --name acp-stack \
  -p 7700:7700 \
  -v acp-stack-workspace:/workspace \
  -v acp-stack-config:/home/acp/.config/acp-stack \
  -v acp-stack-state:/home/acp/.local/share/acp-stack \
  acp-stack:local
```

## Smoke Test

Run the container startup smoke test from the repository root:

```sh
scripts/docker-smoke.sh
```

The script builds an `acp-stack-smoke:phase4` image, initializes fresh named volumes, starts a daemon container, parses the generated session key locally, and checks authenticated `GET /v1/status`.

Named volumes are preserved by default for inspection. Remove them after the run with:

```sh
scripts/docker-smoke.sh --cleanup-volumes
```

If host port `7700` is already in use:

```sh
ACP_STACK_DOCKER_SMOKE_PORT=17700 scripts/docker-smoke.sh
```

For repeated local smoke runs, use persistent mode:

```sh
scripts/docker-smoke.sh --persistent --rebuild
scripts/docker-smoke.sh --persistent
```

Persistent mode reuses image `acp-stack-smoke:persistent`, container `acp-stack-smoke-server`, and stable named volumes. It runs `acps init --no-install-agent` only when the config volume has not been initialized. The first init output and parsed session key are stored under `.git/docker-smoke/` with owner-only permissions so later persistent smoke runs can authenticate without reinitializing the instance.

Reset the persistent environment with:

```sh
scripts/docker-smoke.sh --persistent --reset
```

## Security Notes

Production containers should use the image default `USER acp`. `acps serve` has an `ACP_STACK_ALLOW_ROOT=1` escape hatch for disposable development profiles, but it is not needed for this image and should not be set in normal deployments.

For public exposure, keep TLS termination and public routing at a reverse proxy or Cloudflare Tunnel. Runtime HTTP hardening remains active behind the proxy, including authentication, request limits, CORS/origin checks, rate limits, and security logging.
