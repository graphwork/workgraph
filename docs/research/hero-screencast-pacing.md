# Research: Hero Screencast Pacing and Script

**Task:** research-hero-screencast  
**Date:** 2026-03-25  
**Status:** Complete

---

## 1. Current Recording Setup and File Locations

### Recording Toolchain

The screencast system is **fully custom** — no raw `asciinema` CLI recording. Instead:

1. **`record-harness.py`** — Core recording engine. Creates a tmux session at exactly 65×38, polls `tmux capture-pane` at configurable FPS (5–15), and writes asciinema v2 `.cast` files. Provides `type_naturally(text, wpm=N)`, `send_keys()`, `wait_for()`, `sleep()` (with continuous frame capture), and `snapshot()`.

2. **`record-interaction.py`** — The current hero screencast recording script. 7 scenes (0–6): CLI orient → TUI launch → coordinator chat → agent spawn → detail view → round 2 → survey/exit. Uses real coordinator with fallback injection.

3. **`compress-interaction.py`** — Scene-aware post-processor. Detects scene boundaries from content, applies per-scene compression parameters (interaction speed, activity speed, short/long wait caps). Produces `interaction-compressed.cast` + `interaction-timemap.json`.

4. **Website player** — `hero-snippet.html` uses asciinema-player v3.9.0 (CDN) with `interaction.annotations.json` for phase labels. Player options: `fit: 'width'`, `loop: true`, `idleTimeLimit: 1`.

### File Locations

| File | Path | Purpose |
|------|------|---------|
| Recording script | `screencast/record-interaction.py` | Orchestrates 7-scene recording |
| Recording harness | `screencast/record-harness.py` | tmux + cast file engine |
| Compression script | `screencast/compress-interaction.py` | Scene-aware time compression |
| Raw recording | `screencast/recordings/interaction-raw.cast` | 7.8 min, 473 frames |
| Compressed recording | `screencast/recordings/interaction-compressed.cast` | ~60s |
| Timemap | `screencast/recordings/interaction-timemap.json` | Real→compressed time mapping |
| **Deployed hero cast** | `website/assets/casts/interaction.cast` | 2.2 MB, 59.8s, 473 frames |
| Annotations | `website/assets/casts/interaction.annotations.json` | 8 phase labels |
| Website embed | `website/hero-snippet.html` | Embeddable HTML+JS snippet |
| Design doc | `docs/design/screencast-interaction-flow.md` | Storyboard and rationale |

### Older recordings (not the current hero)
- `record-hero-v2.py` / `record-hero-v3.py` — Previous iterations ("Ship the search service" scenario)
- `record-showcase.py` — 6-scene TUI showcase (haiku-news, pre-populated tasks)

---

## 2. Current Script Flow and Timing

### Scene Breakdown (from `interaction.annotations.json`)

| Phase | Compressed Time | Real Time | Ratio | What Happens |
|-------|----------------|-----------|-------|--------------|
| CLI | 0.0–7.6s (7.6s) | 0–19.5s | 2.6× | `wg status`, `wg list`, `wg ready`, `wg viz` |
| Launch | 7.6–9.0s (1.4s) | 19.5–20.5s | 0.7× | `wg tui`, Shift+I to shrink inspector |
| Prompt | 9.0–11.4s (2.4s) | 20.5–27.1s | 2.8× | Type "Build a haiku news pipeline...", submit |
| Agents | 11.4–14.0s (2.6s) | 27.1–29.8s | 1.0× | Tasks created, agents spawn |
| Detail View | 14.0–24.7s (10.7s) | 29.8–41.8s | 1.1× | Detail, Log, Firehose tabs |
| Round 2 | 24.7–31.5s (6.8s) | 41.8–63.7s | 3.2× | "Add a roast mode", new tasks |
| Survey | 31.5–52.0s (20.5s) | 63.7–372.1s | 15.0× | Navigate graph, watch completions |
| Exit | 52.0–60.0s (8.0s) | 372.1–467.3s | 11.9× | Final survey and exit |

### Raw Recording Statistics
- **Total raw duration:** 467.3s (7.8 min)
- **Total compressed duration:** 59.8s
- **Overall compression ratio:** 7.8×
- **Frame count:** 473

### Gap Distribution (raw)
| Gap Size | Frames | Total Time | % of Raw |
|----------|--------|------------|----------|
| <0.1s | 183 | 12.9s | 2.8% |
| 0.1–0.3s | 137 | 27.2s | 5.8% |
| 0.3–1s | 84 | 46.0s | 9.8% |
| 1–3s | 33 | 54.8s | 11.7% |
| 3–10s | 32 | 118.1s | 25.3% |
| >10s | 3 | 208.4s | 44.6% |

---

## 3. Pacing Problems Diagnosed

### Problem 1: Typing dominates the compressed screencast
The CLI phase (0–7.6s) and prompt typing phases together consume ~16s of 60s (27%). The `type_naturally()` function types at 40–50 WPM, which is realistic but **boring in a demo**. The viewer isn't learning anything while watching characters appear one by one.

**Root cause:** `compress-interaction.py` preserves typing at 1.0× speed (`interaction_speed = 1.0` for all scenes). The design doc explicitly says "Typing is real-speed" — but this was a design mistake. Real-speed typing is appropriate for a tutorial, not a hero demo.

### Problem 2: Graph operations and coordinator responses are invisible
The "Agents" phase is only 2.6s compressed. The "Prompt" phase (coordinator responding) is only 2.4s. These are the most interesting parts — the coordinator decomposing a request into tasks, agents claiming work, status transitions — but they flash by.

**Root cause:** The compressor aggressively crushes wait times (>3s gaps → 0.10–0.15s). Since coordinator responses and agent status changes happen during long wait periods (with periodic TUI refreshes), they get compressed away. The compressor doesn't distinguish "boring wait" from "interesting status change happening during a wait."

### Problem 3: No results shown
The haiku news bot task creates tasks like `draft-haikus` and `draft-roast-haikus` that should produce interesting output (generated haiku). The viewer never sees this output. The recording navigates through tabs but doesn't pause on content long enough for the compressed version to preserve it.

**Root cause:** Scene 6 (Survey) is 20.5s but it's mostly fast arrow-key navigation. The script doesn't specifically navigate to a completed task's output tab and linger.

### Problem 4: Abrupt ending
The "Exit" phase is 8s but doesn't provide closure. The viewer sees `q` pressed and the shell returns. There's no moment of "look at what we accomplished" — no final graph survey where all tasks are done.

---

## 4. Speed Control Capabilities

### Per-segment speed control: YES, already supported

The `compress-interaction.py` already supports **per-scene, per-gap-type compression**. Each scene has four tunable parameters:

```python
SCENE_PARAMS = {
    "cli":     (interaction_speed, activity_speed, short_wait_cap, long_wait_cap),
    "launch":  (1.0, 2.0, 0.25, 0.20),
    "chat":    (1.0, 1.5, 0.20, 0.15),
    ...
}
```

- **`interaction_speed`**: Divides gaps <0.3s (typing). Currently 1.0× everywhere. Set to 3.0–5.0 to speed up typing.
- **`activity_speed`**: Divides gaps 0.3–1s (TUI updates). Currently 1.5–3.0×.
- **`short_wait_cap`**: Caps gaps 1–3s. Currently 0.10–0.30s.
- **`long_wait_cap`**: Caps gaps >3s. Currently 0.06–0.25s.

**No additional tooling needed for speed adjustment.** The existing compressor is the right tool. We just need to change the parameters and potentially add more scene granularity.

### What's NOT supported (but could be)
- **Content-aware compression**: The compressor doesn't look at *what* changed between frames, only the time gap. A frame where a task flips from `open` → `in-progress` gets the same treatment as a frame where nothing visible changed. Adding content-aware logic (detect status transitions, pause longer) would improve the result but requires new code.
- **Frame insertion**: Can't add new frames (e.g., a "pause here" frame). Can only adjust timing of existing frames.

---

## 5. Revised Script/Flow Proposal

### Guiding Principles
1. **Typing at 3–5× speed** — fast enough to not bore, slow enough to read
2. **Linger on graph changes** — when tasks appear or status transitions happen, hold the frame
3. **Show the payoff** — navigate to completed task output and pause
4. **Satisfying ending** — final graph view with all tasks done, brief pause, then exit

### Revised Scene Map

| # | Scene | Target Time | Key Changes |
|---|-------|-------------|-------------|
| 0 | CLI Orient | 3–4s | Speed up typing 3×. Show `wg status` and `wg viz` only (drop `wg list` and `wg ready` — redundant) |
| 1 | Launch | 2–3s | Speed up `wg tui` typing 3×. Keep shrink-inspector at real speed (visual) |
| 2 | Chat: Prompt | 4–6s | Speed up typing 3×. **Slow down coordinator response** — increase `short_wait_cap` and `long_wait_cap` so task creation is visible |
| 3 | Agents Spawn | 6–8s | **Key change**: Slow down. Keep status transitions at 2× not 3×. Arrow navigation at real speed |
| 4 | Detail View | 12–15s | Keep at current speed. This is the payoff scene. Extend Firehose hold from 8s to 12s |
| 5 | Round 2 | 4–6s | Speed up typing 3×. Keep coordinator response compression moderate |
| 6 | Results Reveal | 6–8s | **NEW**: Navigate to `draft-haikus` → Log tab → pause 4s showing haiku output. Then navigate to `draft-roast-haikus` → Log tab → pause 3s showing snarky haiku |
| 7 | Final Survey + Exit | 4–5s | Navigate to graph top, pause on full completed graph 3s, `q` to exit, hold shell prompt 1s |

**Target total: 45–55s** (down from 60s, more content-dense)

### Compression Parameter Changes

```python
SCENE_PARAMS = {
    # Scene 0: CLI — typing 3x faster, output waits short
    "cli":         (3.0, 2.0, 0.15, 0.10),
    # Scene 1: Launch — typing 3x faster
    "launch":      (3.0, 2.0, 0.20, 0.15),
    # Scene 2: Chat — typing 3x, but SLOW DOWN coordinator wait
    "chat":        (3.0, 1.2, 0.40, 0.30),   # was (1.0, 1.5, 0.20, 0.15)
    # Scene 3: Agents — moderate, keep transitions visible
    "agents":      (1.5, 1.5, 0.25, 0.20),    # was (1.5, 3.0, 0.15, 0.10)
    # Scene 4: Detail — near real-speed for live output
    "detail":      (1.0, 1.5, 0.40, 0.35),    # was (1.0, 2.0, 0.30, 0.25)
    # Scene 5: Round 2 — typing 3x, coordinator moderate
    "round2":      (3.0, 1.5, 0.30, 0.20),    # was (1.0, 1.5, 0.20, 0.15)
    # Scene 6: Results — slow enough to read output
    "results":     (1.0, 1.5, 0.50, 0.40),    # NEW
    # Scene 7: Survey + exit
    "survey":      (1.5, 2.0, 0.15, 0.10),    # was (1.5, 3.0, 0.10, 0.06)
    "exit":        (1.0, 2.0, 0.20, 0.15),
}
```

### Recording Script Changes (`record-interaction.py`)

1. **Scene 0**: Remove `wg list` and `wg ready` commands. Keep `wg status` and `wg viz`.
2. **Scene 2**: Increase `type_naturally` WPM from 50 to 150 (typing appears 3× faster in raw, compressor preserves at 3× too).
3. **Scene 4**: Increase Firehose hold from `h.sleep(8)` to `h.sleep(12)`.
4. **Scene 5**: Increase WPM to 150.
5. **Scene 6 (NEW "Results Reveal")**: After round 2, navigate to `draft-haikus`, press `2` (Log tab), `h.sleep(5)`. Then navigate to `draft-roast-haikus`, press `2`, `h.sleep(4)`. This ensures the viewer sees the actual haiku output.
6. **Scene 7 (was Scene 6)**: Navigate to graph top, hold 3s on completed graph, `q`, hold 1.5s.

### Alternative: Adjust WPM vs Compressor

Two approaches to speed up typing:

**Option A (Recommended): Increase WPM in recording script**
- Change `wpm=50` → `wpm=150` in `type_naturally()` calls
- Pro: Frames have naturally faster typing, compressor just preserves
- Con: Need to re-record

**Option B: Increase `interaction_speed` in compressor**
- Change from 1.0 to 3.0–5.0 in `SCENE_PARAMS`
- Pro: Can re-compress existing raw recording without re-recording
- Con: Typing looks artificially fast (gaps disappear but frame count stays same)

**Recommendation: Option B first** (immediate improvement, no re-record), then **Option A for the final version** (better quality, re-record needed anyway for new scenes).

---

## 6. Tooling Assessment

### What exists and works
| Tool | Status | Notes |
|------|--------|-------|
| `record-harness.py` | ✅ Solid | tmux-based, reliable, configurable FPS |
| `record-interaction.py` | ✅ Works | 7-scene recording with fallbacks |
| `compress-interaction.py` | ✅ Works | Scene-aware compression, per-scene params |
| asciinema-player 3.9.0 | ✅ Works | CDN-hosted, annotations support |
| `hero-snippet.html` | ✅ Works | Progress bar, annotation display |

### What's needed for the revised screencast

| Change | Effort | Requires Re-record? |
|--------|--------|---------------------|
| Adjust compression parameters | Low (edit `SCENE_PARAMS`) | No |
| Add "results" scene detection to compressor | Low (add content marker) | No |
| Increase typing WPM in recording script | Low (change `wpm=50` → `wpm=150`) | Yes |
| Add "Results Reveal" scene to recording | Medium (new scene function) | Yes |
| Revise "Final Survey" scene | Low (modify existing function) | Yes |
| Drop redundant CLI commands (Scene 0) | Low (remove 2 commands) | Yes |
| Update `interaction.annotations.json` | Low (edit JSON) | No |

### Tools NOT needed
- **agg** (asciinema GIF generator) — not needed, website uses asciinema-player JS
- **asciinema-edit** — not needed, custom compressor handles all post-processing
- **svg-term-cli** — not needed for the same reason
- **asciinema cut** — the custom compressor subsumes this functionality

### Potential improvement: Content-aware frame scoring
The compressor could score each frame by "visual interest" (did task status change? did new task appear? did agent output appear?) and allocate more compressed time to high-interest frames. This would be a new feature (~50-100 lines of Python) but would significantly improve the "agents spawn" and "task completion" phases. **Recommended as a follow-up task, not a blocker.**

---

## 7. Summary and Recommendations

### Quick Win (no re-recording)
1. Edit `compress-interaction.py` `SCENE_PARAMS`: increase `interaction_speed` to 3.0 for cli/launch/chat/round2 scenes, increase wait caps for chat/agents/detail scenes
2. Re-run compressor on existing `interaction-raw.cast`
3. Copy to `website/assets/casts/interaction.cast`
4. Update annotations timing

**Expected improvement:** Typing 3× faster, graph changes more visible. ~40-50s total.

### Full Re-record (recommended for best result)
1. Modify `record-interaction.py`: increase WPM, drop redundant CLI commands, add Results Reveal scene, improve final survey
2. Re-record with real coordinator
3. Compress with new scene parameters
4. Update annotations

**Expected result:** 45-55s screencast where typing is fast, coordinator responses are visible, haiku results are shown, and ending is satisfying.

---

*End of research document.*
