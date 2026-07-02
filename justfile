# Ninox native app — development recipes
# Usage: just <recipe>  (install just: brew install just / cargo install just)

bin := justfile_directory() / "target/debug/ninox"

build:
    cargo build

build-release:
    cargo build --release

# Run the TUI — sets NINOX_BIN so spawned orchestrators call this binary
run *args: build
    NINOX_BIN={{bin}} {{bin}} {{args}}

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
