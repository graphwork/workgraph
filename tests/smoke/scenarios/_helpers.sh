#!/usr/bin/env bash
# Helpers shared by smoke-gate scenarios.
#
# Each scenario script sources this file. The contract:
#   * exit 0  → PASS
#   * exit 77 → loud SKIP (precondition missing — endpoint unreachable, no creds, ...)
#   * any other non-zero → FAIL
#
# Loud SKIPs MUST go through `loud_skip` so the banner is greppable in run logs.
#
# ── Fixture lifecycle (read this before adding a scenario) ──
# Smoke scenarios spawn `wg service daemon` processes and create temp dirs.
# Two failure modes have leaked daemons + dirs in production:
#
#   1. Per-scenario `trap` lines silently overwrote one another, so only the
#      last cleanup ran on EXIT.
#   2. `daemon_pid=$!` after `wg service start &` captured the WRAPPER PID
#      that exits as soon as it forks the real daemon. Killing the wrapper
#      did nothing — the real daemon was already re-parented to init.
#
# The contract this file enforces:
#
#   * Every scratch dir lives under `wg_smoke_root` (a single shared parent).
#   * Every spawned daemon is registered for teardown via `start_wg_daemon`,
#     which reads the canonical PID from `service/state.json` rather than
#     `$!`. No scenario should call `wg service start` directly.
#   * One trap, installed by this file, tears every registered fixture down
#     on EXIT/INT/TERM/HUP. Scenarios MUST NOT install their own EXIT trap;
#     use `add_cleanup_hook <fn>` instead if extra teardown is needed.
#   * `wg_smoke_sweep` is a defense-in-depth reaper invoked at session start
#     (and exposed for callers) that finds and kills any `wg service daemon`
#     under the smoke root, then rms the leftover dirs. Kills survive
#     re-parenting because we scan `/proc/*/cmdline`, not the process tree.

set -u

# ── Strip agent-context env vars ────────────────────────────────────
# When `wg done` runs the smoke gate from inside an agent's session, the
# agent's environment is inherited: WG_DIR pins every `wg ...` call to
# the agent's project graph (so `wg init` in a scratch dir is a no-op
# and `wg service start` reports "already running" against the parent
# project's daemon). WG_PROJECT_ROOT / WG_WORKTREE_PATH influence
# worktree-aware behaviour the same way. Unset them so smoke fixtures
# truly run in the scratch dir, not the surrounding project.
unset WG_DIR
unset WG_PROJECT_ROOT
unset WG_WORKTREE_PATH
unset WG_WORKTREE_ACTIVE
unset WG_BRANCH
unset WG_TASK_ID

# ── Skip banner ─────────────────────────────────────────────────────
loud_skip() {
    local kind="$1"
    shift
    local reason="$*"
    echo "" 1>&2
    echo "================================================================" 1>&2
    echo "  SMOKE SKIPPED — $kind" 1>&2
    echo "  scenario: ${WG_SMOKE_SCENARIO:-?}" 1>&2
    echo "  reason:   $reason" 1>&2
    echo "================================================================" 1>&2
    exit 77
}

# ── Fail banner ─────────────────────────────────────────────────────
loud_fail() {
    local reason="$*"
    echo "" 1>&2
    echo "================================================================" 1>&2
    echo "  SMOKE FAILED" 1>&2
    echo "  scenario: ${WG_SMOKE_SCENARIO:-?}" 1>&2
    echo "  reason:   $reason" 1>&2
    echo "================================================================" 1>&2
    exit 1
}

# ── wg binary discovery ─────────────────────────────────────────────
require_wg() {
    if ! command -v wg >/dev/null 2>&1; then
        loud_skip "MISSING WG BINARY" "wg not found on PATH; run 'cargo install --path .' first"
    fi
}

# ── HTTP probe ──────────────────────────────────────────────────────
endpoint_reachable() {
    local url="$1"
    if ! command -v curl >/dev/null 2>&1; then
        return 1
    fi
    curl -fsS -m 5 "$url" -o /dev/null 2>/dev/null
}

# ── Fixture root (single shared parent) ─────────────────────────────
# Pinning every smoke scratch dir under one well-known root means cleanup
# is one `find $root -maxdepth 1 -delete`, not a glob hunt across /tmp.
wg_smoke_root() {
    echo "${WG_SMOKE_ROOT:-${TMPDIR:-/tmp}/wgsmoke}"
}

# ── scratch dir under the smoke root ────────────────────────────────
make_scratch() {
    local root
    root="$(wg_smoke_root)"
    mkdir -p "$root"
    local scenario="${WG_SMOKE_SCENARIO:-adhoc}"
    # `mktemp -d $root/<scenario>.XXXXXX` keeps everything under one parent
    # AND tags each dir with the scenario it belongs to so a stale leak is
    # immediately attributable to a specific scenario.
    local scratch
    scratch=$(mktemp -d "$root/${scenario}.XXXXXX")
    register_scratch "$scratch"
    echo "$scratch"
}

# ── Cleanup registries ──────────────────────────────────────────────
# IMPORTANT: registrations must survive subshells. `make_scratch` is
# called as `scratch=$(make_scratch)` (command substitution → subshell);
# bash array mutations inside a subshell do NOT propagate to the parent.
# We worked around this by writing entries to per-script registry FILES
# whose paths live in env vars inherited by every subshell, and reading
# them back at cleanup. Daemon and scratch registries both go through
# files for symmetry — that way a future scenario that calls
# `start_wg_daemon` in a subshell does not silently leak.
WG_SMOKE_CLEANUP_HOOKS=()

WG_SMOKE_REGISTRY_DIR="$(mktemp -d "${TMPDIR:-/tmp}/wgsmoke-registry.XXXXXX")"
export WG_SMOKE_REGISTRY_DIR
WG_SMOKE_SCRATCHES_FILE="$WG_SMOKE_REGISTRY_DIR/scratches"
WG_SMOKE_DAEMONS_FILE="$WG_SMOKE_REGISTRY_DIR/daemons"
export WG_SMOKE_SCRATCHES_FILE WG_SMOKE_DAEMONS_FILE
: >"$WG_SMOKE_SCRATCHES_FILE"
: >"$WG_SMOKE_DAEMONS_FILE"

# Add a cleanup hook (function name) to run before daemon teardown. Use
# this to e.g. `tmux kill-session` before the workgraph dir disappears.
# (Hooks run in the parent shell; this stays as an in-memory array.)
add_cleanup_hook() {
    WG_SMOKE_CLEANUP_HOOKS+=("$1")
}

# Register a scratch dir for `rm -rf` on cleanup. Called automatically by
# `make_scratch`; expose it for callers that mint scratch dirs manually.
register_scratch() {
    printf '%s\n' "$1" >>"$WG_SMOKE_SCRATCHES_FILE"
}

# Register a daemon (real PID, workgraph dir) for teardown. Format:
# `<pid> <dir>` on one line; dir is read with `read pid dir` so dirs with
# spaces are not supported, but the smoke root never contains spaces.
register_wg_daemon() {
    printf '%s %s\n' "$1" "$2" >>"$WG_SMOKE_DAEMONS_FILE"
}

# ── Find .wg or .workgraph under a scratch dir ──────────────────────
graph_dir_in() {
    local scratch="$1"
    if [[ -d "$scratch/.wg" ]]; then
        echo "$scratch/.wg"; return 0
    fi
    if [[ -d "$scratch/.workgraph" ]]; then
        echo "$scratch/.workgraph"; return 0
    fi
    return 1
}

# ── Read the canonical daemon PID from service/state.json ────────────
# `wg service start` forks a child that becomes the real daemon, then
# exits. The child is the only process that knows the workgraph dir; its
# PID is recorded in state.json. Capturing `$!` from `wg service start &`
# captures the wrapper, NOT the daemon, and on wrapper exit the daemon is
# re-parented to init — `kill $wrapper_pid` then `pkill -P $wrapper_pid`
# both find nothing, and the daemon leaks. Read state.json instead.
wait_for_daemon_pid() {
    local wg_dir="$1"
    local timeout_s="${2:-30}"
    local state="$wg_dir/service/state.json"
    local i pid
    for i in $(seq 1 $((timeout_s * 5))); do
        if [[ -f "$state" ]]; then
            pid=$(grep -oE '"pid"[[:space:]]*:[[:space:]]*[0-9]+' "$state" 2>/dev/null \
                | head -1 | grep -oE '[0-9]+$')
            if [[ -n "$pid" ]] && kill -0 "$pid" 2>/dev/null; then
                echo "$pid"
                return 0
            fi
        fi
        sleep 0.2
    done
    return 1
}

# ── Start wg service daemon and register for teardown ────────────────
# Usage: start_wg_daemon <scratch> [extra wg service start args...]
# After this returns, $WG_SMOKE_DAEMON_PID and $WG_SMOKE_DAEMON_DIR hold
# the canonical daemon PID and the workgraph dir housing it. The daemon
# is registered so `wg_smoke_cleanup` (installed by this file) tears it
# down on script exit.
start_wg_daemon() {
    local scratch="$1"; shift
    local wg_dir
    if ! wg_dir=$(graph_dir_in "$scratch"); then
        loud_fail "no .wg/.workgraph dir under $scratch — run 'wg init' before start_wg_daemon"
    fi
    local wrap_log="$scratch/daemon.log"
    # Pass --dir explicitly so the daemon binds to the scratch fixture
    # regardless of any leaked WG_DIR / discovery context. The cd is
    # belt-and-braces in case a child invocation uses cwd-discovery.
    ( cd "$scratch" && wg --dir "$wg_dir" service start "$@" >"$wrap_log" 2>&1 ) &
    local wrap_pid=$!
    local pid
    if ! pid=$(wait_for_daemon_pid "$wg_dir" 30); then
        wait "$wrap_pid" 2>/dev/null || true
        loud_fail "daemon never wrote state.json at $wg_dir/service/state.json. wrapper log:
$(tail -20 "$wrap_log" 2>/dev/null || echo '<no log>')"
    fi
    wait "$wrap_pid" 2>/dev/null || true
    register_wg_daemon "$pid" "$wg_dir"
    WG_SMOKE_DAEMON_PID="$pid"
    WG_SMOKE_DAEMON_DIR="$wg_dir"
    return 0
}

# ── Sweep: kill stray daemons + remove scratch dirs under the root ──
# Scans `/proc/*/cmdline` for `wg service daemon` processes whose `--dir`
# argv starts with the smoke root and signals them. Survives re-parenting
# (init-owned orphans show up in /proc the same as direct children).
# Then removes every subdir under the root. Idempotent.
wg_smoke_sweep() {
    local root
    root="$(wg_smoke_root)"
    local prefix="${root}/"
    if [[ -d /proc ]]; then
        local cmdline_path cmdline pid args
        local -a victims=()
        for cmdline_path in /proc/[0-9]*/cmdline; do
            [[ -r "$cmdline_path" ]] || continue
            cmdline=$(tr '\0' ' ' < "$cmdline_path" 2>/dev/null) || continue
            # Must contain "service daemon" AND --dir under root.
            case " $cmdline " in
                *" service daemon "*) ;;
                *) continue ;;
            esac
            case " $cmdline " in
                *" --dir $prefix"*) ;;
                *) continue ;;
            esac
            pid=${cmdline_path#/proc/}
            pid=${pid%/cmdline}
            victims+=("$pid")
        done
        local v
        for v in "${victims[@]:-}"; do
            [[ -n "$v" ]] || continue
            kill -TERM "$v" 2>/dev/null || true
        done
        # Give SIGTERM 0.5s to land before SIGKILL.
        if [[ ${#victims[@]} -gt 0 ]]; then
            sleep 0.5
        fi
        for v in "${victims[@]:-}"; do
            [[ -n "$v" ]] || continue
            if kill -0 "$v" 2>/dev/null; then
                kill -KILL "$v" 2>/dev/null || true
            fi
        done
    fi
    if [[ -d "$root" ]]; then
        find "$root" -mindepth 1 -maxdepth 1 -exec rm -rf {} + 2>/dev/null || true
    fi
}

# ── Single EXIT/INT/TERM/HUP trap installed by this file ─────────────
wg_smoke_cleanup() {
    local rc=$?
    # Disable our own trap so cleanup can't re-enter.
    trap - EXIT INT TERM HUP
    # User hooks first (e.g., tmux kill-session before .wg/ disappears).
    local fn
    for fn in "${WG_SMOKE_CLEANUP_HOOKS[@]:-}"; do
        [[ -n "$fn" ]] || continue
        "$fn" 2>/dev/null || true
    done
    # Daemon teardown — graceful via IPC, then SIGTERM, then SIGKILL.
    # Read entries from the persistent file (survives subshell registers).
    local pid dir had_daemons=0
    if [[ -f "$WG_SMOKE_DAEMONS_FILE" ]]; then
        while read -r pid dir; do
            [[ -n "$pid" ]] || continue
            had_daemons=1
            if [[ -n "$dir" ]]; then
                wg --dir "$dir" service stop --force >/dev/null 2>&1 || true
            fi
            if kill -0 "$pid" 2>/dev/null; then
                kill -TERM "$pid" 2>/dev/null || true
            fi
        done <"$WG_SMOKE_DAEMONS_FILE"
    fi
    # Brief reap window then SIGKILL anything still up.
    if [[ "$had_daemons" -eq 1 ]]; then
        sleep 0.5
        while read -r pid dir; do
            [[ -n "$pid" ]] || continue
            if kill -0 "$pid" 2>/dev/null; then
                kill -KILL "$pid" 2>/dev/null || true
            fi
        done <"$WG_SMOKE_DAEMONS_FILE"
    fi
    # Finally remove scratch dirs (also from the persistent file).
    local d
    if [[ -f "$WG_SMOKE_SCRATCHES_FILE" ]]; then
        while read -r d; do
            [[ -n "$d" ]] || continue
            if [[ -d "$d" ]]; then
                rm -rf "$d" 2>/dev/null || true
            fi
        done <"$WG_SMOKE_SCRATCHES_FILE"
    fi
    # Reap our own registry dir so we don't leak meta-state.
    if [[ -n "${WG_SMOKE_REGISTRY_DIR:-}" && -d "$WG_SMOKE_REGISTRY_DIR" ]]; then
        rm -rf "$WG_SMOKE_REGISTRY_DIR" 2>/dev/null || true
    fi
    exit "$rc"
}

trap wg_smoke_cleanup EXIT
trap wg_smoke_cleanup INT
trap wg_smoke_cleanup TERM
trap wg_smoke_cleanup HUP

# ── Legacy: kill_tree by direct children. Kept for the rare scenario
#    that owns a non-daemon background process (e.g. tmux). Do NOT use
#    for `wg service start` — start_wg_daemon handles that correctly. ──
kill_tree() {
    local pid="$1"
    if [[ -z "$pid" ]]; then return 0; fi
    if kill -0 "$pid" 2>/dev/null; then
        pkill -P "$pid" 2>/dev/null || true
        kill "$pid" 2>/dev/null || true
        sleep 1
        kill -9 "$pid" 2>/dev/null || true
    fi
}
