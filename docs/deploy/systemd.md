# systemd Deployment

Use the systemd installer when you want `acps` to run as an unprivileged service on a Linux host. The installer creates the runtime user, prepares directories, installs the binaries, initializes the instance, and writes a hardened unit.

## Prerequisites

- Linux host running systemd.
- Root access for user creation and unit installation.
- Local `acps` and `acpctl` release binaries.

## Install

Place the release binaries on the host, then run the installer:

```sh
sudo bash scripts/install-systemd.sh \
  --acps-binary /path/to/acps \
  --acpctl-binary /path/to/acpctl
```

During first initialization, the installer prints the session and admin API keys. Save them immediately. Later runs preserve existing keys and do not print them again.

## Installer Options

| Flag                     | Default                                 | Purpose                                 |
| ------------------------ | --------------------------------------- | --------------------------------------- |
| `--acps-binary <path>`   | required                                | local path to `acps`                    |
| `--acpctl-binary <path>` | required                                | local path to `acpctl`                  |
| `--user <name>`          | `acp`                                   | runtime user                            |
| `--home <dir>`           | `/home/<user>`                          | runtime home                            |
| `--workspace <dir>`      | `/workspace`                            | workspace root                          |
| `--bind <addr>`          | `127.0.0.1:7700`                        | daemon bind address                     |
| `--agent <id>`           | required when init runs                 | agent id selected for init              |
| `--unit-path <path>`     | `/etc/systemd/system/acp-stack.service` | unit destination                        |
| `--no-init`              | off                                     | install files but skip `acps init`      |
| `--no-os-deps`           | off                                     | skip OS dependency installation         |
| `--force`                | off                                     | replace a differing unit or re-run init |

The installer is idempotent for the same arguments. It preserves existing instance data, skips initialization when config already exists, and refuses to replace a differing unit unless `--force` is passed.

## Two-Step Install

Use `--no-init` when you want to inspect config or prepare secrets before the first runtime initialization:

```sh
sudo bash scripts/install-systemd.sh \
  --acps-binary /path/to/acps \
  --acpctl-binary /path/to/acpctl \
  --no-init

sudo -u acp -H /usr/local/bin/acps init \
  --agent <agent-id> \
  --workspace-root /workspace \
  --workspace-uploads /workspace/uploads \
  --runtime-user acp
```

## Paths

Default paths match the Docker image:

```text
/workspace
/workspace/uploads
/home/acp/.config/acp-stack/acp-stack.toml
/home/acp/.config/acp-stack/age.key
/home/acp/.local/share/acp-stack/state.sqlite
/home/acp/.local/share/acp-stack/secrets.age
```

The starter config binds the API to `127.0.0.1:7700`. Keep that loopback bind and expose the service through a reverse proxy or Cloudflare Tunnel.

## Environment Overrides

The unit ships with `EnvironmentFile=-/etc/acp-stack/environment` (the leading `-` makes the file optional). Use it for runtime env vars that the service should receive without editing the unit:

```sh
sudo install -m 0600 -o root -g root /dev/null /etc/acp-stack/environment
```

Configure Supabase logging after init on a host where the Supabase CLI is authenticated:

```sh
sudo -u acp -H acps logging supabase setup --url https://example.supabase.co
sudo -u acp -H acps logging supabase check
```

## Operate

```sh
sudo systemctl enable --now acp-stack
sudo systemctl status acp-stack
sudo systemctl restart acp-stack
sudo systemctl stop acp-stack
sudo systemctl disable --now acp-stack
```

Use the journal for process logs:

```sh
journalctl -u acp-stack -f
journalctl -u acp-stack --since "1 hour ago"
journalctl -u acp-stack -p warning
```

Use `acpctl logs query` or `acps logs query` for structured runtime history.

## Reverse Proxy

`acp-stack` does not terminate TLS. For public access, bind the daemon to loopback and use one of:

- [Cloudflare Tunnel](./cloudflare.md)
- [Nginx](./nginx.md)
- [Caddy](./caddy.md)

## Upgrade

```sh
sudo install -m 0755 /path/to/acps /usr/local/bin/acps
sudo install -m 0755 /path/to/acpctl /usr/local/bin/acpctl
sudo systemctl restart acp-stack
```

Re-running the installer with `--force` also refreshes the unit template.

## Uninstall

```sh
sudo systemctl disable --now acp-stack
sudo rm /etc/systemd/system/acp-stack.service
sudo systemctl daemon-reload
sudo rm /usr/local/bin/acps /usr/local/bin/acpctl
```

Instance data is intentionally left in place. Remove `/workspace` and `/home/acp` only when you intend to destroy the instance.

## Security

The unit runs as `User=acp`, sets `NoNewPrivileges=true`, `PrivateTmp=true`, and `ProtectSystem=strict`, and constrains writes through `ReadWritePaths=<workspace> <home>`. Root execution is not the production path.

`ReadWritePaths` covers the workspace root and the runtime user's home because agent installs land under `~/.local/bin` and supported headless agents write their own config under `~/.config/{goose,opencode}`, `~/.pi`, or `~/.codex`. The daemon itself stays unprivileged.
