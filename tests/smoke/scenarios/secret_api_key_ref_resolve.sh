#!/usr/bin/env bash
# Smoke: api_key_ref resolution in endpoint config.
# Stores a secret in the keyring backend (file fallback path on headless
# Linux), configures an endpoint with api_key_ref, verifies wg endpoints
# list shows the key as present (✓).
# owner: implement-wg-secret, fix-wg-secret
#
# exit 0  → PASS
# exit 77 → loud SKIP
# any other non-zero → FAIL
set -euo pipefail
. "$(dirname "$0")/_helpers.sh"
require_wg

# Use a temp HOME + temp workgraph dir
SMOKE_HOME=$(mktemp -d)
SMOKE_DIR=$(mktemp -d)
add_cleanup_hook "rm -rf $SMOKE_HOME $SMOKE_DIR"
export HOME="$SMOKE_HOME"

mkdir -p "$SMOKE_HOME/.wg"
mkdir -p "$SMOKE_DIR"

# Store a test secret in keyring (uses --from-stdin per fix-wg-secret)
SECRET_NAME="smoke-api-key-ref-$$"
SECRET_VALUE="sk-api-ref-test-${RANDOM}"

cleanup_secret() {
    wg secret rm "$SECRET_NAME" --yes 2>/dev/null || true
}
add_cleanup_hook cleanup_secret

echo "$SECRET_VALUE" | wg secret set "$SECRET_NAME" --from-stdin 2>&1 \
    | grep -q "stored in keyring backend" \
    || { echo "FAIL: storing test secret via --from-stdin"; exit 1; }

# Initialize a minimal workgraph
wg --dir "$SMOKE_DIR" init --name "smoke-test-$$" --yes 2>/dev/null || true

# Write a config with api_key_ref
cat > "$SMOKE_DIR/config.toml" <<TOML
[project]
name = "smoke-test"

[[llm_endpoints.endpoints]]
name = "test-endpoint"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key_ref = "keyring:${SECRET_NAME}"
is_default = true
TOML

# wg endpoints list should show the key as present (✓)
ENDPOINTS_OUT=$(wg --dir "$SMOKE_DIR" endpoints list 2>&1)
echo "endpoints output: $ENDPOINTS_OUT"
echo "$ENDPOINTS_OUT" | grep -q "test-endpoint" \
    || { echo "FAIL: endpoint not listed"; exit 1; }
echo "$ENDPOINTS_OUT" | grep -q "✓" \
    || { echo "FAIL: key not marked as present (✓) in endpoints list"; exit 1; }

# wg secret check should confirm the ref is reachable
wg secret check "keyring:${SECRET_NAME}" 2>&1 | grep -q "is reachable" \
    || { echo "FAIL: secret check failed"; exit 1; }

echo "PASS: api_key_ref resolution OK"
exit 0
