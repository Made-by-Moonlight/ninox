---
name: set-agent-config
description: Use when the user asks to change the orchestrator's or worker's agent harness or model.
---

# Set Ninox Agent Config

Use this skill when the user asks to change the agent harness or model.

## Config file

```
{{CONFIG_PATH}}
```

## Format

```toml
[orchestrator]
harness = "claude-code"   # claude-code | codex | aider | opencode
model = "model-name"      # omit to use the harness default

[worker]
harness = "claude-code"
model = "model-name"
```

Use the Edit tool to update the relevant field. Changes take effect on the next spawn.
