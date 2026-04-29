#!/usr/bin/env bash
# Smoke: explicit `keystore` backend round-trip + URI scheme.
# owner: fix-wg-secret
#
# Pins fix-wg-secret: the OLD `Backend::Keyring` was a misnomer — it actually
# wrote a 0600 file at ~/.wg/keystore/. fix-wg-secret renamed that to
# `Backend::Keystore` (and added a real OS keyring as `Backend::Keyring`).
# This scenario asserts the explicit keystore path works end-to-end.
#
# Specifically pins:
#   * `wg secret set <name> --backend keystore --from-stdin` works
#   * the value lands at ~/.wg/keystore/<name> with 0600 perms
#   * `keystore:<name>` URI resolves
#   * `wg secret list` reports it as `keystore:<name>` (not the legacy
#     `keyring:<name>` label)
#   * `wg secret rm <name> --backend keystore --yes` deletes
#
# exit 0  → PASS
# exit 77 → loud SKIP
# any other non-zero → FAIL
set -euo pipefail
. "$(dirname "$0")/_helpers.sh"
require_wg

SMOKE_HOME=$(mktemp -d)
add_cleanup_hook "rm -rf $SMOKE_HOME"
export HOME="$SMOKE_HOME"
mkdir -p "$SMOKE_HOME/.wg"

NAME="smoke-keystore-$$"
VALUE="sk-keystore-${RANDOM}"

# ── set via --from-stdin into explicit keystore backend ──────────────────────
echo "$VALUE" | wg secret set "$NAME" --backend keystore --from-stdin 2>&1 \
    | grep -q "stored in keystore backend" \
    || { echo "FAIL: --backend keystore --from-stdin did not report success"; exit 1; }

# ── verify file landed at ~/.wg/keystore/ with 0600 perms ────────────────────
FILE="$SMOKE_HOME/.wg/keystore/$NAME"
[ -f "$FILE" ] || { echo "FAIL: keystore file not at $FILE"; exit 1; }
if command -v stat >/dev/null 2>&1; then
    PERMS=$(stat -c "%a" "$FILE" 2>/dev/null || stat -f "%p" "$FILE" 2>/dev/null | tail -c 4)
    echo "$PERMS" | grep -qE "600$" \
        || { echo "FAIL: keystore file perms are $PERMS (expected 600)"; exit 1; }
fi

# ── keystore:<name> URI resolves via wg secret check ─────────────────────────
wg secret check "keystore:${NAME}" 2>&1 | grep -q "is reachable" \
    || { echo "FAIL: keystore:<name> URI not reachable"; exit 1; }

# ── get --reveal returns the literal value (with --backend keystore) ─────────
REVEALED=$(wg secret get "$NAME" --backend keystore --reveal 2>/dev/null)
[ "$REVEALED" = "$VALUE" ] \
    || { echo "FAIL: --reveal returned '$REVEALED', expected '$VALUE'"; exit 1; }

# ── list reports the new prefix `keystore:`, never the old misnomer ──────────
LIST_OUT=$(wg secret list 2>&1)
echo "$LIST_OUT" | grep -q "keystore:${NAME}" \
    || { echo "FAIL: list missing 'keystore:${NAME}': $LIST_OUT"; exit 1; }
# It MUST NOT also be reported under the OS-native `keyring:` prefix on a
# plain file write (that label is for the OS keyring index only).
if echo "$LIST_OUT" | grep -qE "^[[:space:]]*keyring:${NAME}$"; then
    echo "FAIL: file-keystore secret incorrectly listed under keyring: prefix"
    echo "list output: $LIST_OUT"
    exit 1
fi

# ── rm --yes ─────────────────────────────────────────────────────────────────
wg secret rm "$NAME" --backend keystore --yes 2>&1 \
    | grep -q "deleted from keystore backend" \
    || { echo "FAIL: wg secret rm --backend keystore --yes did not report success"; exit 1; }

[ ! -f "$FILE" ] || { echo "FAIL: keystore file still exists after rm"; exit 1; }

echo "PASS: explicit keystore backend round-trip OK"
exit 0
