# Ninox native app — development recipes
# Usage: just <recipe>  (install just: brew install just / cargo install just)

# Split across two install paths: ninox-tui (CLI + terminal UI, no iced/wgpu)
# and ninox-gui (desktop app). Day-to-day recipes below drive ninox-tui,
# since every subcommand they use (spawn/serve/etc.) works there too.
bin := justfile_directory() / "target/debug/ninox-tui"
gui_bin := justfile_directory() / "target/debug/ninox-gui"

build:
    cargo build

build-release:
    cargo build --release

# Run the TUI — sets NINOX_BIN so spawned orchestrators call this binary
run *args: build
    NINOX_BIN={{bin}} {{bin}} {{args}}

# Run the desktop app
gui *args: build
    NINOX_BIN={{gui_bin}} {{gui_bin}} {{args}}

serve *args:
    just run --headless {{args}}

# Smoke-test the spawn subcommand
# Usage: just spawn "fix the login bug" /path/to/repo
spawn prompt workspace: build
    NINOX_BIN={{bin}} {{bin}} spawn --prompt {{quote(prompt)}} --workspace {{workspace}}

test:
    cargo test

test-verbose:
    cargo test -- --nocapture

check:
    cargo check

lint:
    cargo clippy -- -D warnings
