#!/usr/bin/env python3
"""Compress an asciinema .cast file for demo purposes.

Remaps timestamps to achieve a target duration while keeping typing speed
natural and compressing long waiting periods. Optionally trims the recording.

Usage: python3 compress-cast.py input.cast output.cast [--trim-after SECONDS]
"""

import json
import sys
import re


def analyze(events):
    """Find key timeline milestones in the recording."""
    milestones = {}
    for event in events:
        t, typ, text = event
        clean = re.sub(r'\x1b\[[^a-zA-Z]*[a-zA-Z]', '', text).lower()

        if 'typing_start' not in milestones and t > 2:
            # Single chars being typed
            if len(clean.strip()) <= 3 and len(clean.strip()) >= 1:
                milestones['typing_start'] = t

        if 'prompt_visible' not in milestones:
            if 'plan' in clean and 'heist' in clean:
                milestones['prompt_visible'] = t

        if 'task_creation' not in milestones:
            if 'wg add' in clean:
                milestones['task_creation'] = t

        if 'parallel' not in milestones:
            if clean.count('in-progress') >= 2:
                milestones['parallel'] = t

        if '(done' in clean:
            done_count = clean.count('(done')
            if done_count >= 2 and 'multi_done' not in milestones:
                milestones['multi_done'] = t
            if done_count >= 1:
                milestones['last_done'] = t

    return milestones


def compress(input_path, output_path, trim_after=None):
    with open(input_path) as f:
        lines = f.readlines()

    header = json.loads(lines[0])
    events = [json.loads(line) for line in lines[1:]]

    if not events:
        print("No events found")
        return

    total_duration = events[-1][0]
    print(f"Input: {len(events)} frames, {total_duration:.1f}s")

    # Analyze milestones
    milestones = analyze(events)
    print(f"Milestones: {milestones}")

    # Trim if requested
    if trim_after:
        events = [e for e in events if e[0] <= trim_after]
        total_duration = events[-1][0] if events else 0
        print(f"Trimmed to {len(events)} frames, {total_duration:.1f}s")

    # Define adaptive compression zones based on milestones
    typing_start = milestones.get('typing_start', 5.0)
    typing_end = milestones.get('prompt_visible', 15.0) + 2  # 2s after prompt visible
    task_creation = milestones.get('task_creation', typing_end + 30)
    parallel = milestones.get('parallel', task_creation + 60)
    multi_done = milestones.get('multi_done', parallel + 30)

    zones = [
        (0.0, typing_start, 2.0),                          # TUI loads
        (typing_start, typing_end, 8.0),                    # Typing prompt
        (typing_end, task_creation - 2, 3.0),               # Coordinator thinks
        (task_creation - 2, task_creation + 10, 5.0),       # Tasks created
        (task_creation + 10, parallel - 5, 4.0),            # Agents dispatching
        (parallel - 5, parallel + 20, 6.0),                 # Parallel execution visible
        (parallel + 20, multi_done - 5, 5.0),               # Agents working
        (multi_done - 5, total_duration + 1, 5.0),          # Completion + exit
    ]

    # Build time mapping
    compressed_events = []
    for event in events:
        old_t = event[0]
        new_t = 0.0
        for zone_start, zone_end, zone_target in zones:
            if old_t < zone_start:
                break
            elif old_t < zone_end:
                zone_progress = (old_t - zone_start) / max(zone_end - zone_start, 0.001)
                new_t += zone_progress * zone_target
                break
            else:
                new_t += zone_target

        compressed_events.append([round(new_t, 6), event[1], event[2]])

    # Apply additional idle time limit (max 0.5s gap)
    max_gap = 0.5
    final_events = []
    offset = 0.0
    prev_t = 0.0
    for event in compressed_events:
        adjusted_t = event[0] - offset
        gap = adjusted_t - prev_t
        if gap > max_gap:
            offset += gap - max_gap
            adjusted_t = prev_t + max_gap
        prev_t = adjusted_t
        final_events.append([round(adjusted_t, 6), event[1], event[2]])

    actual_duration = final_events[-1][0] if final_events else 0
    print(f"Output: {len(final_events)} frames, {actual_duration:.1f}s")

    header["idle_time_limit"] = max_gap

    with open(output_path, "w") as f:
        f.write(json.dumps(header) + "\n")
        for event in final_events:
            f.write(json.dumps(event) + "\n")

    print(f"Saved to {output_path}")


if __name__ == "__main__":
    if len(sys.argv) < 3:
        print(f"Usage: {sys.argv[0]} input.cast output.cast [--trim-after SECONDS]")
        sys.exit(1)

    input_path = sys.argv[1]
    output_path = sys.argv[2]
    trim = None
    if '--trim-after' in sys.argv:
        idx = sys.argv.index('--trim-after')
        trim = float(sys.argv[idx + 1])

    compress(input_path, output_path, trim)
