#!/usr/bin/env bash
# Phase 7 interrupt smoke — SIGINT-during-turn under the daemon's
# supervisor doesn't wedge the coordinator.
#
# Empirically, Claude CLI 2.x exits with status 0 on SIGINT rather
# than the "stop generating, keep session" behavior the old inline
# daemon code aspired to. Our handler's SIGINT forwarder sends
# SIGINT to the Claude child (harmless async-signal-safe forward);
# Claude exits; handler detects child-gone and exits non-zero; the
# daemon's subprocess_coordinator_loop restarts the handler; next
# turn succeeds (context via chat files, not Claude CLI in-memory
# session).
#
# Assertions:
#   A. Turn 1 under the daemon completes normally
#   B. SIGINT-ing the live handler (simulating
#      CoordinatorAgent::interrupt() via IPC) causes the daemon to
#      restart it — the replacement PID is live with a fresh lock
#   C. Turn 2 — sent to the same coordinator post-interrupt — still
#      produces an outbox response
set -euo pipefail
tmp=$(mktemp -d)
trap "pkill -P $$ 2>/dev/null || true; pkill -f 'claude-handler' 2>/dev/null || true; rm -rf $tmp 2>/dev/null || true" EXIT
cd "$tmp"
export WG_DIR="$tmp/.workgraph"
wg init --no-agency >/dev/null 2>&1
wg config --coordinator-executor claude >/dev/null 2>&1

fail() { echo "FAIL: $*"; exit 1; }
pass() { echo "PASS: $*"; }

wg service start >/dev/null 2>&1 &
sleep 4
wg service status 2>&1 | grep -qE "^Service: running" || fail "daemon not running"

echo "=== Test A: turn 1 completes ==="
wg chat --coordinator 0 "Reply with just: one" --timeout 60 >/dev/null 2>&1 || true
for i in {1..180}; do
  [ -s "$WG_DIR/chat/coordinator-0/outbox.jsonl" ] && break
  sleep 0.5
done
[ -s "$WG_DIR/chat/coordinator-0/outbox.jsonl" ] || fail "no turn-1 outbox"
lines_1=$(wc -l < "$WG_DIR/chat/coordinator-0/outbox.jsonl")
pass "turn 1 ok ($lines_1 outbox line)"

echo
echo "=== Test B: SIGINT during handler triggers supervisor restart ==="
pid1=$(pgrep -f "wg claude-handler --chat coordinator-0" | head -1)
[ -n "$pid1" ] || fail "no handler to interrupt"
kill -INT "$pid1"
# Daemon should replace it — wait for a NEW live PID.
pid2=""
for i in {1..60}; do
  lock_pid=$(sed -n 1p "$WG_DIR/chat/coordinator-0/.handler.pid" 2>/dev/null || echo "")
  if [ -n "$lock_pid" ] && [ "$lock_pid" != "$pid1" ] && kill -0 "$lock_pid" 2>/dev/null; then
    pid2="$lock_pid"
    break
  fi
  sleep 0.5
done
[ -n "$pid2" ] || { echo "DIAG daemon log:"; tail -15 "$WG_DIR/service/daemon.log"; fail "no post-interrupt restart within 30s"; }
pass "handler restarted after SIGINT (PID $pid1 → $pid2)"

echo
echo "=== Test C: turn 2 succeeds post-interrupt ==="
wg chat --coordinator 0 "Reply with just: two" --timeout 60 >/dev/null 2>&1 || true
for i in {1..180}; do
  n=$(wc -l < "$WG_DIR/chat/coordinator-0/outbox.jsonl")
  [ "$n" -gt "$lines_1" ] && break
  sleep 0.5
done
n=$(wc -l < "$WG_DIR/chat/coordinator-0/outbox.jsonl")
[ "$n" -gt "$lines_1" ] || { echo "DIAG outbox:"; cat "$WG_DIR/chat/coordinator-0/outbox.jsonl"; fail "no turn-2 outbox after interrupt (handler wedged)"; }
pass "turn 2 ok ($lines_1 → $n outbox lines) — coordinator recovered from interrupt"

wg service stop >/dev/null 2>&1 || true

echo
echo "=== ALL PHASE 7 INTERRUPT CHECKS PASSED ==="
