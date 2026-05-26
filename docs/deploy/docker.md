# Docker Deployment

The Docker image runs `acps serve` as the unprivileged `acp` user and stores all instance state in mounted volumes.

## Image Behavior

| Item              | Value                                     |
| ----------------- | ----------------------------------------- |
| Runtime user      | `acp` (`uid 1000`, `gid 1000`)            |
| Working directory | `/workspace`                              |
| Default port      | `7700` or `$PORT` when set                |
| Default command   | `acps serve --bind 0.0.0.0:${PORT:-7700}` |
| Included binaries | `acps`, `acpctl`                          |

Mount these paths for a persistent instance:

```text
/workspace
/home/acp/.config/acp-stack
/home/acp/.local/share/acp-stack
```

`/workspace` stores project files. The config and state mounts preserve `acp-stack.toml`, `age.key`, `state.sqlite`, and `secrets.age`.

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

Save both printed API keys immediately. The session key is used for normal API calls. The admin key is used for management actions and is printed only when it is first generated.

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

Set `ACP_STACK_AUTO_INIT=1` only when you want the entrypoint to initialize a missing config automatically. First-run API keys are printed to container logs in that mode.

## Railway

Use the root `Dockerfile` and attach a persistent Railway volume at `/home/acp`. Railway provides `PORT`; the image binds to that port automatically.

Railway deployments are detected from the `RAILWAY_*` platform vars. When detected, the entrypoint defaults `ACP_STACK_AUTO_INIT=1`, and `ACP_STACK_ALLOW_ROOT=1` if the volume mount forces root execution.

On the first successful deploy, capture both generated API keys from deployment logs. Later deploys reuse the persisted `/home/acp` config, state, age key, and encrypted secret store.

## Security Notes

Production Docker deployments should use the image default `USER acp`. `ACP_STACK_ALLOW_ROOT=1` is only for disposable environments and platform shapes that require root-owned mounted volumes.

For public exposure, put TLS termination and routing at a reverse proxy or Cloudflare Tunnel. Runtime HTTP hardening remains active behind the proxy, including authentication, request limits, origin checks, rate limits, and security logging.
