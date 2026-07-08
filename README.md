# Ninox

Ninox is a native desktop agent orchestrator — built in Rust with [Iced](https://github.com/iced-rs/iced). It embeds its own orchestrator engine directly and runs a GPU-accelerated UI: no Electron, no bundled browser. Running Ninox starts the engine with its own SQLite store and an HTTP/WebSocket API server.

## Prerequisites

- Rust toolchain: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
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
./target/release/ninox --port 9090 --db ~/.local/share/ninox/ninox.db
```

The HTTP server always starts on `127.0.0.1:8080` (or `--port`), exposing the engine's HTTP/WebSocket API.

## macOS app bundle

Every [tagged release](https://github.com/Made-by-Moonlight/ninox/releases) has a prebuilt `Ninox.app.zip` attached as a release asset — download it, unzip, and drag `Ninox.app` into `/Applications`. No local Rust toolchain needed.

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

App config is stored at `~/.config/ninox/config.toml`:

```toml
port = 8080
font_size = 13.0
```

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
| `ninox-core` | Engine: session lifecycle, config, storage |
| `ninox-server` | HTTP/WebSocket server exposing the engine |
| `ninox` | Native Iced UI + binary entry point |

## License

MIT
