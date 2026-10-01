#!/usr/bin/env bash
# End-to-end smoke for the ptyd runtime, fully isolated from the real fleet:
# a throwaway HOME (so data dir, config, sessions and the ptyd socket are all
# private), a stand-in `sh` harness instead of a real agent, and a private
# tmux socket is never touched because the backend is ptyd.
set -euo pipefail

NINOX="${NINOX:-$(cd "$(dirname "$0")/.." && pwd)/target/debug/ninox}"
SANDBOX="$(mktemp -d /tmp/tnr-smoke.XXXX)"
export HOME="$SANDBOX"
export NINOX_CONFIG="$SANDBOX/config.toml"
export NINOX_PTYD_SOCKET="$SANDBOX/ptyd.sock"
# Belt and braces: the backend is ptyd, but any tmux call still lands on a
# throwaway server, never the real `-L ninox` one.
export NINOX_TMUX_SOCKET="tnr-smoke-$$"
DB="$SANDBOX/ninox.db"
unset NINOX_SESSION NINOX_ORCHESTRATOR_ID NINOX_CALLER_TYPE NINOX_PANE_ID TMUX TMUX_PANE CLAUDECODE

ENGINE=
PORT=$((40000 + RANDOM % 20000))
cleanup() { [[ -n "$ENGINE" ]] && kill "$ENGINE" 2>/dev/null; "$NINOX" ptyd stop >/dev/null 2>&1 || true; tmux -L "$NINOX_TMUX_SOCKET" kill-server 2>/dev/null || true; [[ -n "${KEEP:-}" ]] || rm -rf "$SANDBOX"; }
trap cleanup EXIT

cat > "$NINOX_CONFIG" <<'TOML'
port = 0
font_size = 13.0

[runtime]
backend = "ptyd"

[worker]
harness = "fake"

[orchestrator]
harness = "fake"

[harnesses.fake]
enabled = true
binary = "sh"
worker_args = ["-c", "'printf \"task: %s\\n❯ \\n\" \"$0\"; exec cat'", "{prompt}"]
interactive_args = ["-c", "'printf \"❯ \\n\"; exec cat'"]
resume_args = ["-c", "'printf \"resumed\\n❯ \\n\"; exec cat'", "{session_id}"]
TOML

REPO="$SANDBOX/repo"
git init -q "$REPO" && git -C "$REPO" -c user.email=s@s -c user.name=s commit -q --allow-empty -m init

step() { printf '\n== %s\n' "$*"; }
fail() { echo "FAIL: $*" >&2; KEEP=1; exit 1; }

step "spawn worker (auto-starts ptyd)"
out="$("$NINOX" spawn --db "$DB" --prompt "smoke task" --workspace "$REPO" 2>&1)" || fail "spawn: $out"
echo "$out"
ID="$("$NINOX" list --db "$DB" --json 2>/dev/null | python3 -c 'import json,sys; d=json.load(sys.stdin); print((d[0] if isinstance(d,list) else d["sessions"][0])["id"])')"
echo "session: $ID"

step "pane is held by ptyd"
"$NINOX" pane list | tee /dev/stderr | grep -q "$ID" || fail "pane not listed"

step "read screen"
sleep 1
"$NINOX" read "$ID" | tee /dev/stderr | grep -q "task: smoke task" || fail "task brief not on screen"

step "send input, see it echoed"
"$NINOX" send --db "$DB" "$ID" "hello-from-smoke" >/dev/null 2>&1 || "$NINOX" send "$ID" "hello-from-smoke"
sleep 1
"$NINOX" read "$ID" | grep -q "hello-from-smoke" || fail "input not echoed"

step "engine restart never touches agents"
"$NINOX" --headless --port "$PORT" --db "$DB" >"$SANDBOX/engine.log" 2>&1 &
ENGINE=$!
sleep 3
kill "$ENGINE"; wait "$ENGINE" 2>/dev/null || true
"$NINOX" pane list | grep "$ID" | grep -q live || fail "pane died with the engine"
"$NINOX" read "$ID" | grep -q "hello-from-smoke" || fail "screen lost across engine restart"

step "simulate reboot: kill ptyd and every pane"
pid="$("$NINOX" ptyd status | sed -n 's/.*pid \([0-9]*\).*/\1/p')"
pane_pid="$("$NINOX" pane list | awk -v id="$ID" '$1==id {print $3}')"
kill -9 "$pid"; kill -9 -- "-$pane_pid" 2>/dev/null || kill -9 "$pane_pid" 2>/dev/null || true
sleep 1

step "checkpoint survived"
test -s "$SANDBOX/checkpoints/$ID.json" || fail "no checkpoint for $ID"
grep -q "hello-from-smoke" "$SANDBOX/checkpoints/$ID.json" || fail "checkpoint missing last screen"

step "engine start after 'reboot' reconciles the dead session"
"$NINOX" --headless --port "$PORT" --db "$DB" >>"$SANDBOX/engine.log" 2>&1 &
ENGINE=$!
for _ in $(seq 1 20); do
  "$NINOX" fleet status --db "$DB" | grep -qi "interrupted" && break
  sleep 0.5
done
"$NINOX" fleet status --db "$DB" | tee /dev/stderr | grep -qi "interrupted" || fail "session not marked interrupted"
"$NINOX" fleet status --db "$DB" | grep -q "uncommitted" && fail "fresh worker reported dirty"

step "dry-run restore"
"$NINOX" fleet restore --db "$DB" --dry-run

step "restore"
"$NINOX" fleet restore --db "$DB" --yes
sleep 2
"$NINOX" pane list | grep "$ID" | grep -q live || fail "restored pane not live"
"$NINOX" read "$ID" | tee /dev/stderr | grep -q "resumed" || fail "pane did not resume"

step "restore is idempotent"
"$NINOX" fleet restore --db "$DB" --yes

kill "$ENGINE" 2>/dev/null || true
echo; echo "SMOKE OK (sandbox $SANDBOX)"
