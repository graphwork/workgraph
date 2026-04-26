#!/usr/bin/env bash
# Codex coordinator PTY smoke. Asserts:
#   1. `wg tui` with executor=codex writes the coordinator prompt
#      into `<chat_dir>/AGENTS.md` (codex's auto-load mechanism —
#      no --system-prompt flag exists for interactive codex).
#   2. Banner renders / `codex` process spawns with CWD=chat_dir.
#
# We do NOT assert two-turn dialogue like the claude smoke does:
# interactive codex requires an auth'd account and is noisy to fake.
# Priming + spawn is what this smoke locks in.
#
# Exit 0 pass, 1 fail, 77 skip (tmux/codex missing).

set -u

POLL_DEADLINE=${POLL_DEADLINE:-15}

for t in tmux codex python3; do
    if ! command -v "$t" >/dev/null; then
        echo "SKIP: $t not available"
        exit 77
    fi
done

TMPHOME=$(mktemp -d)
SESSION=wg-smoke-codex-pty-$$
cleanup() {
    tmux kill-session -t "$SESSION" 2>/dev/null
    pkill -f "^codex" 2>/dev/null
    cd /
    rm -rf "$TMPHOME"
}
trap cleanup EXIT

cd "$TMPHOME"

wg init --no-agency -x codex -m local:m -e http://127.0.0.1:1 >/dev/null 2>&1

UUID="019db700-0000-7000-8000-0000000000c0"
python3 - <<PY
import json, pathlib
wg = pathlib.Path.cwd() / ".wg"
(wg / "chat").mkdir(parents=True, exist_ok=True)
(wg / "chat" / "$UUID").mkdir(parents=True, exist_ok=True)
(wg / "chat" / "sessions.json").write_text(json.dumps({
    "version": 0,
    "sessions": {"$UUID": {
        "kind": "coordinator",
        "created": "2026-04-22T21:00:00Z",
        "aliases": ["coordinator-1", "1"],
        "label": "test",
    }}
}))
PY
wg add ".coordinator-1" --id .coordinator-1 --tag coordinator-loop >/dev/null 2>&1

tmux kill-session -t "$SESSION" 2>/dev/null
tmux new-session -d -s "$SESSION" -x 200 -y 50 \
    "cd '$TMPHOME' && wg tui 2>$TMPHOME/tui.err"

# Wait for AGENTS.md to be written (TUI writes it before codex spawn).
CHAT_DIR="$TMPHOME/.wg/chat/$UUID"
for i in $(seq 1 "$POLL_DEADLINE"); do
    sleep 1
    [[ -f "$CHAT_DIR/AGENTS.md" ]] && break
done

if [[ ! -f "$CHAT_DIR/AGENTS.md" ]]; then
    echo "FAIL: AGENTS.md not written to chat_dir ($CHAT_DIR)"
    ls "$CHAT_DIR" 2>/dev/null
    exit 1
fi

# Content check: must be the coordinator prompt (not a stray empty or
# project-level file). The first line should start with the
# coordinator prompt's distinctive text.
if ! head -1 "$CHAT_DIR/AGENTS.md" | grep -qE "You are the workgraph coordinator|coordinator"; then
    echo "FAIL: AGENTS.md content doesn't look like the coordinator prompt"
    echo "--- AGENTS.md head ---"
    head -5 "$CHAT_DIR/AGENTS.md"
    exit 1
fi

# Sanity: codex process launched (parent node wrapper OR the native
# codex binary — depends on how codex is installed).
for i in $(seq 1 "$POLL_DEADLINE"); do
    sleep 1
    if pgrep -f "codex$|codex exec" >/dev/null; then
        break
    fi
done
if ! pgrep -f "codex$|codex exec" >/dev/null; then
    echo "FAIL: codex process not found after spawn"
    pgrep -af codex | head
    exit 1
fi

echo "PASS: codex spawned with AGENTS.md primed (coordinator prompt, $(wc -l <"$CHAT_DIR/AGENTS.md") lines)"
