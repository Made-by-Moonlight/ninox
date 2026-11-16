---
name: set-agent-config
description: Use when the user asks to change the orchestrator's or worker's agent harness or model.
---

# Set Ninox Agent Config

Use this skill when the user asks to change the agent harness or model.

## Change the harness or model

```
ninox config set orchestrator.harness codex
ninox config set orchestrator.model <model-name>
ninox config set worker.harness codex
ninox config set worker.model <model-name>
```

`harness` is validated against the known agent harnesses (claude-code,
codex, aider, opencode, pi, plus any configured in `[harnesses.*]`) — an
unknown or disabled name is rejected with an error instead of being
written, and `worker.harness` is additionally rejected if that harness has
no verified worker mode. Harnesses other than claude-code ship disabled;
enable one with `ninox config set harnesses.<name>.enabled true`. `pi`
(npm `@earendil-works/pi-coding-agent`, 1.5.0 or newer) has no permission
prompts, so a worker type with permissions enabled is refused on it. `model` isn't validated; omit it to use the
harness's default. Changes take effect on the next spawn; running sessions
(including restored or resumed ones) keep the model they started with.

These are defaults: a worker type's own model, or an explicit `--model` on
a spawn, overrides them. Left unset, ninox uses its built-in default for the
harness (on claude-code: Opus for orchestrators, Sonnet for everything else). Check the effective result with:

```
ninox models show
```

## Inspect current config

```
ninox config get orchestrator.harness
ninox config show          # full resolved config
```
