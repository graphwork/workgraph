#!/usr/bin/env python3
"""Capture tmux pane output and produce an asciinema .cast file.

Usage: capture-tmux.py <session> <output.cast> [--cols 120] [--rows 36] [--fps 10]

Captures tmux pane content at the given FPS using `tmux capture-pane -e -p`
(including ANSI escape sequences) and writes asciinema v2 .cast format.

Send SIGTERM or SIGINT to stop recording.
"""

import json
import subprocess
import sys
import time
import signal

def main():
    import argparse
    parser = argparse.ArgumentParser()
    parser.add_argument("session", help="tmux session name")
    parser.add_argument("output", help="output .cast file path")
    parser.add_argument("--cols", type=int, default=120)
    parser.add_argument("--rows", type=int, default=36)
    parser.add_argument("--fps", type=int, default=10)
    args = parser.parse_args()

    running = True
    def handle_signal(signum, frame):
        nonlocal running
        running = False
    signal.signal(signal.SIGTERM, handle_signal)
    signal.signal(signal.SIGINT, handle_signal)

    interval = 1.0 / args.fps
    start_time = time.monotonic()
    prev_content = ""

    with open(args.output, "w") as f:
        # Write header
        header = {
            "version": 2,
            "width": args.cols,
            "height": args.rows,
            "timestamp": int(time.time()),
            "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"},
            "idle_time_limit": 2.0,
        }
        f.write(json.dumps(header) + "\n")

        while running:
            try:
                result = subprocess.run(
                    ["tmux", "capture-pane", "-t", args.session, "-e", "-p"],
                    capture_output=True, text=True, timeout=2,
                )
                content = result.stdout
            except (subprocess.TimeoutExpired, subprocess.CalledProcessError):
                break

            # Only write frames that changed
            if content != prev_content:
                elapsed = time.monotonic() - start_time
                # Clear screen + move to top-left, then write full frame
                # This ensures each frame renders cleanly
                frame_data = "\x1b[H\x1b[2J" + content
                event = [round(elapsed, 6), "o", frame_data]
                f.write(json.dumps(event) + "\n")
                f.flush()
                prev_content = content

            time.sleep(interval)

    print(f"Recording saved: {args.output}", file=sys.stderr)

if __name__ == "__main__":
    main()
