#!/usr/bin/env bash
# Helpers shared by smoke-gate scenarios.
#
# Each scenario script sources this file. The contract:
#   * exit 0  → PASS
#   * exit 77 → loud SKIP (precondition missing — endpoint unreachable, no creds, ...)
#   * any other non-zero → FAIL
#
# Loud SKIPs MUST go through `loud_skip` so the banner is greppable in run logs.

set -u

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

# ── scratch dir under /tmp (or $TMPDIR) ─────────────────────────────
make_scratch() {
    mktemp -d -t wgsmoke.XXXXXX
}

# ── kill background process tree quietly ────────────────────────────
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
