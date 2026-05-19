# systemd Deployment

`acp-stack` ships a root-phase installer and a hardened systemd unit so the `acps` daemon can run as the unprivileged `acp` user on any Linux host with systemd. The installer mirrors the Docker image: same default user, same paths, same workspace layout.

## Prerequisites

- Linux host with systemd as PID 1 (`systemctl is-system-running` returns a value).
- Root access on the host (installer requires it for user creation and unit installation).
- `acps` and `acpctl` binaries built or downloaded locally — the installer accepts local paths only. For Docker-based deployments use [docker.md](./docker.md).

## Install

Build the binaries (or copy a release artifact onto the host):

```sh
cargo build --release --bin acps --bin acpctl
```

Run the installer:

```sh
sudo bash scripts/install-systemd.sh \
  --acps-binary ./target/release/acps \
  --acpctl-binary ./target/release/acpctl
```

What the installer does, in order:

1. Verifies it's running as root, that `systemctl` is available, and that the binary paths exist.
2. Installs `ca-certificates` through the detected package manager (apt, dnf/yum, or zypper). Skip with `--no-os-deps`.
3. Creates the `acp` system user with `/home/acp` if missing (`useradd --system --shell /usr/sbin/nologin`).
4. Creates `/workspace`, `/workspace/uploads`, `/home/acp/.config/acp-stack`, and `/home/acp/.local/share/acp-stack` with owner-only permissions on the config and state dirs.
5. Installs `acps` and `acpctl` to `/usr/local/bin/`.
6. Runs `acps init --no-install-agent --workspace-root <workspace> --workspace-uploads <workspace>/uploads --runtime-user <user>` as the runtime user (unless `--no-init`). This generates the starter `acp-stack.toml`, the age key, the encrypted secret store, the SQLite state, and the session and admin API keys. **The keys are printed to the installer's stdout — save them now; they are not recoverable from scrollback later.**
7. Writes `/etc/systemd/system/acp-stack.service`. If the existing unit already matches the rendered unit, the installer leaves it unchanged; if it differs, the installer refuses to overwrite unless `--force` is passed.
8. Runs `systemctl daemon-reload`. **The unit is not enabled automatically** — you opt in explicitly.

The installer is idempotent for the same arguments: re-running it preserves existing user data, skips `acps init` when the config is already present, and leaves an identical unit file untouched. Pass `--force` only when you intentionally want to overwrite a differing unit file or re-run init on an already-initialized instance with the same config-managed user and workspace values. To change the runtime user or workspace after initialization, update the config intentionally first; the installer will not silently drift the unit away from `acp-stack.toml`.

### Installer options

| Flag | Default | Purpose |
| --- | --- | --- |
| `--acps-binary <path>` | required | local path to `acps` binary |
| `--acpctl-binary <path>` | required | local path to `acpctl` binary |
| `--user <name>` | `acp` | runtime user name |
| `--home <dir>` | `/home/<user>` | runtime user homedir |
| `--workspace <dir>` | `/workspace` | workspace root used in both the generated config and unit |
| `--bind <addr>` | `127.0.0.1:7700` | bind address baked into `ExecStart` |
| `--unit-path <path>` | `/etc/systemd/system/acp-stack.service` | unit destination |
| `--no-init` | off | skip `acps init` (two-step install) |
| `--no-os-deps` | off | skip the `ca-certificates` install |
| `--force` | off | overwrite a differing unit file; re-run init |

### Two-step install

For setups where the operator wants to inspect the config or wire secrets before the first start, install bits now and init later:

```sh
sudo bash scripts/install-systemd.sh \
  --acps-binary ./target/release/acps \
  --acpctl-binary ./target/release/acpctl \
  --no-init

# Later, after preparing config or secrets:
sudo -u acp -H /usr/local/bin/acps init \
  --no-install-agent \
  --workspace-root /workspace \
  --workspace-uploads /workspace/uploads \
  --runtime-user acp
```

## Configure

Default paths (matching the Docker image):

```text
/workspace                              # workspace root
/workspace/uploads                      # writable uploads root
/home/acp/.config/acp-stack/acp-stack.toml   # config
/home/acp/.config/acp-stack/age.key           # age key (0600)
/home/acp/.local/share/acp-stack/state.sqlite # SQLite state
/home/acp/.local/share/acp-stack/secrets.age  # encrypted secret store
```

The starter `acp-stack.toml` binds the API to `127.0.0.1:7700`. Front it with a reverse proxy (see below) and never re-bind to `0.0.0.0` directly unless you understand the hardening surface.

### Environment overrides

The unit ships with `EnvironmentFile=-/etc/acp-stack/environment` (the leading `-` makes it optional). Create that file to set runtime env vars without editing the unit:

```sh
sudo install -m 0600 -o root -g root /dev/null /etc/acp-stack/environment
sudo tee -a /etc/acp-stack/environment >/dev/null <<'ENV'
ACP_STACK_SUPABASE_URL=https://example.supabase.co
ENV
sudo systemctl restart acp-stack
```

## Operate

```sh
sudo systemctl enable --now acp-stack   # enable + start
sudo systemctl status acp-stack         # current status
sudo systemctl restart acp-stack        # restart
sudo systemctl stop acp-stack           # stop
sudo systemctl disable --now acp-stack  # stop + disable
```

Logs go to the journal:

```sh
journalctl -u acp-stack -f                       # follow
journalctl -u acp-stack --since "1 hour ago"     # recent
journalctl -u acp-stack -p warning                # warnings+
journalctl -u acp-stack -n 200 --no-pager         # last 200 lines
```

`acps` emits its rich runtime history to SQLite, not to the journal. Use `acpctl logs query` or `acps logs query` for structured history; use `journalctl` for the tracing/eprintln status lines.

## Smoke test

The session and admin keys are printed only by the installer's `acps init` step. Capture them from that terminal output the first time you install. The keys are written to the encrypted secret store at `${HOME_DIR}/.local/share/acp-stack/secrets.age`, but `acps` does not re-print them on later runs.

If you missed them, rotate the keys with `acps reset --yes` (destroys the existing instance) or replace the secret store from a backup. Save the new keys when they're printed.

With the session key in hand, probe locally:

```sh
curl -fsS -H "Authorization: Bearer ${SESSION_KEY}" http://127.0.0.1:7700/v1/status
```

For an end-to-end install + start + probe smoke against a clean systemd container (runs on any Linux host with Docker):

```sh
bash scripts/install-systemd-smoke.sh
```

This script also runs in CI on every PR that touches the installer or unit file (`.github/workflows/install-systemd-smoke.yml`).

## Reverse proxy

`acp-stack` does not terminate TLS. Bind to loopback (the default) and front with one of:

- [nginx.md](./nginx.md)
- [caddy.md](./caddy.md)
- [cloudflare.md](./cloudflare.md) — preferred for public exposure (Cloudflare Tunnel)

Runtime HTTP hardening (auth, rate limits, request size, origin checks, security logging) stays active behind the proxy.

## Upgrade

```sh
cargo build --release --bin acps --bin acpctl   # on the build host
sudo install -m 0755 ./target/release/acps   /usr/local/bin/acps
sudo install -m 0755 ./target/release/acpctl /usr/local/bin/acpctl
sudo systemctl restart acp-stack
```

Re-running the installer with `--force` produces the same effect, and also refreshes the unit file from the in-tree template.

## Uninstall

```sh
sudo systemctl disable --now acp-stack
sudo rm /etc/systemd/system/acp-stack.service
sudo systemctl daemon-reload
sudo rm /usr/local/bin/acps /usr/local/bin/acpctl
```

Removing user data is intentionally manual. To wipe the instance entirely:

```sh
sudo rm -rf /workspace /home/acp/.config/acp-stack /home/acp/.local/share/acp-stack
sudo userdel --remove acp
```

## Security

The unit applies the hardening directives recommended in `docs/specs/security.md`:

| Directive | Effect |
| --- | --- |
| `User=acp / Group=acp` | the daemon runs unprivileged; production never relies on `--allow-root` |
| `NoNewPrivileges=true` | the daemon cannot gain new privileges via setuid binaries |
| `PrivateTmp=true` | the daemon gets a private `/tmp` namespace |
| `ProtectSystem=strict` | the entire filesystem is read-only except `/dev`, `/proc`, `/sys`, and paths re-allowed by `ReadWritePaths` |
| `ReadWritePaths=<workspace> <home>` | the only writable paths the daemon owns: the workspace root and the runtime user's homedir (`acps` writes config/state under `~/.config/acp-stack` and `~/.local/share/acp-stack`; agent installs land under `~/.local/bin`; supported headless agents write their config under `~/.config/{goose,opencode}`, `~/.pi`, or `~/.codex`) |
| `Restart=on-failure` + `RestartSec=5s` | crash recovery without masking clean shutdowns |
| `TimeoutStopSec=30s` | gives the daemon time to drain the Supabase logging outbox on SIGTERM |

`acps serve` itself refuses to start as root unless `--allow-root` (or `ACP_STACK_ALLOW_ROOT=1`) is set, and even then refuses an empty admin key. The unit's `User=acp` directive is the production mechanism; root opt-in is for disposable/dev profiles only.
