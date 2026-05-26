# Cursor CLI

Cursor CLI is a native ACP target. `acp-stack` launches `cursor-agent acp`.

## Setup

```sh
acps init --agent cursor
acps secrets set CURSOR_API_KEY
acps agent set --model <cursor-model-id>
```

Agent config shape:

```toml
[agent]
id = "cursor"
command = "cursor-agent"
args = ["acp"]
cwd = "/workspace"
env = ["CURSOR_API_KEY"]
restart = "on-crash"
```

Cursor model values are discovered from the agent over ACP. `acps agent set` stores the exact advertised value after validation.

Agent, Ask, and Plan modes are supported via:

```sh
acps agent set --mode <agent|ask|plan>
```
