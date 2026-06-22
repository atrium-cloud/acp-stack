# Linux VM Dependencies

Use the VM dependency profile when the host image should be ready for common agent work before `acps` starts.

```sh
sudo bash scripts/install-agent-vm-deps.sh
```

The base profile installs common runtime tools for agent harnesses and workspace work: Node.js/npm, Python, uv, Git/SSH, archive tools, `jq`, `rg`, patch/diff tools, and process utilities. It does not install build toolchains or language headers.

## Browser Profile

The optional browser profile adds Chromium, browser fonts, Browser Use, uv-managed Python 3.14, and a local MCP launcher:

```sh
sudo bash scripts/install-agent-vm-deps.sh --profile browser
```

Browser automation is not enabled automatically. Add the MCP server only in runtimes that should expose browsing:

```toml
[[dependencies.commands]]
name = "acp-stack-browser-use-mcp"
required = true
feature = "browser"

[[mcp.servers]]
type = "stdio"
name = "browser-use"
command = "acp-stack-browser-use-mcp"
args = [
  "--allowed-domain", "example.com",
  "--download-dir", "/workspace/browser-downloads",
]
env = ["BROWSER_USE_API_KEY"]
```

Store the Browser Use key separately:

```sh
acps secrets set BROWSER_USE_API_KEY
```

Use `--allowed-domain` to constrain Browser Use navigation. The launcher uses Chromium from the VM profile by default; pass `--browser-executable` only when using a custom browser path. Credentialed login and payment flows remain blocked unless the launcher is explicitly configured with `--allow-credentials` or `--allow-payments`.
