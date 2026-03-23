#!/usr/bin/env python3
"""Recording harness: asciinema + tmux at 65x38.

Provides a reusable class for capturing TUI screencasts with:
- tmux session forced to exactly 65x38 (regardless of outer terminal)
- asciinema-compatible .cast output via tmux capture-pane polling
- Control interface: send keys, type naturally, wait for content, snapshots
- CR+LF line endings guaranteed in cast output

Usage as standalone (quick test):
    ./record-harness.py [--output test.cast] [--cols 65] [--rows 38]

Usage as library:
    from record_harness import RecordingHarness

    with RecordingHarness(cast_file="output.cast") as h:
        h.send_keys("ls", "Enter")
        h.wait_for("$")
        h.type_naturally("echo hello")
        h.send_keys("Enter")
        snapshot = h.snapshot()
        print(snapshot)
"""

import argparse
import json
import os
import random
import re
import signal
import subprocess
import sys
import time


class RecordingHarness:
    """Manages a tmux session at fixed dimensions with cast file recording."""

    def __init__(
        self,
        cast_file="recording.cast",
        cols=65,
        rows=38,
        fps=15,
        session_name=None,
        cwd=None,
        shell_command=None,
        idle_time_limit=2.0,
    ):
        self.cols = cols
        self.rows = rows
        self.fps = fps
        self.cast_file = os.path.abspath(cast_file)
        self.session = session_name or f"rec-harness-{os.getpid()}"
        self.cwd = cwd or os.getcwd()
        self.shell_command = shell_command
        self.idle_time_limit = idle_time_limit

        self._recording = False
        self._start_time = None
        self._prev_content = ""
        self._cast_fh = None
        self._frame_count = 0

    # -- Context manager -------------------------------------------------------

    def __enter__(self):
        self.start()
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        self.stop()
        return False

    # -- Lifecycle -------------------------------------------------------------

    def start(self):
        """Create tmux session, start recording."""
        self._create_tmux_session()
        self._verify_dimensions()
        self._start_recording()

    def stop(self):
        """Stop recording, kill tmux, fix line endings."""
        self._stop_recording()
        self._kill_tmux()
        self._fix_line_endings()

    # -- tmux session management -----------------------------------------------

    def _create_tmux_session(self):
        """Create a tmux session at exactly cols x rows."""
        # Kill any existing session with this name
        self._tmux("kill-session", "-t", self.session)

        # Build the shell command
        if self.shell_command:
            shell_cmd = self.shell_command
        else:
            shell_cmd = f"cd {self.cwd} && exec bash --norc --noprofile"

        # Create detached session with exact dimensions
        self._tmux(
            "new-session", "-d",
            "-s", self.session,
            "-x", str(self.cols),
            "-y", str(self.rows),
            shell_cmd,
        )

        # Force resize (belt and suspenders)
        self._tmux("resize-window", "-t", self.session,
                    "-x", str(self.cols), "-y", str(self.rows))

        # Disable tmux status bar to get exact content area
        self._tmux("set-option", "-t", self.session, "status", "off")

        # Wait for shell to be ready
        time.sleep(0.5)

    def _verify_dimensions(self):
        """Verify tmux pane has the correct dimensions."""
        r = self._tmux("display-message", "-t", self.session, "-p",
                        "#{pane_width}x#{pane_height}")
        if r and r.stdout:
            actual = r.stdout.strip()
            expected = f"{self.cols}x{self.rows}"
            if actual != expected:
                # Try one more resize
                self._tmux("resize-window", "-t", self.session,
                           "-x", str(self.cols), "-y", str(self.rows))
                time.sleep(0.3)
                r = self._tmux("display-message", "-t", self.session, "-p",
                                "#{pane_width}x#{pane_height}")
                actual = r.stdout.strip() if r and r.stdout else "unknown"
                if actual != expected:
                    print(f"WARNING: tmux pane is {actual}, expected {expected}",
                          file=sys.stderr)
            return actual
        return "unknown"

    def _kill_tmux(self):
        """Kill the tmux session."""
        self._tmux("kill-session", "-t", self.session)

    def _tmux(self, *args):
        """Run a tmux command, returning CompletedProcess or None."""
        try:
            return subprocess.run(
                ["tmux"] + list(args),
                capture_output=True, text=True, timeout=10,
            )
        except (subprocess.TimeoutExpired, FileNotFoundError):
            return None

    # -- Recording -------------------------------------------------------------

    def _start_recording(self):
        """Open cast file and begin polling tmux for frames."""
        os.makedirs(os.path.dirname(self.cast_file) or ".", exist_ok=True)

        self._cast_fh = open(self.cast_file, "w")

        # Write asciinema v2 header
        header = {
            "version": 2,
            "width": self.cols,
            "height": self.rows,
            "timestamp": int(time.time()),
            "env": {"TERM": "xterm-256color", "SHELL": "/bin/bash"},
            "idle_time_limit": self.idle_time_limit,
        }
        self._cast_fh.write(json.dumps(header) + "\n")
        self._cast_fh.flush()

        self._start_time = time.monotonic()
        self._recording = True
        self._prev_content = ""
        self._frame_count = 0

    def _stop_recording(self):
        """Flush and close the cast file."""
        # Capture one last frame
        if self._recording:
            self._capture_frame()
        self._recording = False

        if self._cast_fh:
            self._cast_fh.close()
            self._cast_fh = None

    def _capture_frame(self):
        """Capture current tmux pane and write a frame if content changed."""
        if not self._recording or not self._cast_fh:
            return False

        try:
            result = subprocess.run(
                ["tmux", "capture-pane", "-t", self.session, "-e", "-p"],
                capture_output=True, text=True, timeout=2,
            )
            content = result.stdout
        except (subprocess.TimeoutExpired, subprocess.CalledProcessError):
            return False

        if content != self._prev_content:
            elapsed = time.monotonic() - self._start_time

            # Convert bare LF to CR+LF for correct terminal rendering.
            # tmux capture-pane outputs lines separated by \n, but terminals
            # need \r\n (CR returns cursor to column 0, LF moves down).
            # Without CR, text "slides" rightward on each line in playback.
            #
            # Strategy: replace any \n that isn't preceded by \r with \r\n.
            content_fixed = re.sub(r'(?<!\r)\n', '\r\n', content)

            # Clear screen + home cursor, then write full frame
            frame_data = "\x1b[H\x1b[2J" + content_fixed
            event = [round(elapsed, 6), "o", frame_data]
            self._cast_fh.write(json.dumps(event) + "\n")
            self._cast_fh.flush()
            self._prev_content = content
            self._frame_count += 1
            return True
        return False

    def flush_frame(self):
        """Force capture a frame right now (useful after visual changes)."""
        self._capture_frame()

    def _fix_line_endings(self):
        """Post-process cast file to guarantee CR+LF in all output data.

        This is a safety net. The frame capture already converts bare LF
        to CR+LF, but this pass catches anything that slipped through
        (e.g., ANSI sequences that contain bare LF).
        """
        if not os.path.exists(self.cast_file):
            return

        with open(self.cast_file, "r") as f:
            lines = f.readlines()

        fixed_count = 0
        fixed_lines = []

        for i, line in enumerate(lines):
            if i == 0:
                # Header line — don't modify
                fixed_lines.append(line)
                continue

            try:
                event = json.loads(line)
                if len(event) >= 3 and event[1] == "o":
                    original = event[2]
                    # Fix bare \n (not preceded by \r) in the output data
                    fixed = re.sub(r'(?<!\r)\n', '\r\n', original)
                    if fixed != original:
                        event[2] = fixed
                        fixed_count += 1
                    fixed_lines.append(json.dumps(event) + "\n")
                else:
                    fixed_lines.append(line)
            except json.JSONDecodeError:
                fixed_lines.append(line)

        with open(self.cast_file, "w") as f:
            f.writelines(fixed_lines)

        if fixed_count > 0:
            print(f"  Post-process: fixed CR+LF in {fixed_count} frames",
                  file=sys.stderr)

    # -- Control interface -----------------------------------------------------

    def send_keys(self, *keys):
        """Send keys to the tmux session (tmux send-keys syntax).

        Examples:
            h.send_keys("Enter")
            h.send_keys("C-c")
            h.send_keys("Down", "Down")
            h.send_keys("q")
        """
        self._tmux("send-keys", "-t", self.session, *keys)
        # Small delay for tmux to process + capture the frame
        time.sleep(0.05)
        self._capture_frame()

    def send_text(self, text):
        """Send literal text to the tmux session (no key interpretation).

        Unlike send_keys, this sends the exact characters. Use for typing
        text that might contain tmux-special characters.
        """
        self._tmux("send-keys", "-t", self.session, "-l", text)
        time.sleep(0.05)
        self._capture_frame()

    def type_naturally(self, text, wpm=50):
        """Type text with natural-looking keystroke timing.

        Args:
            text: The text to type character by character.
            wpm: Target words per minute (approximate). Default 50.
        """
        # Average chars per word ~5, so chars per second = wpm * 5 / 60
        base_delay = 60.0 / (wpm * 5)

        for ch in text:
            self._tmux("send-keys", "-t", self.session, "-l", ch)
            # Vary timing: faster for common letters, slower for shifts/punctuation
            jitter = random.uniform(0.5, 1.5)
            delay = base_delay * jitter
            if ch in ' \t':
                delay *= 0.7  # Spaces are faster
            elif ch in '!"#$%&\'()*+,-./:;<=>?@[\\]^_`{|}~':
                delay *= 1.3  # Punctuation is slower
            time.sleep(delay)
            self._capture_frame()

    def wait_for(self, pattern, timeout=30, interval=0.2):
        """Wait until pattern appears in the tmux pane content.

        Args:
            pattern: String or compiled regex to search for.
            timeout: Maximum seconds to wait.
            interval: Polling interval in seconds.

        Returns:
            True if pattern found, False if timeout.
        """
        if isinstance(pattern, str):
            check = lambda s: pattern in s
        else:
            check = lambda s: pattern.search(s)

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            content = self.snapshot(capture_frame=True)
            if check(content):
                return True
            time.sleep(interval)

        print(f"WARNING: wait_for timed out after {timeout}s for: {pattern}",
              file=sys.stderr)
        return False

    def wait_absent(self, pattern, timeout=30, interval=0.2):
        """Wait until pattern is NOT present in the tmux pane.

        Useful for waiting for something to disappear (e.g., a loading indicator).

        Args:
            pattern: String or compiled regex.
            timeout: Maximum seconds to wait.
            interval: Polling interval.

        Returns:
            True if pattern disappeared, False if timeout.
        """
        if isinstance(pattern, str):
            check = lambda s: pattern in s
        else:
            check = lambda s: pattern.search(s)

        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            content = self.snapshot(capture_frame=True)
            if not check(content):
                return True
            time.sleep(interval)
        return False

    def snapshot(self, capture_frame=False):
        """Capture and return the current tmux pane content as plain text.

        Args:
            capture_frame: If True, also write a recording frame.

        Returns:
            The pane content as a string.
        """
        try:
            result = subprocess.run(
                ["tmux", "capture-pane", "-t", self.session, "-p"],
                capture_output=True, text=True, timeout=2,
            )
            content = result.stdout
        except (subprocess.TimeoutExpired, subprocess.CalledProcessError):
            content = ""

        if capture_frame:
            self._capture_frame()

        return content

    def sleep(self, seconds):
        """Sleep while continuing to capture frames.

        This keeps the recording alive during pauses (e.g., waiting for
        the viewer to read something).
        """
        interval = 1.0 / self.fps
        deadline = time.monotonic() + seconds
        while time.monotonic() < deadline:
            self._capture_frame()
            remaining = deadline - time.monotonic()
            time.sleep(min(interval, max(0, remaining)))

    # -- Informational ---------------------------------------------------------

    @property
    def duration(self):
        """Elapsed recording time in seconds."""
        if self._start_time is None:
            return 0
        return time.monotonic() - self._start_time

    @property
    def frame_count(self):
        """Number of frames written to the cast file."""
        return self._frame_count

    def __repr__(self):
        status = "recording" if self._recording else "stopped"
        return (f"RecordingHarness({self.cols}x{self.rows}, "
                f"session={self.session!r}, {status}, "
                f"frames={self._frame_count})")


# -- Standalone test -----------------------------------------------------------

def _run_test(args):
    """Quick self-test: start harness, type commands, verify output."""
    print(f"=== Recording Harness Test ===")
    print(f"  Dimensions: {args.cols}x{args.rows}")
    print(f"  Output: {args.output}")
    print(f"  FPS: {args.fps}")

    random.seed(42)

    with RecordingHarness(
        cast_file=args.output,
        cols=args.cols,
        rows=args.rows,
        fps=args.fps,
    ) as h:
        # Verify dimensions
        dims = h._verify_dimensions()
        print(f"  Actual tmux pane: {dims}")

        # Let shell prompt appear
        h.sleep(1)

        # Type a command naturally
        print("  Typing 'echo hello world'...")
        h.type_naturally("echo hello world")
        h.send_keys("Enter")
        h.sleep(1)

        # Wait for output
        found = h.wait_for("hello world", timeout=5)
        print(f"  Output appeared: {found}")

        # Take a snapshot
        snap = h.snapshot()
        print(f"  Snapshot ({len(snap)} chars):")
        for line in snap.strip().split('\n')[:5]:
            print(f"    | {line}")

        # Type another command
        print("  Typing 'ls -la'...")
        h.type_naturally("ls -la")
        h.send_keys("Enter")
        h.sleep(2)

        # Send special keys
        print("  Sending Ctrl-L (clear)...")
        h.send_keys("C-l")
        h.sleep(1)

        print(f"  Duration: {h.duration:.1f}s, Frames: {h.frame_count}")

    # Verify cast file
    print(f"\n=== Verifying {args.output} ===")
    _verify_cast(args.output)


def _verify_cast(path):
    """Verify cast file dimensions and line endings."""
    if not os.path.exists(path):
        print("  ERROR: Cast file not found!")
        return False

    ok = True
    with open(path) as f:
        lines = f.readlines()

    if len(lines) < 2:
        print("  ERROR: Cast file has fewer than 2 lines (header + frames)")
        return False

    # Check header
    header = json.loads(lines[0])
    w, h = header.get("width"), header.get("height")
    print(f"  Header: width={w}, height={h}, version={header.get('version')}")
    if w != 65 or h != 38:
        print(f"  WARNING: Expected 65x38, got {w}x{h}")
        ok = False
    else:
        print(f"  Dimensions: OK (65x38)")

    # Check CR+LF in frame data
    bare_lf_count = 0
    crlf_count = 0
    for i, line in enumerate(lines[1:], 1):
        try:
            event = json.loads(line)
            if len(event) >= 3 and event[1] == "o":
                data = event[2]
                # Count line endings in the output data
                # Look for \n not preceded by \r
                bare = len(re.findall(r'(?<!\r)\n', data))
                cr = data.count('\r\n')
                bare_lf_count += bare
                crlf_count += cr
        except json.JSONDecodeError:
            pass

    print(f"  Line endings: {crlf_count} CR+LF, {bare_lf_count} bare LF")
    if bare_lf_count > 0:
        print(f"  ERROR: Found {bare_lf_count} bare LF occurrences!")
        ok = False
    else:
        print(f"  Line endings: OK (all CR+LF)")

    frame_count = len(lines) - 1
    last_event = json.loads(lines[-1])
    duration = last_event[0] if isinstance(last_event, list) else 0
    print(f"  Frames: {frame_count}, Duration: {duration:.1f}s")

    if ok:
        print("  ALL CHECKS PASSED")
    return ok


if __name__ == "__main__":
    parser = argparse.ArgumentParser(
        description="Recording harness: asciinema + tmux at 65x38",
        formatter_class=argparse.RawDescriptionHelpFormatter,
        epilog=__doc__,
    )
    parser.add_argument("--output", "-o",
                        default=os.path.join(os.path.dirname(os.path.abspath(__file__)),
                                             "recordings", "harness-test.cast"),
                        help="Output cast file path")
    parser.add_argument("--cols", type=int, default=65,
                        help="Terminal width (default: 65)")
    parser.add_argument("--rows", type=int, default=38,
                        help="Terminal height (default: 38)")
    parser.add_argument("--fps", type=int, default=15,
                        help="Capture frames per second (default: 15)")
    parser.add_argument("--verify-only", metavar="FILE",
                        help="Only verify an existing cast file")

    args = parser.parse_args()

    if args.verify_only:
        ok = _verify_cast(args.verify_only)
        sys.exit(0 if ok else 1)

    _run_test(args)
