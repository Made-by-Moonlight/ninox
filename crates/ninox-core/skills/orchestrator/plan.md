---
name: plan
description: Register a markdown file as your goals/plan doc so the user can follow along live in the desktop app, rendered and updating as you edit it.
---

# Tracking Your Plan

Register a markdown file as your goals/plan doc:

    {{NINOX_BIN}} plan register /absolute/path/to/plan.md

Keep editing that same file as your plan evolves — no need to re-run
`register` unless the path changes; re-registering (or just re-editing) is
always safe, it just replaces the tracked content.

    {{NINOX_BIN}} plan show
    {{NINOX_BIN}} plan unregister
