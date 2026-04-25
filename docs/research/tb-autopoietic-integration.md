# Research: Autopoietic TB-Workgraph Integration

## Summary

Terminal Bench (TB) trials currently bypass workgraph's agency pipeline. The harbor
adapter (`terminal-bench/wg/adapter.py`) runs agents directly via litellm inside
Docker containers, managing its own agent loop. The full agency lifecycle
(`.assign` -> agent executes -> `.flip` -> `.evaluate` -> `.verify`) never fires.
This document designs the integration so that TB trials run through `wg service
start`, enabling FLIP-based false-PASS detection.

---

## Q1: How Does the TB Harbor Adapter Currently Dispatch Agents?

**File**: `terminal-bench/wg/adapter.py`

The adapter implements Harbor's `BaseAgent` protocol:

1. **`setup()`** (line 1296): Creates a temp directory on the host, runs `wg init`
   to initialize a host-side workgraph. For conditions D/E, it bootstraps agency
   (`wg agency init`, `wg agent create`).

2. **`run()`** (line 1351): Runs an LLM-in-the-loop agent. Key flow:
   - Creates a root task via `_exec_wg_cmd_host()` (host-side `wg add`)
   - Builds condition-specific system prompt (A through F)
   - Enters a turn loop: `litellm.acompletion()` -> parse tool calls -> execute tools
   - Tool execution: `bash`, `read_file`, `write_file` run inside the Docker container
     via `env.exec()`; `wg_*` tools run on the host via `_exec_wg_cmd_host()`
   - Loop terminates on: no tool calls, `wg_done`/`wg_fail` on root task, or max turns

3. **Interface between harbor and agent**: Harbor provides `BaseEnvironment` (Docker
   container with `exec()` method) and `AgentContext` (metadata accumulator). The
   adapter does ALL orchestration — Harbor just provides the sandbox and scoring.

**What's missing**: The host-side workgraph is isolated per trial (tempdir). No
coordinator runs. No `.assign-*`, `.flip-*`, or `.evaluate-*` tasks are ever created
because `scaffold_full_pipeline()` (`src/commands/eval_scaffold.rs`) only runs inside
the coordinator loop (`src/commands/service/coordinator.rs`).

---

## Q2: Making TB Tasks Into Real Workgraph Tasks

**Goal**: Replace the adapter's custom agent loop with `wg service start`.

### Architecture: TB Task -> wg Task -> Agency Pipeline -> Results Collection

```
TB Trial Runner (Python)
  |
  |  For each TB task definition:
  |    1. wg add "TB: <task-name>" --verify "<harbor-verifier-cmd>" \
  |         -d "<task instruction from harbor>"
  |    2. Configure .workgraph/config.toml:
  |         [agency] auto_assign=true, auto_evaluate=true, flip_enabled=true
  |         [coordinator] verify_mode="separate"
  |
  v
wg service start --max-agents N
  |
  |  Coordinator loop (coordinator.rs, line ~3530):
  |    Phase 4.2: scaffold_full_pipeline() creates:
  |      .assign-tb-task -> tb-task -> .flip-tb-task -> .evaluate-tb-task
  |    Phase 4.3: build_auto_assign_tasks() runs lightweight LLM assignment
  |    Phase 4.5: Dispatches ready tasks via spawn_agent()
  |    Phase 4.55: build_flip_verification_tasks() creates .verify-* on low FLIP
  |    Phase 4.55: build_separate_verify_tasks() if verify_mode=separate
  |
  v
Agent executes in worktree (isolated git working tree)
  |  - Receives TB task instruction as wg task description
  |  - Has full tool access (bash, file ops, wg tools)
  |  - Uses the same Harbor Docker env? (see Blocker #1)
  |
  v
Agent calls wg done -> triggers:
  1. .flip-* task runs FLIP inference + comparison
  2. .evaluate-* task runs structured evaluation (4-dim scoring)
  3. If FLIP score < threshold: .verify-* task auto-created (Opus verification)
  |
  v
Results Collection Task (--after all trial tasks)
  - Reads evaluations from .workgraph/agency/evaluations/
  - Compares FLIP scores against Harbor's external verifier results
  - Produces correlation analysis: FLIP vs ground truth
```

### Key Design Decision: Docker Environment Access

**Blocker #1**: The biggest gap. Currently, `wg service start` spawns agents as
Claude Code sessions in git worktrees on the HOST. TB tasks require execution inside
Harbor's Docker containers (specific toolchains, test environments, etc.).

**Options**:

A. **Shell executor + Docker bridge**: Configure tasks with `exec_mode: shell` and
   have the exec command start a Docker container matching the Harbor task spec. The
   agent's bash commands get proxied into the container.

B. **Custom executor type**: Add a `harbor` executor to workgraph that wraps
   `BaseEnvironment.exec()`. This is the cleanest but requires Rust code changes.

C. **Host-side execution only**: For tasks that don't need specialized Docker envs
   (most coding tasks have standard toolchains), run directly on the host. This works
   for a large subset of TB tasks but not all.

**Recommendation**: Option A for the first iteration. Each wg task gets an `exec`
field like `docker exec <container-id> sh -c "{command}"`. The TB runner pre-starts
containers, records container IDs, and wires them into task descriptions.

---

## Q3: Capturing TB External Verifier Results vs FLIP Scores

Harbor's external verifier runs test suites inside the Docker container AFTER the
agent finishes. Its results are binary (PASS/FAIL) with detailed test output.

### Data Collection Strategy

```
For each trial task:
  1. wg service runs the full pipeline -> produces:
     - .flip-* score (0.0 - 1.0 fidelity score)
     - .evaluate-* score (0.0 - 1.0, 4 dimensions)
     - .verify-* result if triggered (PASS/FAIL via Opus review)
     
  2. Harbor verifier runs independently -> produces:
     - Binary PASS/FAIL
     - Test output details
     
  3. Results collector joins on task ID:
     | task_id | flip_score | eval_score | verify_triggered | verify_result | harbor_pass |
     |---------|-----------|------------|-----------------|---------------|-------------|
     | chess   | 0.82      | 0.85       | no              | -             | PASS        |
     | pov-ray | 0.45      | 0.60       | yes             | FAIL          | FAIL        |
     | regex   | 0.91      | 0.90       | no              | -             | PASS        |
     | cython  | 0.78      | 0.80       | no              | -             | FAIL ← false PASS candidate |
```

### Key Analysis: Does FLIP Catch What the External Verifier Catches?

The critical question is the **false-PASS rate**. Currently 33% of tasks where the
agent calls `wg_done` actually fail Harbor's verifier. FLIP should catch these by
comparing the agent's stated intent (inferred from output) against the actual task
specification.

**Metrics to compute**:
- FLIP sensitivity: % of harbor-FAIL tasks where FLIP score < threshold
- FLIP specificity: % of harbor-PASS tasks where FLIP score >= threshold
- Optimal FLIP threshold: ROC curve analysis
- Correlation: Pearson/Spearman between FLIP score and harbor binary outcome

---

## Q4: Configuration Knobs

### Existing knobs (from `src/config.rs`):

| Knob | Location | Current Default | TB Trial Setting |
|------|----------|----------------|-----------------|
| `agency.auto_assign` | config.toml | false | **true** |
| `agency.auto_evaluate` | config.toml | false | **true** |
| `agency.flip_enabled` | config.toml | false | **true** |
| `agency.auto_place` | config.toml | false | false (flat task graph) |
| `agency.auto_create` | config.toml | false | false (pre-seeded agents) |
| `agency.triage_timeout` | config.toml | 30s | **300s** (evals need time) |
| `coordinator.verify_mode` | config.toml | "inline" | **"separate"** |
| `coordinator.decomp_guidance` | config.toml | true | true |
| `coordinator.max_agents` | CLI | 1 | N (fanout parallelism) |
| `agency.flip_threshold` | config.toml | 0.6 | tunable per experiment |

### Per-task knobs:

| Knob | Mechanism | Purpose |
|------|-----------|---------|
| `--verify "<cmd>"` | Task flag | Machine-checkable pass/fail gate |
| `--model <model>` | Task flag | Per-task model override |
| `context_scope` | Task field | clean/task/graph/full |
| `exec_mode` | Task field | shell/bare/light/full |
| `tags: [flip-eval]` | Task tag | Force FLIP even if globally disabled |

---

## Q5: Fanout Design

### Single Condition Trial (e.g., 50 TB tasks, N=3 replicas each)

```bash
# Phase 1: Create trial tasks (Python TB runner generates these)
for task in tb_task_definitions:
    for replica in range(N):
        wg add "TB-{condition}: {task.name} (rep {replica})" \
            --id "tb-{task.id}-r{replica}" \
            --verify "{task.verify_command}" \
            -d "{task.instruction}"

# Phase 2: Create results collection task
wg add "Collect: {condition} results" \
    --id "collect-{condition}" \
    --after "$(all_trial_task_ids | join ',')" \
    -d "Read all evaluations, FLIP scores, and verify results. Produce correlation table."

# Phase 3: Launch coordinator
wg service start --max-agents 4
```

The coordinator handles:
- Scaffolding `.assign-*`, `.flip-*`, `.evaluate-*` for each trial task
- Dispatching agents in parallel (up to `max_agents`)
- Agent isolation via git worktrees
- Separate verification when `verify_mode=separate`

### Multi-Condition Comparison (A vs B vs D)

```bash
# Create condition-specific configs by setting agency knobs differently
# Condition A: no agency pipeline
# Condition D: full pipeline with auto_assign + flip_enabled
# Run each condition as a separate wg instance (separate .workgraph dirs)
# OR use coordinator namespacing (separate coordinators per condition)
```

### Proposed Task Graph for a Single TB Trial Run

```bash
# Initialize
wg init
wg agency init
wg agent create solver --role programmer --tradeoff careful

# Configure for full pipeline
wg config set agency.auto_assign true
wg config set agency.auto_evaluate true
wg config set agency.flip_enabled true
wg config set coordinator.verify_mode separate
wg config set agency.triage_timeout 300

# Create trial tasks (example: 3 tasks from TB)
wg add "TB: chess-best-move" \
    --id "tb-chess" \
    --verify "cd /tests && python -m pytest test_outputs.py -v" \
    -d "$(cat tb_tasks/chess-best-move/instruction.txt)"

wg add "TB: build-pov-ray" \
    --id "tb-povray" \
    --verify "cd /tests && python -m pytest test_outputs.py -v" \
    -d "$(cat tb_tasks/build-pov-ray/instruction.txt)"

wg add "TB: regex-log" \
    --id "tb-regex" \
    --verify "cd /tests && python -m pytest test_outputs.py -v" \
    -d "$(cat tb_tasks/regex-log/instruction.txt)"

# Create results collection task
wg add "Collect trial results" \
    --id "collect-results" \
    --after "tb-chess,tb-povray,tb-regex" \
    -d "## Description
Read .workgraph/agency/evaluations/ for all completed trial tasks.
Compare FLIP scores against harbor verifier outcomes.
Produce correlation table and false-PASS analysis.

## Validation
- [ ] Correlation table produced with FLIP vs harbor-verifier columns
- [ ] False-PASS rate calculated with and without FLIP gating"

# Launch
wg service start --max-agents 3
```

After `wg service start`, the coordinator auto-scaffolds:
```
.assign-tb-chess -> tb-chess -> .flip-tb-chess -> .evaluate-tb-chess
.assign-tb-povray -> tb-povray -> .flip-tb-povray -> .evaluate-tb-povray  
.assign-tb-regex -> tb-regex -> .flip-tb-regex -> .evaluate-tb-regex
                                                   |
collect-results <--(after tb-chess, tb-povray, tb-regex)--+
```

---

## Evaluation Failure Root Cause Analysis

### Two distinct failure modes observed:

#### Failure Mode 1: Timeout (exit code 124 = SIGALRM)

**Task**: `.evaluate-.verify-impl-thinking-tokens`
**Error**: `Claude CLI call failed (exit Some(124))`

**Root cause**: The `run_lightweight_llm_call()` function (`src/service/llm.rs:100`)
wraps the `claude` CLI in a `timeout` command. The eval timeout is set to
`max(triage_timeout, 300)` seconds (5 minutes) — see `src/commands/evaluate.rs:338`.

When the native API client isn't configured (no `provider` set), the call falls back
to `call_claude_cli()` which shells out to `timeout {secs}s claude --print ...`. If
the Claude CLI takes longer than 300s (e.g., large prompt, slow model, network
issues), `timeout` sends SIGALRM (signal 14, exit 124).

**Fix**: Either:
1. Increase `triage_timeout` in config (e.g., 600s)
2. Configure a native API provider (`provider: "anthropic"`) to avoid CLI overhead
3. The native API path has its own timeout via `reqwest::Client` but handles it more
   gracefully

#### Failure Mode 2: LLM Returns Non-JSON

**Task**: `.evaluate-.verify-fix-executor-verify`
**Error**: `Failed to extract valid JSON from evaluator output after 3 attempts.
Last response: The cargo test command I ran earlier has completed successfully...`

**Root cause**: The evaluator model (claude-sonnet-4-latest) is returning
conversational text instead of the expected JSON evaluation object
`{"score": 0.85, "dimensions": {...}, "notes": "..."}`. This happens because:

1. The `claude --print` CLI mode sends the prompt as a user message
2. The `--output-format json` flag is used (`llm.rs:108`), but this only formats
   the CLI's metadata wrapper — it doesn't constrain the MODEL to output JSON
3. The model sees a long evaluation prompt and responds conversationally instead
   of with structured JSON

The `extract_json()` function (`evaluate.rs:1281`) tries to find `{...}` in the
response but fails when the response is pure prose.

**Fix**: 
1. Use native API with `response_format: {"type": "json_object"}` to force JSON mode
2. Or add a system prompt that more strongly instructs JSON-only output
3. The retry loop (3 attempts) doesn't change the prompt, so all 3 attempts fail
   the same way — consider adding "You MUST respond with only JSON" to retry prompts

---

## Evolution System Requirements

### What `wg evolve` Consumes

**File**: `src/commands/evolve/mod.rs` and `src/commands/evolve/prompt.rs`

The evolution system reads:
1. **Evaluations** (`agency/evaluations/*.yaml`): Each has:
   - `score: f64` (0.0-1.0 overall)
   - `dimensions: HashMap<String, f64>` (correctness, completeness, efficiency, style)
   - `role_id`, `tradeoff_id` (links eval to agent identity)
   - `task_id` (links eval to the completed task)
   - `notes` (qualitative feedback)

2. **Performance summaries** (`evolve/prompt.rs:15`): Aggregated per-role and
   per-tradeoff statistics (avg score, task count, dimensional breakdown).

3. **FLIP scores**: Stored in evaluations with `eval_source: "flip"`. The FLIP
   comparison produces a fidelity score indicating how well the agent's output
   matches the stated task intent.

### What Evolution Produces

Based on evaluation data, `wg evolve` runs strategies:
- **Mutation**: Modify underperforming roles/tradeoffs
- **Crossover**: Combine successful roles
- **Speciation**: Create new specialized roles
- **Retirement**: Remove consistently poor performers (< 0.35 avg, >= 5 evals)

### TB Integration Requirements for Evolution

For evolution to be meaningful with TB data:
1. **Evaluations must succeed**: The JSON extraction failures (see above) mean zero
   evaluations are recorded, giving evolution nothing to work with
2. **FLIP must succeed**: Same issue — FLIP failures produce no fidelity data
3. **Volume**: Evolution needs >= 5 evaluations per role/tradeoff to make statistically
   meaningful decisions
4. **Diversity**: Multiple agents with different roles should attempt the same tasks
   to enable comparative evolution

---

## Blockers Summary

| Blocker | Severity | Description | Fix |
|---------|----------|-------------|-----|
| Docker env access | **Critical** | wg agents run on host, TB tasks need Docker containers | Shell executor + docker bridge (Option A) |
| Eval JSON failures | **High** | Evaluator returns prose instead of JSON | Use native API with JSON mode; improve retry prompts |
| Eval timeouts | **High** | Claude CLI call hits 300s timeout | Configure native API provider; increase timeout |
| Harbor verifier integration | **Medium** | Need to capture harbor verifier results alongside FLIP | Post-trial comparison script; harbor API or file-based results |
| Config propagation | **Medium** | Per-trial config needs isolation | Separate .workgraph dirs per condition |
| Worktree isolation vs Docker | **Low** | Git worktrees don't help if work happens in Docker | Each trial task gets its own container; worktree is just for wg state |

---

## Appendix: File References

| System | Key Files |
|--------|-----------|
| Harbor adapter | `terminal-bench/wg/adapter.py` (all conditions A-F) |
| Eval scaffold | `src/commands/eval_scaffold.rs` (creates .assign/.flip/.evaluate) |
| Coordinator loop | `src/commands/service/coordinator.rs` (dispatch, phases 4.2-4.55) |
| Evaluate command | `src/commands/evaluate.rs` (LLM eval + FLIP) |
| LLM dispatch | `src/service/llm.rs` (lightweight calls, CLI fallback) |
| Config | `src/config.rs` (agency knobs, coordinator settings) |
| Done/verify | `src/commands/done.rs` (verify gates, PendingValidation) |
| Assignment | `src/commands/service/assignment.rs` (LLM-based agent selection) |
| Evolution | `src/commands/evolve/mod.rs`, `prompt.rs` (performance-based mutation) |
| Trial logging | `terminal-bench/wg/tb_logging.py` (structured NDJSON logging) |
