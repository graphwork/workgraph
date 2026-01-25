#!/bin/bash
# wg-daemon.sh - Daemon wrapper for wg agent
#
# Usage:
#   wg-daemon.sh start <actor> [--dir <dir>] [--once] [--interval <secs>]
#   wg-daemon.sh stop <actor>
#   wg-daemon.sh restart <actor> [--dir <dir>] [--once] [--interval <secs>]
#   wg-daemon.sh status <actor>
#   wg-daemon.sh logs <actor> [-f]
#
# Environment:
#   WG_BIN         - Path to wg binary (default: wg)
#   WG_LOG_DIR     - Log directory (default: .workgraph/logs)
#   WG_PID_DIR     - PID file directory (default: .workgraph/pids)

set -e

# Configuration
WG_BIN="${WG_BIN:-wg}"
WG_LOG_DIR="${WG_LOG_DIR:-.workgraph/logs}"
WG_PID_DIR="${WG_PID_DIR:-.workgraph/pids}"

# Parse command
COMMAND="${1:-}"
ACTOR="${2:-}"
shift 2 2>/dev/null || true

# Validate required args
if [ -z "$COMMAND" ] || [ -z "$ACTOR" ]; then
    echo "Usage: $0 <start|stop|restart|status|logs> <actor> [options]"
    exit 1
fi

# Create directories
mkdir -p "$WG_LOG_DIR" "$WG_PID_DIR"

# File paths
PID_FILE="$WG_PID_DIR/wg-agent-$ACTOR.pid"
LOG_FILE="$WG_LOG_DIR/wg-agent-$ACTOR.log"

# Get process status
get_pid() {
    if [ -f "$PID_FILE" ]; then
        cat "$PID_FILE"
    fi
}

is_running() {
    local pid=$(get_pid)
    if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
        return 0
    fi
    return 1
}

# Start the agent
do_start() {
    if is_running; then
        echo "Agent '$ACTOR' is already running (PID: $(get_pid))"
        exit 1
    fi

    echo "Starting wg agent for actor '$ACTOR'..."
    echo "Log file: $LOG_FILE"

    # Build command
    CMD="$WG_BIN agent $ACTOR $*"

    # Start in background with nohup, restart on failure
    (
        while true; do
            echo "[$(date -Iseconds)] Starting agent: $CMD" >> "$LOG_FILE"

            # Run agent, capturing output
            if $CMD >> "$LOG_FILE" 2>&1; then
                echo "[$(date -Iseconds)] Agent exited normally" >> "$LOG_FILE"
                break  # Normal exit, don't restart
            else
                EXIT_CODE=$?
                echo "[$(date -Iseconds)] Agent exited with code $EXIT_CODE, restarting in 5s..." >> "$LOG_FILE"
                sleep 5
            fi
        done
    ) &

    # Save PID
    echo $! > "$PID_FILE"
    echo "Started agent '$ACTOR' (PID: $!)"
}

# Stop the agent
do_stop() {
    if ! is_running; then
        echo "Agent '$ACTOR' is not running"
        rm -f "$PID_FILE"
        return 0
    fi

    local pid=$(get_pid)
    echo "Stopping agent '$ACTOR' (PID: $pid)..."

    # Send SIGTERM for graceful shutdown
    kill -TERM "$pid" 2>/dev/null

    # Wait for process to exit
    for i in {1..30}; do
        if ! is_running; then
            echo "Agent stopped"
            rm -f "$PID_FILE"
            return 0
        fi
        sleep 1
    done

    # Force kill if still running
    echo "Agent didn't stop gracefully, forcing..."
    kill -KILL "$pid" 2>/dev/null
    rm -f "$PID_FILE"
    echo "Agent killed"
}

# Show status
do_status() {
    if is_running; then
        echo "Agent '$ACTOR' is running (PID: $(get_pid))"
        exit 0
    else
        echo "Agent '$ACTOR' is not running"
        exit 1
    fi
}

# Show logs
do_logs() {
    if [ ! -f "$LOG_FILE" ]; then
        echo "No log file found at $LOG_FILE"
        exit 1
    fi

    if [ "$1" = "-f" ]; then
        tail -f "$LOG_FILE"
    else
        tail -100 "$LOG_FILE"
    fi
}

# Execute command
case "$COMMAND" in
    start)
        do_start "$@"
        ;;
    stop)
        do_stop
        ;;
    restart)
        do_stop || true
        sleep 2
        do_start "$@"
        ;;
    status)
        do_status
        ;;
    logs)
        do_logs "$@"
        ;;
    *)
        echo "Unknown command: $COMMAND"
        echo "Usage: $0 <start|stop|restart|status|logs> <actor> [options]"
        exit 1
        ;;
esac
