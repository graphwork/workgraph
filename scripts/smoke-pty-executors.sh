#!/usr/bin/env bash
# Unified PTY smoke runner — exercises native (wg nex), claude, and
# codex executor paths through the TUI's embedded PTY pane.
#
# Runs every tui_*.sh smoke from scripts/smoke/ and reports
# per-executor pass/fail/skip. Exit 0 if all pass, 1 if any fail.
#
# Usage:
#   bash scripts/smoke-pty-executors.sh          # all smokes
#   bash scripts/smoke-pty-executors.sh --quick   # spawn-only (fast)

set -u

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
SMOKE_DIR="$SCRIPT_DIR/smoke"

# Ordered from fastest to slowest so failures surface early.
SMOKES=(
    # --- Native (wg nex) executor ---
    "tui_auto_pty.sh          native  spawn+banner+--resume"
    "tui_key_forwarding.sh    native  13-key byte-level forwarding"
    "tui_pty_escape.sh        native  Ctrl-T escape toggle"
    "tui_pty_paste_scroll_resume.sh native paste+scroll+resume"
    "tui_chat_turn.sh         native  two-turn dialogue via fake LLM"
    "tui_takeover.sh          native  daemon→TUI handler takeover"
    # --- Claude executor ---
    "tui_claude_pty.sh        claude  spawn+trust-prompt+Down-arrow"
    # --- Codex executor ---
    "tui_codex_pty.sh         codex   AGENTS.md priming+spawn"
)

# --quick mode: only run spawn/banner tests (< 30s total).
if [[ "${1:-}" == "--quick" ]]; then
    SMOKES=(
        "tui_auto_pty.sh      native  spawn+banner+--resume"
        "tui_claude_pty.sh    claude  spawn+trust-prompt+Down-arrow"
        "tui_codex_pty.sh     codex   AGENTS.md priming+spawn"
    )
fi

PASS=0
FAIL=0
SKIP=0
FAILED_NAMES=()

echo "=== PTY Executor Smoke Suite ==="
echo ""

for entry in "${SMOKES[@]}"; do
    script=$(echo "$entry" | awk '{print $1}')
    executor=$(echo "$entry" | awk '{print $2}')
    label=$(echo "$entry" | awk '{$1=$2=""; print $0}' | sed 's/^ *//')
    path="$SMOKE_DIR/$script"

    if [[ ! -f "$path" ]]; then
        echo "  MISS  [$executor] $script — file not found"
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$script")
        continue
    fi

    printf "  RUN   [%-7s] %-40s " "$executor" "$script"
    output=$(bash "$path" 2>&1)
    rc=$?

    if [[ $rc -eq 0 ]]; then
        echo "PASS"
        PASS=$((PASS + 1))
    elif [[ $rc -eq 77 ]]; then
        echo "SKIP  ($label)"
        SKIP=$((SKIP + 1))
    else
        echo "FAIL"
        FAIL=$((FAIL + 1))
        FAILED_NAMES+=("$script")
        echo "$output" | sed 's/^/        /' | tail -15
        echo ""
    fi
done

echo ""
echo "=== Summary: $PASS pass, $FAIL fail, $SKIP skip ==="

if [[ $FAIL -gt 0 ]]; then
    echo ""
    echo "Failed:"
    for name in "${FAILED_NAMES[@]}"; do
        echo "  - $name"
    done
    exit 1
fi

exit 0
