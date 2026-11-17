# Ninox

Ninox is a native desktop agent orchestrator — built in Rust with [Iced](https://github.com/iced-rs/iced). It embeds its own orchestrator engine directly and runs a GPU-accelerated UI: no Electron, no bundled browser. Running Ninox starts the engine with its own SQLite store and an HTTP/WebSocket API server.

## Prerequisites

Only needed to build from source (prebuilt binaries via `cargo binstall` need none of it):

- Rust toolchain: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- macOS or Linux (Windows not yet supported). Ubuntu 22.04 and newer are
  supported and covered by CI; on Debian/Ubuntu install the build
  dependencies first:

  ```bash
  sudo apt-get install -y build-essential pkg-config cmake \
    libxkbcommon-dev libwayland-dev
  ```

  No OpenSSL headers are required — all TLS goes through rustls.

## Install

### Homebrew

The Homebrew formulae build the current `main` branch from source. They require
Homebrew's Rust toolchain to meet the version in `rust-toolchain.toml`, and the
first installation can take a while. Both formulae support macOS and Linux:

```bash
brew install --HEAD made-by-moonlight/tap/ninox-gui  # desktop app
brew install --HEAD made-by-moonlight/tap/ninox-tui  # terminal app and nx
```

Install either or both. Run `brew upgrade --fetch-HEAD ninox-gui ninox-tui`
to update HEAD builds, omitting any formula you did not install. The formulae
are maintained in the [Homebrew tap](https://github.com/Made-by-Moonlight/homebrew-tap).
The GUI formula installs the `ninox-gui` executable, not an Applications bundle.
On first run Ninox creates its `ninox` shim in its own bin directory; make sure
that directory is on your PATH as described by Ninox. Use Homebrew to upgrade
these installations rather than `ninox update`.

### Prebuilt binaries

Prebuilt binaries need no Rust toolchain and no compile. With
[`cargo-binstall`](https://github.com/cargo-bins/cargo-binstall):

```bash
cargo binstall ninox-gui ninox-tui
```

On Linux install just the terminal app (`cargo binstall ninox-tui`): there is no
prebuilt `ninox-gui` for Linux, so naming it compiles it.

This downloads the release tarball for your platform from this repo's GitHub
Releases (`ninox-gui`: Apple silicon macOS; `ninox-tui`: Apple silicon macOS
and Linux x86_64/aarch64). On a platform with no published tarball (for
example Intel Macs), `cargo binstall` offers to build from source instead.

Installing either crate also provides a `ninox` command, pointing at
whichever one you ran first (switch it later with `ninox reset-bin`);
`ninox-tui` also provides the `nx` alias, since bare `nx` has always meant
"open the terminal UI." The two are independent, so install just one if you
only need the desktop app or the terminal app.

### Build from source

Needs the Rust toolchain and, on Debian/Ubuntu, the build dependencies from
Prerequisites. This compiles the whole workspace:

```bash
cargo install ninox-gui ninox-tui
```

Ninox launches every Claude Code session with `--settings '{"crossSessionInbound":"accept"}'` so inter-session messages are delivered; sessions started by an older build pick it up on their next restart or resume, and a `hold`/`refuse` in a project's `.claude/settings*.json` or in managed settings still overrides it (the fleet recovery briefing flags that).

## Build and run

```bash
cargo build --release -p ninox-gui -p ninox-tui

# Run the desktop app (requires display)
./target/release/ninox-gui

# Run the terminal UI
./target/release/ninox-tui

# Run headless (engine + HTTP API only, no window/terminal) — either binary
./target/release/ninox-gui --headless
./target/release/ninox-tui --headless

# Custom port and database path — either binary
./target/release/ninox-tui --port 9090 --db ~/.local/share/ninox/ninox.db
```

The HTTP server always starts on `127.0.0.1:8080` (or `--port`), exposing the engine's HTTP/WebSocket API.

## Terminal mode

Ninox also offers a terminal UI (TUI) for SSH access or headless environments, as its own install path (`ninox-tui`, also built as `nx`) with no iced/wgpu dependency — see [Install](#install). `ninox tui` and bare `nx` both open it; `ninox-gui` has no terminal UI to fall back to, so it always opens the desktop app (or runs headless, with `--headless` or no display).

### Terminal commands

```bash
# Start the terminal UI
ninox tui

# List running sessions (--json for machine-readable output)
ninox list [--json]

# Connect to an existing session
ninox connect <session-id>

# Spawn a new orchestrator session
ninox orchestrate <name> [--prompt <brief>] [--no-attach]
```

The daemon automatically starts in the background on first use and logs to `~/.local/share/ninox/daemon.log` (Linux) or `~/Library/Application Support/ninox/daemon.log` (macOS). Quitting the TUI doesn't shut down the daemon — sessions continue running in the background.

## macOS app bundle

Every [tagged release](https://github.com/Made-by-Moonlight/ninox/releases) has a prebuilt `Ninox.app.zip` attached as a release asset — download it, unzip, and drag `Ninox.app` into `/Applications`. No local Rust toolchain needed.

The bundle is ad-hoc code-signed in CI (no paid Apple Developer ID or notarization), which is enough to stop Gatekeeper from calling it "damaged" after a browser download. It's not enough to clear the separate "unidentified developer" warning macOS shows the first time you open an app from outside the App Store — that only goes away with real notarization. The first time you launch `Ninox.app`, expect that prompt; get past it with either:

- Right-click (or Control-click) `Ninox.app` → **Open** → **Open** in the confirmation dialog, or
- **System Settings** → **Privacy & Security** → scroll to the blocked-app notice → **Open Anyway**

You only need to do this once per download.

To build it yourself instead — to get a proper `Ninox.app` that shows up in the Dock and Launchpad with its own icon (instead of running as a bare binary) — build it with [`cargo-bundle`](https://github.com/burtonageo/cargo-bundle):

```bash
cargo install cargo-bundle

# Run from the repo root — bundle asset paths in crates/ninox-gui/Cargo.toml
# are resolved relative to the current working directory, not the crate dir.
cargo bundle --release -p ninox-gui --format osx
```

This produces `target/release/bundle/osx/Ninox.app`, which you can open directly or drag into `/Applications`:

```bash
open target/release/bundle/osx/Ninox.app
```

Bundle metadata (name, identifier, icon) lives under `[package.metadata.bundle]` in `crates/ninox-gui/Cargo.toml`. The icon source is `crates/ninox-gui/assets/icon-1024.png`; the compiled `crates/ninox-gui/assets/Ninox.icns` is generated from it with `sips` + `iconutil` (regenerate after changing the source image):

```bash
cd crates/ninox-gui/assets
rm -rf Ninox.iconset && mkdir Ninox.iconset
for size in 16 32 128 256 512; do
  sips -z $size $size icon-1024.png --out Ninox.iconset/icon_${size}x${size}.png
  sips -z $((size*2)) $((size*2)) icon-1024.png --out Ninox.iconset/icon_${size}x${size}@2x.png
done
iconutil -c icns Ninox.iconset -o Ninox.icns
rm -rf Ninox.iconset
```

## Configuration

App config is stored at `~/.config/ninox/config.toml`:

```toml
port = 8080
font_size = 13.0
```

## Development

```bash
cargo build                         # Debug build (all crates)
cargo test                          # Run all crate tests
cargo run -p ninox-gui              # Run the desktop app
cargo run -p ninox-tui              # Run the terminal UI
cargo run -p ninox-gui -- --headless  # Run headless (engine + HTTP only)
```

## Crates

| Crate | Purpose |
|---|---|
| `ninox-core` | Engine: session lifecycle, config, storage |
| `ninox-server` | HTTP/WebSocket server exposing the engine |
| `ninox-ui-shared` | CLI dispatch and session/lifecycle logic shared by the two UI binaries |
| `ninox-gui` | Native Iced desktop UI + binary entry point |
| `ninox-tui` | Ratatui terminal UI + binary entry point (also built as `nx`) |

## License

MIT
