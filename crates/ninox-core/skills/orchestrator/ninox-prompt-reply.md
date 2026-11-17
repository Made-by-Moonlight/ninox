---
name: ninox-prompt-reply
description: Reply to a message sent via the fleet-overview TUI's always-on "ninox prompt" — answer text and/or node ids to highlight, not a chat message.
---

# Replying to the ninox Prompt

A message ending in a block that starts "This came from the ninox prompt"
was typed by a human into the TUI's always-visible prompt, not sent with
`{{NINOX_BIN}} send`. It reached you because you're the orchestrator the
selected node resolved to (or the only orchestrator running) — see "The
ninox prompt" in the fleet-overview UI/UX vision spec.

## Answering

The block tells you the exact command to run, with a `reply_to` id unique
to that message:

```bash
{{NINOX_BIN}} prompt-answer a1b2c3d4-... "nothing is blocking it — webhook-rewrite is waiting on review"
```

For a request that's really about one specific node ("what's blocking the
webhook rewrite?"), highlight it instead of (or alongside) text:

```bash
{{NINOX_BIN}} prompt-answer a1b2c3d4-... "blocked on review" --select webhook-rewrite-worker
```

`--select` takes one or more session ids, comma-separated — the same ids
`{{NINOX_BIN}} list` prints. The answer renders inline in the TUI's prompt
area, or by highlighting the node when no text is given — never as a chat
transcript, so don't send a long narrative; keep it to what belongs in that
one line.

Each `reply_to` is good for one reply — the TUI consumes the file once.
Don't reuse an old one for an unrelated answer; `reply_to` only has meaning
for the prompt that issued it.

## Action requests

A request that resolves to spawning one clear worker or orchestrator: just
do it (`{{NINOX_BIN}} spawn` / `{{NINOX_BIN}} spawn-orchestrator`), then
reply with a short confirmation through `prompt-answer` the same way.

A request for a standalone session ("I need a session to do X in repo Y", no
orchestrator involved) is a one-section ghost plan: `parent_id` null, a
non-Orchestrator `worker_type`, a `workspace`. Propose it with
`{{NINOX_BIN}} plan propose` for the user to accept — it starts a session the
user owns. Don't offer to parent it under another orchestrator or to start a
throwaway one.

A request that implies multiple sections or dependencies (an epic): propose
it as a ghost plan (`{{NINOX_BIN}} plan propose`, see the `plan` skill) and
tell the user its `plan_id` so they can accept or discard it.

## What this is not

This is a reply channel for the ninox prompt specifically, not a general
substitute for `{{NINOX_BIN}} send` — use `send` for an ordinary follow-up
or status update to a session.
