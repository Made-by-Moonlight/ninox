# Ninox

Ninox is the native desktop app for [Athene](https://github.com/Made-by-Moonlight/Athene) — built in Rust with [Iced](https://github.com/iced-rs/iced). It embeds the Athene engine directly and runs a GPU-accelerated UI: no Electron, no bundled browser.

The app and Athene's Node.js stack **work in tandem**. Running the native app starts a fresh engine with its own SQLite store and HTTP server; the Athene web dashboard continues to work against either backend unchanged.

> This repo was split out of the [Athene](https://github.com/Made-by-Moonlight/Athene) monorepo's `athene/` directory, history intact. The crates are still named `athene-core` / `athene-server` / `athene-app`; renaming them to `ninox-*` is tracked as a follow-up.

## Prerequisites

- Rust toolchain: `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh`
- macOS or Linux (Windows not yet supported)

## Build and run

```bash
cargo build --release -p athene-app

# Run with native UI (requires display)
./target/release/athene-app

# Run headless (engine + HTTP API only, no window)
./target/release/athene-app --headless

# Custom port and database path
./target/release/athene-app --port 9090 --db ~/.local/share/athene/athene.db
```

The HTTP server always starts on `127.0.0.1:8080` (or `--port`). Athene's web dashboard can connect to it at that address exactly as it connects to the Node.js backend.

## Configuration

App config is stored at `~/.config/athene/config.toml`:

```toml
port = 8080
font_size = 13.0
```

## Development

```bash
cargo build                    # Debug build (all crates)
cargo test                     # Run all crate tests
cargo run -p athene-app        # Run with native UI
cargo run -p athene-app -- --headless  # Run headless (engine + HTTP only)
```

## Crates

| Crate | Purpose |
|---|---|
| `athene-core` | Engine: session lifecycle, config, storage |
| `athene-server` | HTTP/WebSocket server exposing the engine |
| `athene-app` | Native Iced UI + binary entry point |

## License

MIT
