---
name: whoami
description: ask ninox what you are — your own identity, ownership, and capabilities, resolved live from the store.
---

# Check Your Own Identity

If you ever need to re-confirm who you are, who owns you, or what you're
currently allowed to reach for — especially after a context compaction —
ask ninox directly instead of guessing from `cwd`, your prompt history, or
what a teammate's pane looks like:

```bash
ninox whoami          # plain text
ninox whoami --json   # machine-readable
```

This is resolved fresh from the store on every call — never cached — so it
reflects your actual current status, owning orchestrator, workspace,
branch, PR, and the capabilities currently enabled for you.

If ninox isn't managing the session you're running in (or it's already
been terminated), `whoami` says so explicitly rather than staying silent
or guessing. It never discloses your original task brief or prompt — only
identity/role facts the store owns.
