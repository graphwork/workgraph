#!/bin/bash
# Start the local Ollama model server for TerminalBench
# Uses updated Ollama v0.20.6 binary on port 11435

set -euo pipefail

PORT=${OLLAMA_PORT:-11435}
OLLAMA_BIN="$HOME/bin/ollama"
OLLAMA_LOG="/tmp/ollama-local.log"
DEFAULT_MODEL="qwen3-coder:30b-a3b-q8_0"

# Check if the binary exists
if [ ! -x "$OLLAMA_BIN" ]; then
    echo "ERROR: Ollama binary not found at $OLLAMA_BIN"
    echo "Download from: https://github.com/ollama/ollama/releases"
    exit 1
fi

# Check if already running on this port
if curl -sf "http://localhost:$PORT/api/tags" > /dev/null 2>&1; then
    echo "Ollama already running on port $PORT"
    curl -s "http://localhost:$PORT/v1/models" | python3 -m json.tool
    exit 0
fi

echo "Starting Ollama v0.20.6 on port $PORT..."

export OLLAMA_HOST="127.0.0.1:$PORT"
export OLLAMA_MODELS="$HOME/.ollama/models"
export LD_LIBRARY_PATH="$HOME/ollama-lib:${LD_LIBRARY_PATH:-}"

nohup "$OLLAMA_BIN" serve > "$OLLAMA_LOG" 2>&1 &
SERVER_PID=$!

echo "Server PID: $SERVER_PID"
echo "Log: $OLLAMA_LOG"

# Wait for server to be ready
echo -n "Waiting for server"
for i in $(seq 1 30); do
    if curl -sf "http://localhost:$PORT/api/tags" > /dev/null 2>&1; then
        echo " ready!"
        break
    fi
    echo -n "."
    sleep 1
done

# Verify
echo ""
echo "=== Available Models ==="
curl -s "http://localhost:$PORT/v1/models" | python3 -c "
import sys, json
data = json.load(sys.stdin)
for m in data['data']:
    print(f'  {m[\"id\"]}')
" 2>/dev/null || echo "  (none — run: OLLAMA_HOST=127.0.0.1:$PORT $OLLAMA_BIN pull $DEFAULT_MODEL)"

echo ""
echo "=== Endpoints ==="
echo "  OpenAI API: http://localhost:$PORT/v1"
echo "  Ollama API: http://localhost:$PORT/api"
echo "  Models:     http://localhost:$PORT/v1/models"
echo ""
echo "Test: curl http://localhost:$PORT/v1/chat/completions -H 'Content-Type: application/json' \\"
echo "  -d '{\"model\": \"$DEFAULT_MODEL\", \"messages\": [{\"role\": \"user\", \"content\": \"Hello\"}]}'"
