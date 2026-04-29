#!/usr/bin/env bash
# Smoke: OS-native keyring when the platform actually provides one.
# owner: fix-wg-secret
#
# Pins fix-wg-secret option A: when the OS keyring (macOS Keychain, libsecret /
# secret-service on Linux, Windows Credential Manager) is reachable,
# `wg secret set <name>` MUST land in the OS store — not silently fall back
# to a 0600 file. This scenario exercises that path.
#
# Reachability detection (no libsecret-tools CLI required):
#   * macOS — assume Keychain is always reachable.
#   * Linux — require a running session D-Bus AND a registered
#     `org.freedesktop.secrets` service. We probe via `dbus-send`. The
#     `keyring` crate links the dbus-secret-service crate directly, so it
#     does not need the libsecret CLI utilities.
#   * Other platforms — loud SKIP for now.
#
# When the OS keyring IS reachable, this scenario asserts:
#   * `wg secret set <name> --from-stdin` succeeds.
#   * NO file is written under ~/.wg/keystore/<name> (proving the OS path
#     was used, not the file fallback).
#   * `wg secret get <name> --reveal` round-trips the value.
#   * `wg secret backend show` reports the OS keyring as reachable.
#   * `wg secret rm <name> --yes` removes the entry; subsequent get returns
#     not-found.
#   * On macOS specifically, `security find-generic-password` confirms the
#     entry landed in the Keychain (this is the strongest possible
#     independent check; on Linux without secret-tool installed, we rely
#     on the absence-of-keystore-file invariant + wg's own round-trip).
#
# exit 0  → PASS
# exit 77 → loud SKIP (OS keyring not reachable in this environment)
# any other non-zero → FAIL
set -euo pipefail
. "$(dirname "$0")/_helpers.sh"
require_wg

# ── Platform-specific OS keyring detection ───────────────────────────────────
PLATFORM="$(uname -s)"
HAVE_LIBSECRET_CLI=0
case "$PLATFORM" in
    Darwin)
        command -v security >/dev/null 2>&1 \
            || loud_skip "OS_KEYRING_UNAVAILABLE" "macOS 'security' tool not on PATH"
        ;;
    Linux)
        if [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ]; then
            loud_skip "OS_KEYRING_UNAVAILABLE" "DBUS_SESSION_BUS_ADDRESS unset (headless? no D-Bus session)"
        fi
        if ! command -v dbus-send >/dev/null 2>&1; then
            loud_skip "OS_KEYRING_UNAVAILABLE" "dbus-send not on PATH (cannot probe secret-service)"
        fi
        if ! dbus-send --session --print-reply --dest=org.freedesktop.DBus / \
                org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1; then
            loud_skip "OS_KEYRING_UNAVAILABLE" "D-Bus session bus does not respond to Ping"
        fi
        if ! dbus-send --session --print-reply --dest=org.freedesktop.secrets \
                /org/freedesktop/secrets \
                org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1; then
            loud_skip "OS_KEYRING_UNAVAILABLE" "org.freedesktop.secrets service not registered (no gnome-keyring / kwallet running)"
        fi
        # libsecret CLI is OPTIONAL — if available we use it for the
        # strongest verification, but its absence does not mean the OS
        # keyring is unavailable.
        if command -v secret-tool >/dev/null 2>&1; then
            HAVE_LIBSECRET_CLI=1
        fi
        ;;
    *)
        loud_skip "OS_KEYRING_UNAVAILABLE" "platform '$PLATFORM' not handled by this scenario"
        ;;
esac

# Fresh HOME so we own the keyring-index sidecar AND can assert that no
# file fallback was written. The OS keyring entry itself is global to the
# user's session — we use a unique name and clean up.
SMOKE_HOME=$(mktemp -d)
add_cleanup_hook "rm -rf $SMOKE_HOME"
export HOME="$SMOKE_HOME"
mkdir -p "$SMOKE_HOME/.wg"

NAME="wg-smoke-os-$$"
VALUE="sk-os-keyring-${RANDOM}"

cleanup_os_entry() {
    wg secret rm "$NAME" --yes 2>/dev/null || true
    case "$PLATFORM" in
        Darwin) security delete-generic-password -a "$NAME" -s wg 2>/dev/null || true ;;
        Linux)
            [ "$HAVE_LIBSECRET_CLI" = "1" ] && \
                secret-tool clear service wg account "$NAME" 2>/dev/null || true
            ;;
    esac
}
add_cleanup_hook cleanup_os_entry

# ── set via --from-stdin ─────────────────────────────────────────────────────
echo "$VALUE" | wg secret set "$NAME" --from-stdin 2>&1 \
    | grep -q "stored in keyring backend" \
    || { echo "FAIL: --from-stdin store reported wrong backend"; exit 1; }

# ── invariant: no keystore file fallback was written ─────────────────────────
KEYSTORE_FALLBACK_FILE="$SMOKE_HOME/.wg/keystore/$NAME"
if [ -f "$KEYSTORE_FALLBACK_FILE" ]; then
    echo "FAIL: OS keyring is reachable but wg fell back to file keystore at $KEYSTORE_FALLBACK_FILE"
    echo "      (the value should be in OS storage, not on disk)"
    exit 1
fi

# ── get --reveal round-trips ─────────────────────────────────────────────────
REVEALED=$(wg secret get "$NAME" --reveal 2>/dev/null)
[ "$REVEALED" = "$VALUE" ] \
    || { echo "FAIL: --reveal returned '$REVEALED', expected '$VALUE'"; exit 1; }

# ── platform-native independent verification when available ─────────────────
case "$PLATFORM" in
    Darwin)
        FOUND=$(security find-generic-password -a "$NAME" -s wg -w 2>/dev/null || true)
        [ "$FOUND" = "$VALUE" ] \
            || { echo "FAIL: macOS Keychain did not return the stored value (got: '$FOUND')"; exit 1; }
        ;;
    Linux)
        if [ "$HAVE_LIBSECRET_CLI" = "1" ]; then
            FOUND=$(secret-tool lookup service wg account "$NAME" 2>/dev/null || true)
            [ "$FOUND" = "$VALUE" ] \
                || { echo "FAIL: secret-service did not return the stored value via secret-tool (got: '$FOUND')"; exit 1; }
        else
            echo "INFO: secret-tool not installed — relying on absence-of-fallback-file + wg round-trip as proxy for OS-keyring placement"
        fi
        ;;
esac

# ── backend show reports OS keyring reachable ────────────────────────────────
BACKEND_OUT=$(wg secret backend show 2>&1)
echo "$BACKEND_OUT" | grep -qiE "Keyring \(OS native\): reachable" \
    || { echo "FAIL: backend show does not report OS keyring as reachable: $BACKEND_OUT"; exit 1; }

# ── rm --yes removes from OS storage ─────────────────────────────────────────
wg secret rm "$NAME" --yes 2>&1 | grep -q "deleted from keyring backend" \
    || { echo "FAIL: wg secret rm --yes did not report success"; exit 1; }

# After rm, get should return not-found.
GET_AFTER=$(wg secret get "$NAME" --reveal 2>&1 || true)
if echo "$GET_AFTER" | grep -q "$VALUE"; then
    echo "FAIL: wg secret get still returns the value after rm: $GET_AFTER"
    exit 1
fi

# Platform-native confirmation when available.
case "$PLATFORM" in
    Darwin)
        if security find-generic-password -a "$NAME" -s wg -w >/dev/null 2>&1; then
            echo "FAIL: macOS Keychain still has the entry after rm"
            exit 1
        fi
        ;;
    Linux)
        if [ "$HAVE_LIBSECRET_CLI" = "1" ]; then
            if [ -n "$(secret-tool lookup service wg account "$NAME" 2>/dev/null)" ]; then
                echo "FAIL: secret-service still has the entry after rm"
                exit 1
            fi
        fi
        ;;
esac

echo "PASS: OS keyring round-trip OK on $PLATFORM"
exit 0
