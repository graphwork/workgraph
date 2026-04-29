#!/usr/bin/env bash
# Smoke: keyring backend round-trip (set / get / list / rm) with --from-stdin.
# owner: implement-wg-secret, fix-wg-secret
#
# After fix-wg-secret, the `keyring` backend tries the OS keyring first and
# falls back to the file keystore at ~/.wg/keystore/ when the OS keyring is
# unreachable (typical CI / headless Linux). Either path is a PASS — what we
# test here is the user-facing contract: round-trip works, list does not leak
# values, --from-stdin reads piped input, and --yes makes rm non-interactive.
#
# Pinned regressions (from fix-wg-secret):
#   * `--from-stdin` flag exists and reads one line from stdin (no prompt).
#   * `wg secret rm --yes` deletes without prompting and exits 0.
#   * Backend naming is honest — `wg secret backend show` no longer claims
#     the keyring backend "is" a file at ~/.wg/keystore (it now reports OS
#     keyring reachability and the file fallback explicitly).
#
# exit 0  → PASS
# exit 77 → loud SKIP (no usable keyring path)
# any other non-zero → FAIL
set -euo pipefail
. "$(dirname "$0")/_helpers.sh"
require_wg

# Use a fresh HOME so we don't pollute the developer's real OS keyring with
# smoke artifacts AND so list() output is deterministic.
SMOKE_HOME=$(mktemp -d)
add_cleanup_hook "rm -rf $SMOKE_HOME"
export HOME="$SMOKE_HOME"
mkdir -p "$SMOKE_HOME/.wg"

SECRET_NAME="smoke-keyring-$$"
SECRET_VALUE="sk-smoke-test-value-${RANDOM}"

# ── --from-stdin (the regression this task pins) ─────────────────────────────
# echo "value" | wg secret set <name> --from-stdin should round-trip.
echo "$SECRET_VALUE" | wg secret set "$SECRET_NAME" --from-stdin 2>&1 \
    | grep -q "stored in keyring backend" \
    || { echo "FAIL: --from-stdin did not store in keyring backend"; exit 1; }

# ── get (redacted by default) ────────────────────────────────────────────────
wg secret get "$SECRET_NAME" 2>&1 | grep -q "exists:" \
    || { echo "FAIL: wg secret get did not show key exists"; exit 1; }

# get --reveal must show the actual value
REVEALED=$(wg secret get "$SECRET_NAME" --reveal 2>/dev/null)
[ "$REVEALED" = "$SECRET_VALUE" ] \
    || { echo "FAIL: --reveal returned '$REVEALED', expected '$SECRET_VALUE'"; exit 1; }

# ── list MUST NOT leak the value, and MUST include the name ──────────────────
LIST_OUT=$(wg secret list 2>&1)
echo "$LIST_OUT" | grep -q "$SECRET_NAME" \
    || { echo "FAIL: wg secret list did not include the secret name"; exit 1; }
if echo "$LIST_OUT" | grep -q "$SECRET_VALUE"; then
    echo "FAIL: wg secret list LEAKED the secret value"; exit 1
fi

# ── check ref ────────────────────────────────────────────────────────────────
wg secret check "keyring:${SECRET_NAME}" 2>&1 | grep -q "is reachable" \
    || { echo "FAIL: wg secret check did not report reachable"; exit 1; }

# ── backend show is honest ───────────────────────────────────────────────────
BACKEND_OUT=$(wg secret backend show 2>&1)
# It MUST NOT claim the default backend is a "secure file store at ~/.wg/keystore"
# while calling itself "keyring" — that was the misnomer fix-wg-secret pins.
if echo "$BACKEND_OUT" | grep -qE 'Default backend: keyring \(secure file'; then
    echo "FAIL: backend show still uses the misleading 'keyring (secure file store)' label"
    echo "actual output: $BACKEND_OUT"; exit 1
fi
# It MUST mention both keyring (OS native) and keystore (file) explicitly.
echo "$BACKEND_OUT" | grep -qi "Keyring (OS native)" \
    || { echo "FAIL: backend show missing 'Keyring (OS native)' line: $BACKEND_OUT"; exit 1; }
echo "$BACKEND_OUT" | grep -qi "Keystore" \
    || { echo "FAIL: backend show missing 'Keystore' line: $BACKEND_OUT"; exit 1; }

# ── rm --yes (the second regression this task pins) ──────────────────────────
# Bare rm with no TTY and no --yes must refuse loudly.
if wg secret rm "$SECRET_NAME" </dev/null 2>/dev/null; then
    echo "FAIL: wg secret rm without --yes should refuse on non-TTY stdin"; exit 1
fi
# rm --yes must succeed without prompting and exit 0.
wg secret rm "$SECRET_NAME" --yes 2>&1 | grep -q "deleted from keyring backend" \
    || { echo "FAIL: wg secret rm --yes did not report success"; exit 1; }

# After rm, list should no longer include the name.
LIST_AFTER=$(wg secret list 2>&1)
if echo "$LIST_AFTER" | grep -q "$SECRET_NAME"; then
    echo "FAIL: wg secret list still shows the deleted secret"; exit 1
fi

# After rm, check should report not reachable.
wg secret check "keyring:${SECRET_NAME}" 2>&1 | grep -q "NOT reachable" \
    || { echo "FAIL: wg secret check should report not reachable after rm"; exit 1; }

echo "PASS: keyring backend round-trip OK (--from-stdin, --yes, honest backend status)"
exit 0
