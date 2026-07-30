# Ninox

A native desktop cockpit for running fleets of coding agents.

![Ninox demo](docs/assets/ninox.gif)

## What is Ninox?

Running one coding agent in a terminal is fine. Running five of them across three repos is a wall of terminal tabs with no overview, no shared memory, and no record of what any of them cost. Ninox is a desktop app that turns that mess into a fleet you can actually supervise:

- **Orchestrators and workers** — spawn an orchestrator with a goal and it breaks the work down, spawning worker sessions to execute in parallel. Workers that discover extra work hand it back to the orchestrator (`ninox request-work`) rather than going off-script. Standalone sessions are there for when you just want one agent in one repo.
- **Everything on one board** — a fleet board shows every session at a glance; click through to a live embedded terminal for any of them. Sessions run on a private tmux server, so they keep working when you close the app and are still there when you come back.
- **Isolated by default** — sessions in a git repo get their own worktree, so parallel agents never trample each other's changes.
- **A shared brain** — a plain-Markdown knowledge base that agents query before exploring unfamiliar code and write to before finishing. Knowledge discovered in one session stops being rediscovered in the next. It's just files — commit it, diff it, or point Ninox at an existing Obsidian vault. See [docs/BRAIN.md](docs/BRAIN.md).
- **The boring-but-vital extras** — PR tracking for session branches, per-session cost estimates and context usage, and desktop notifications when a session needs you.

Ninox is built in Rust with [Iced](https://github.com/iced-rs/iced): a GPU-accelerated native UI with its own embedded engine and SQLite store — no Electron, no bundled browser. [Claude Code](https://claude.com/claude-code) is the first-class harness; codex, opencode, aider, and custom harnesses can be enabled via config. Note that worker sessions run unattended with permission prompts bypassed (`--dangerously-skip-permissions` for Claude Code) — spawn fleets in repositories you trust.

## Why use it?

Use Ninox when you've outgrown a single agent in a single terminal:

- You want several agents working in parallel without babysitting each one.
- You want sessions that survive app restarts (and laptops going to sleep).
- You want your agents to accumulate knowledge about your codebases instead of starting cold every session.
- You want one place to see what's running, what it's doing, and what it has cost.

## Prerequisites

- Rust toolchain: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- tmux 3.2+ (3.5+ recommended for full extended-keyboard support)
- macOS or Linux (Windows not yet supported)

## Install

```bash
cargo install ninox
```

## Build and run

```bash
cargo build --release -p ninox

# Run with native UI (requires display)
./target/release/ninox

# Run headless (engine + HTTP API only, no window)
./target/release/ninox --headless

# Custom port and database path
./target/release/ninox --port 9090 --db ./ninox.db
```

The HTTP server always starts on `127.0.0.1:8080` (or `--port`), exposing the engine's HTTP/WebSocket API — so everything the UI does is also scriptable.

## macOS app bundle

Every [tagged release](https://github.com/Made-by-Moonlight/ninox/releases) has a prebuilt `Ninox.app.zip` attached as a release asset — download it, unzip, and drag `Ninox.app` into `/Applications`. No local Rust toolchain needed.

The bundle is ad-hoc code-signed in CI (no paid Apple Developer ID or notarization), which is enough to stop Gatekeeper from calling it "damaged" after a browser download. It's not enough to clear the separate "unidentified developer" warning macOS shows the first time you open an app from outside the App Store — that only goes away with real notarization. The first time you launch `Ninox.app`, expect that prompt; get past it with either:

- Right-click (or Control-click) `Ninox.app` → **Open** → **Open** in the confirmation dialog, or
- **System Settings** → **Privacy & Security** → scroll to the blocked-app notice → **Open Anyway**

You only need to do this once per download.

To build it yourself instead — to get a proper `Ninox.app` that shows up in the Dock and Launchpad with its own icon (instead of running as a bare binary) — build it with [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle):

```bash
cargo install cargo-bundle

# Run from the repo root — bundle asset paths in crates/ninox-app/Cargo.toml
# are resolved relative to the current working directory, not the crate dir.
cargo bundle --release -p ninox --format osx
```

This produces `target/release/bundle/osx/Ninox.app`, which you can open directly or drag into `/Applications`:

```bash
open target/release/bundle/osx/Ninox.app
```

Bundle metadata (name, identifier, icon) lives under `[package.metadata.bundle]` in `crates/ninox-app/Cargo.toml`. The icon source is `crates/ninox-app/assets/icon-1024.png`; the compiled `crates/ninox-app/assets/Ninox.icns` is generated from it with `sips` + `iconutil` (regenerate after changing the source image):

```bash
cd crates/ninox-app/assets
rm -rf Ninox.iconset && mkdir Ninox.iconset
for size in 16 32 128 256 512; do
  sips -z $size $size icon-1024.png --out Ninox.iconset/icon_${size}x${size}.png
  sips -z $((size*2)) $((size*2)) icon-1024.png --out Ninox.iconset/icon_${size}x${size}@2x.png
done
iconutil -c icns Ninox.iconset -o Ninox.icns
rm -rf Ninox.iconset
```

## Configuration

App config lives in the platform config directory — `~/Library/Application Support/ninox/config.toml` on macOS, `~/.config/ninox/config.toml` on Linux (override with `NINOX_CONFIG`):

```toml
port = 8080
font_size = 13.0
```

PR tracking needs a GitHub token: set `github_token` in the config file or export `GITHUB_TOKEN`. Without it, PR status simply won't populate.

## Development

```bash
cargo build                    # Debug build (all crates)
cargo test                     # Run all crate tests
cargo run -p ninox             # Run with native UI
cargo run -p ninox -- --headless  # Run headless (engine + HTTP only)
```

## Crates

| Crate | Purpose |
|---|---|
| `ninox-core` | Engine: session lifecycle, brain, config, storage |
| `ninox-server` | HTTP/WebSocket server exposing the engine |
| `ninox` | Native Iced UI + binary entry point |

## License

MIT
