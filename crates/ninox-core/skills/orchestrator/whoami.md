---
name: whoami
description: ask ninox what you are — your own identity, ownership, and capabilities, resolved live from the store.
---

# Check Your Own Identity

If you ever need to re-confirm your own session id, which capabilities are
currently enabled for you, or whether ninox still considers you live —
especially after a context compaction — ask ninox directly instead of
guessing from `cwd` or pane labels:

```bash
{{NINOX_BIN}} whoami          # plain text
{{NINOX_BIN}} whoami --json   # machine-readable
```

This is resolved fresh from the store on every call — never cached — so it
reflects your actual current status, workspace, and the capabilities
currently enabled for you.

If ninox isn't managing the session you're running in (or it's already
been terminated), `whoami` says so explicitly rather than staying silent
or guessing. It never discloses a worker's original task brief or prompt —
only identity/role facts the store owns.
