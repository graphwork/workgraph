# Research: HOWTO Submission Review & Readiness Assessment

**Task:** research-review-howto
**Date:** 2026-04-07
**Source:** `/home/erik/executors/workgraph/docs/terminal-bench/HOWTO-submit-to-leaderboard.md`

---

## Q1: Exact Submission Format Required

The leaderboard submission to HuggingFace (`harborframework/terminal-bench-2-leaderboard`) requires:

### Directory Structure
```
submissions/terminal-bench/2.0/<agent-name>__<model>/
  metadata.yaml        # Hand-written
  job-001/             # Harbor's raw output
    config.json
    trial-1/result.json
    trial-2/result.json
    trial-3/result.json
    trial-4/result.json
    trial-5/result.json
```

### metadata.yaml Format
```yaml
agent_url: https://github.com/graphwork/workgraph
agent_display_name: "Workgraph"
agent_org_display_name: "Poietic PBC"

models:
  - model_name: minimax-m2.7
    model_provider: openrouter
    model_display_name: "MiniMax-M2.7"
    model_org_display_name: "MiniMax"
```

### Validation Bot Checks
- `timeout_multiplier` must be `1.0`
- No timeout or resource overrides
- Minimum **5 trials per task** (hard requirement)
- Valid `result.json` in every trial directory

### Rules
- Default timeouts (15 min–3.3 hours per task, defined in task.toml)
- Default resources (1-2 CPUs, 2-4 GB RAM, 10 GB storage)
- No overrides of any kind
- Multi-agent architectures allowed
- Retry/convergence loops within a trial allowed
- Agents cannot access tbench.ai or terminal-bench GitHub repo
- Must scrub API keys and proprietary prompts (submissions are public)

---

## Q2: Do We Already Have This Doc?

**No, the HOWTO does not exist in the workgraph repo.** It lives in the executors repo at:
`/home/erik/executors/workgraph/docs/terminal-bench/HOWTO-submit-to-leaderboard.md`

What we DO have in the workgraph repo:
- `terminal-bench/leaderboard-submission/README.md` — a status doc noting conditions A/B/C need 2 more trials
- `terminal-bench/prepare-leaderboard.sh` — copies trial data into submission format
- `terminal-bench/docs/scale-experiment-design.md` — detailed experiment design for A vs F
- `terminal-bench/docs/inventory.md` — resource inventory

The `leaderboard-submission/README.md` covers the "what to do" at a high level but does NOT include:
- The Harbor adapter class implementation details
- The `harbor run` commands
- The exact HuggingFace submission workflow (fork → PR → bot validation)
- The submission rules (timeout, resource, scrubbing constraints)

---

## Q3: Should We Copy or Adapt?

**Recommendation: Extract key steps, don't copy verbatim.**

Reasons:
1. The HOWTO focuses on Harbor adapter implementation (Python class), which is already implemented in `wg/adapter.py`
2. Our actual submission workflow is more complex (conditions A/B/C/F, `prepare-leaderboard.sh`)
3. The relevant actionable steps are a subset of the HOWTO (the "Package for Submission" and "Submit" sections)
4. The `leaderboard-submission/README.md` already serves as our submission checklist — it just needs updating with:
   - The full submission rules from the HOWTO
   - Condition F submission directory
   - The HuggingFace workflow steps (fork → PR → bot)

---

## Q4: Pilot-a-89 Results Format vs Required Format

### Our Format (Harbor-generated, correct)
```
results/pilot-a-89/pilot-a-89/
  config.json                            # Job-level config
  adaptive-rejection-sampler__MuV74Gw/   # Per-trial dirs (task__hash)
    agent/agent_loop.ndjson
    artifacts/
    config.json
    result.json                          # ← This is the key file
    trial.log
    verifier/
```

### Required Format (HOWTO simplified view)
```
job-001/
  config.json
  trial-1/result.json
  trial-2/result.json
  ...
```

### Assessment
The HOWTO's `trial-1/`, `trial-2/` naming is a **simplified illustration**. Harbor actually produces `{task-name}__{hash}/` directories, which is what we have. **Our result.json files ARE in Harbor's native format.** The existing `prepare-leaderboard.sh` correctly copies these into the submission structure.

### Critical Gap: Trial Count
- **pilot-a-89**: 89 tasks × **1 trial** each → NOT submittable (need 5)
- **full-f-m27**: 89 tasks × **3 trials** each → NOT submittable (need 5)
- **full-aprime-m27**: 89 tasks × **3 trials** each → NOT submittable (need 5)
- **rerun-condition-a/b**: 89 tasks × **3 trials** each → NOT submittable (need 5)
- **full-condition-c**: 89 tasks × **3 trials** each → NOT submittable (need 5)

**No condition currently has 5 trials per task.** All need 2 additional trial runs.

---

## Q5: Full Set of Terminal-Bench Tasks

**Terminal-Bench 2.0 has exactly 89 tasks.** This is confirmed by:
- pilot-a-89 summary: `"total": 89`
- scale-experiment-design.md: "89 tasks (canonical set)"
- HOWTO: "89 tasks × 5 trials" for full submission
- All experiment runners (`run_pilot_a_89.py`, `run_pilot_f_89.py`, etc.) target 89 tasks

The full task list is in the Harbor dataset at `terminal-bench/terminal-bench-2`. Each task is defined by a `task.toml` + Docker image in the Harbor package format.

Additionally, there are **18 custom tasks** (8 calibration + 10 hard benchmarks) that are NOT part of the TB 2.0 leaderboard. These use host-native execution and are separate from the leaderboard submission.

---

## Q6: Can We Submit Conditions A and F Separately?

**Yes, absolutely.** The HOWTO and leaderboard format treat each `<agent-name>__<model>/` directory as an independent submission entry. We already have this pattern:
- `workgraph-condition-a__minimax-m2.7/` with its own `metadata.yaml`
- `workgraph-condition-b__minimax-m2.7/` with its own `metadata.yaml`
- `workgraph-condition-c__minimax-m2.7/` with its own `metadata.yaml`

To submit F, we need:
- `workgraph-condition-f__minimax-m2.7/metadata.yaml` (doesn't exist yet)
- 5 trials per task from the F condition runs

The `agent_display_name` field in `metadata.yaml` differentiates them on the leaderboard (e.g., "Workgraph Condition A (bare agent)" vs "Workgraph Condition F (wg-native context)").

**Note:** The leaderboard rules explicitly allow multi-agent and orchestrator architectures, so Condition F's use of workgraph context injection is fully compliant.

---

## Gap Analysis: What We Have vs What We Need

### Have ✅
| Item | Status | Location |
|------|--------|----------|
| Harbor adapter implementation | Complete | `wg/adapter.py` (ConditionAAgent through ConditionFAgent) |
| `metadata.yaml` for A/B/C | Complete | `leaderboard-submission/workgraph-condition-{a,b,c}__minimax-m2.7/` |
| `prepare-leaderboard.sh` | Complete | `terminal-bench/prepare-leaderboard.sh` |
| Trial data for A/B/C (3 trials) | Complete | `results/rerun-condition-a/`, `rerun-condition-b/`, `full-condition-c/` |
| Trial data for F (3 trials) | Complete | `results/full-f-m27/` |
| Trial data for A' (3 trials) | Complete | `results/full-aprime-m27/` |
| `result.json` format | Correct | Harbor-native format, passes validation |
| `timeout_multiplier: 1.0` | Correct | All configs verified |

### Missing ❌
| Item | Gap | Effort |
|------|-----|--------|
| **5 trials per task** | Have 3, need 5 for ALL conditions | 2 more trial runs per condition (~8h each) |
| **metadata.yaml for F** | Doesn't exist in `leaderboard-submission/` | 2 minutes to create |
| **prepare-leaderboard.sh for F** | Script only handles A/B/C, not F | ~10 lines to add |
| **F submission directory** | `leaderboard-submission/workgraph-condition-f__minimax-m2.7/` doesn't exist | mkdir + metadata.yaml |
| **API key scrubbing** | Not verified whether logs contain keys | Audit needed before submission |
| **HuggingFace fork** | Not yet created | Manual step |

### Decisions Needed
| Decision | Options | Recommendation |
|----------|---------|---------------|
| Which conditions to submit? | A only / F only / A+F / A+B+C+F | **A+F minimum** (the primary comparison). B and C optional. |
| Agent display name for F | "Workgraph Condition F (wg-native)" / "Workgraph (context-injected)" | Use descriptive name that explains the treatment |
| Agent org name | "Workgraph" / "Poietic PBC" | Condition A metadata says "Workgraph", HOWTO example says "Poietic PBC" — pick one consistently |
| Run 2 more trials via Harbor or host-native? | Harbor (proven) / host-native (faster) | Harbor — matches existing data format exactly |

---

## Recommendation

### Immediate Actions (pre-submission)
1. **Create F submission directory** with `metadata.yaml`
2. **Update `prepare-leaderboard.sh`** to include Condition F source directories
3. **Run 2 more trials** per condition (A and F at minimum) via `harbor run -k 2`
4. **Audit for API keys** in agent logs before copying to submission
5. **Update `leaderboard-submission/README.md`** with HOWTO rules and F condition

### Submission Workflow
1. Fork `harborframework/terminal-bench-2-leaderboard` on HuggingFace
2. Copy `workgraph-condition-a__minimax-m2.7/` and `workgraph-condition-f__minimax-m2.7/` into `submissions/terminal-bench/2.0/`
3. Open PR — bot validates format and trial count
4. Maintainer reviews and merges
5. Results appear at tbench.ai/leaderboard/terminal-bench/2.0
