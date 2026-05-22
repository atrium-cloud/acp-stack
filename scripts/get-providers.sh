#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"

models_url="${ACPS_MODELS_DEV_URL:-https://models.dev/api.json}"
pi_providers_url="${ACPS_PI_PROVIDERS_URL:-https://raw.githubusercontent.com/earendil-works/pi/main/packages/coding-agent/docs/providers.md}"
curl_bin="${CURL_BIN:-curl}"

tmpdir="$(mktemp -d)"
cleanup() {
  rm -rf "$tmpdir"
}
trap cleanup EXIT

curl_args=(--ipv4 --fail --silent --show-error --location --connect-timeout 20 --retry 2)

models_json="$tmpdir/models.dev.json"
pi_providers_md="$tmpdir/pi-providers.md"

"$curl_bin" "${curl_args[@]}" "$models_url" > "$models_json"
"$curl_bin" "${curl_args[@]}" "$pi_providers_url" > "$pi_providers_md"

python3 - "$repo_root" "$models_json" "$pi_providers_md" <<'PY'
from __future__ import annotations

import json
import re
import sys
import tomllib
from pathlib import Path

repo_root = Path(sys.argv[1])
models_path = Path(sys.argv[2])
pi_providers_path = Path(sys.argv[3])

providers_text = (repo_root / "data/providers.toml").read_text()
providers = tomllib.loads(providers_text)["providers"]
env_vars = tomllib.loads((repo_root / "data/env_vars.toml").read_text())["api_keys"]
models = json.loads(models_path.read_text())

provider_by_id: dict[str, dict] = {}
for provider in providers:
    for provider_id in provider["id"]:
        provider_by_id[provider_id] = provider

opencode_ids = {
    provider_id
    for provider in providers
    if "opencode" in provider.get("agents", [])
    for provider_id in provider["id"]
}
models_ids = set(models)
excluded_models_dev_ids: set[str] = set()
in_excluded_block = False
for line in providers_text.splitlines():
    stripped = line.strip()
    if stripped == "# Excluded:":
        in_excluded_block = True
        continue
    if in_excluded_block and not stripped:
        break
    if in_excluded_block:
        match = re.match(r"^# - ([A-Za-z0-9_.-]+)\b", stripped)
        if match:
            excluded_models_dev_ids.add(match.group(1))
missing_opencode = sorted(models_ids - opencode_ids - excluded_models_dev_ids)

env_by_provider: dict[str, set[str]] = {}
for mapping in env_vars:
    for provider_id in mapping.get("provider_ids", []):
        provider = provider_by_id.get(provider_id)
        ids = provider["id"] if provider else [provider_id]
        refs = {mapping["env_var"], *mapping.get("companion_env_vars", [])}
        for mapped_id in ids:
            env_by_provider.setdefault(mapped_id, set()).update(refs)

for provider in providers:
    provider_refs = set(provider.get("companion_env_vars", []))
    pi_ref = provider.get("api_key_env_vars", {}).get("pi")
    if pi_ref:
        provider_refs.add(pi_ref)
    for provider_id in provider["id"]:
        env_by_provider.setdefault(provider_id, set()).update(provider_refs)

table_rows: list[tuple[str, list[str], str]] = []
in_table = False
for line in pi_providers_path.read_text().splitlines():
    if line.startswith("| Provider | Environment Variable | `auth.json` key |"):
        in_table = True
        continue
    if not in_table:
        continue
    if not line.startswith("|"):
        break
    if re.match(r"^\|[- ]+\|", line):
        continue
    cells = [cell.strip() for cell in line.strip("|").split("|")]
    if len(cells) != 3:
        continue
    provider_name, env_cell, auth_key_cell = cells
    env_refs = re.findall(r"`([^`]+)`", env_cell)
    auth_keys = re.findall(r"`([^`]+)`", auth_key_cell)
    if auth_keys:
        table_rows.append((provider_name, env_refs, auth_keys[0]))

missing_pi_provider: list[str] = []
missing_pi_env: list[str] = []
for provider_name, documented_env_refs, provider_id in table_rows:
    provider = provider_by_id.get(provider_id)
    if not provider or "pi" not in provider.get("agents", []):
        missing_pi_provider.append(f"{provider_id}\t{provider_name}")
        continue
    actual_env_refs = env_by_provider.get(provider_id, set())
    missing_refs = sorted(set(documented_env_refs) - actual_env_refs)
    if missing_refs:
        missing_pi_env.append(
            f"{provider_id}\t{','.join(documented_env_refs)}\t"
            f"missing={','.join(missing_refs)}"
        )

failed = False
if missing_opencode:
    failed = True
    print("models.dev providers missing from data/providers.toml for opencode:")
    for provider_id in missing_opencode:
        print(f"  {provider_id}\t{models[provider_id].get('name', provider_id)}")

if missing_pi_provider:
    failed = True
    print("Pi providers missing from data/providers.toml:")
    for row in missing_pi_provider:
        print(f"  {row}")

if missing_pi_env:
    failed = True
    print("Pi env var drift against data/env_vars.toml:")
    for row in missing_pi_env:
        print(f"  {row}")

if failed:
    sys.exit(1)

print("Provider mappings are in sync with models.dev and Pi provider docs.")
PY
