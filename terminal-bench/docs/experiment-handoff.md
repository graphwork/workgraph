# Terminal-Bench A vs F Experiment: Full Handoff Document

**Date:** 2026-04-07
**Status:** Ready to run
**Author:** Synthesized from pilot data, design docs, and runner implementation

---

## 1. Executive Summary

### What we're testing

Does **workgraph context injection** (Condition F) improve coding task pass rates compared to a **bare agent** (Condition A)?

- **Condition A (Baseline):** An LLM agent receives only the task description and a verify command. No graph context, no `wg` tools, no surveillance loop.
- **Condition F (Full wg-native):** The same LLM agent receives graph-scoped context, access to `wg` CLI tools, a WG Quick Guide, and a surveillance loop (max 3 iterations) that can retry failed work.

Both conditions use the same model (`minimax/minimax-m2.7` via OpenRouter) and the same verification commands. The only variable is the scaffolding.

### Why it matters

If infrastructure-level context injection can turn a commodity model's failures into passes, it's a cheaper and more composable intervention than model upgrades. The pilot data suggests this is the case, but the evidence is incomplete.

### What the pilot showed

Pilots ran at two scales (5-task and 89-task) with partially overlapping task sets:

| Scale | Condition A | Condition F | Delta |
|-------|-------------|-------------|-------|
| 5-task (identical tasks) | 5/5 (100%) | 5/5 (100%) | 0 pp |
| 89-task (aggregate, different task sets) | 37/89 (41.6%) | 89/90 (98.9%) | +57.3 pp |
| 89-task (matched 8 tasks) | 4/8 (50.0%) | 40/40 (100%) | +50.0 pp |

**Key findings:**
- On the 8 matched tasks, F passed all 20 trials (5 replicas x 4 tasks) where A failed. F never caused a failure that A passes — strictly additive.
- F is 3.6x more cost-effective per pass on matched tasks (fewer wasted tokens on failures).
- The surveillance loop activated 0 times across 95 trials — all benefit came from context injection alone.
- The aggregate +57.3 pp gap is **not valid** as a treatment estimate because A ran 89 TB 2.0 tasks while F ran 18 custom tasks. Only the 8-task overlap is a fair comparison.

**What remains open:** The 8-task overlap is too small for confident conclusions. The full-scale experiment runs both conditions on the same 18 tasks with 5 replicas each (180 total trials) to produce a proper matched comparison.

See `terminal-bench/docs/pilot-results-synthesis.md` for the complete pilot analysis.

---

## 2. Experiment Design

### Conditions

| Condition | Context scope | WG tools | Surveillance | Description |
|-----------|--------------|----------|-------------|-------------|
| **A** | `clean` | No | No | Bare agent: task description + verify command only |
| **F** | `graph` | Yes | Yes (max 3 iterations, 1m delay) | Full wg-native: graph context + WG Quick Guide + surveillance loop |
| **G** (optional) | `graph` | Yes | **No** | Ablation: context-only, no surveillance overhead |

Run A and F first. Add G in a second pass if budget allows — it isolates context injection from surveillance.

### Task set

**18 custom tasks** (8 calibration + 10 hard benchmarks), all executed via host-native path (`wg service start` + `wg native-exec`):

| Difficulty | Count | Tasks |
|-----------|-------|-------|
| Easy | 2 | file-ops, text-processing |
| Medium | 3 | debugging, shell-scripting, data-processing |
| Hard | 13 | algorithm, ml, sysadmin, configure-git-webserver, mailman, multi-source-data-merger, financial-document-processor, cobol-modernization, build-cython-ext, fix-code-vulnerability, constraints-scheduling, multi-module-type-migration, iterative-test-fix |

These are project-authored tasks located in `tasks/condition-a-calibration/` (8 tasks) and `tasks/hard-benchmarks/` (10 tasks). They have proven verify commands tested across the pilots.

### Replica count

**5 replicas per task per condition.** This gives:
- 18 tasks x 5 replicas x 2 conditions = **180 total trials**
- Per-task: A task passing 5/5 has Wilson CI [56.6%, 100%]
- Aggregate: 90 trials per condition gives ~±10 pp CI width — adequate for detecting the 30+ pp effects observed in pilots
- Matches the pilot-F-89 design, enabling direct comparison

### Statistical approach

- **Per-task:** Fisher exact test on 2x2 table (pass/fail x A/F) for each task
- **Aggregate:** Overall pass-rate comparison with Wilson confidence intervals
- **Stratified:** Per-difficulty treatment effects
- **Cost analysis:** Tokens per pass by condition
- **Surveillance:** Activation rate and value measurement

### Trial ordering

Trials are **randomized** across conditions and tasks to prevent systematic position bias (the pilot DNS outage showed this matters). Set `--seed` for reproducibility.

---

## 3. Pilot Results Summary

### Pass rates (all from `pilot-results-synthesis.md`)

| Comparison | Cond A | Cond F | Notes |
|-----------|--------|--------|-------|
| 5-task (identical) | 5/5 (100%) | 5/5 (100%) | Ceiling effect — tasks too easy |
| 89-task aggregate | 37/89 (41.6%) | 89/90 (98.9%) | **Invalid comparison** — different task sets |
| 89-task matched (8 tasks) | 4/8 (50.0%) | 40/40 (100%) | Best available evidence |

### Matched task detail (8 overlapping tasks)

| Task | A | F (5 replicas) | Verdict |
|------|---|-----------------|---------|
| configure-git-webserver | FAIL | 5/5 PASS (149s) | F wins |
| constraints-scheduling | FAIL | 5/5 PASS (221s) | F wins |
| financial-document-processor | FAIL | 5/5 PASS (674s) | F wins |
| fix-code-vulnerability | FAIL | 5/5 PASS (167s) | F wins |
| build-cython-ext | PASS (249s) | 5/5 PASS (124s) | Both pass, F 2x faster |
| cobol-modernization | PASS (415s) | 5/5 PASS (827s) | Both pass, A faster |
| mailman | PASS (246s) | 5/5 PASS (400s) | Both pass, A faster |
| multi-source-data-merger | PASS (70s) | 5/5 PASS (243s) | Both pass, A faster |

### Token usage

| Scale | A tokens/trial | F tokens/trial | F/A ratio |
|-------|---------------|---------------|-----------|
| 5-task | 19,558 | 100,663 | 5.1x |
| 89-task | 203,953 | 709,753 | 3.5x |

On matched tasks, F is **3.6x more cost-effective per pass** despite higher per-trial token usage.

### Surveillance value

**Zero activations across 95 trials.** The surveillance loop was never triggered. All benefit came from context injection alone. This motivates testing Condition G (context without surveillance) in a follow-up.

---

## 4. Infrastructure Setup

### Server requirements

| Resource | Minimum | Recommended |
|----------|---------|-------------|
| CPU | 8 cores | 16 cores |
| RAM | 16 GB | 32 GB |
| Disk | 50 GB free | 100 GB free |
| Network | Stable outbound HTTPS | 10+ Mbps |
| OS | Ubuntu 22.04 LTS (amd64) | Ubuntu 24.04 LTS (amd64) |

The experiment is I/O-bound (API calls), not compute-bound. Each concurrent trial needs ~1 CPU core and 2-4 GB RAM.

### Software prerequisites

Install these in order:

```bash
# 1. System packages
sudo apt update && sudo apt install -y \
  build-essential gcc g++ git curl jq \
  python3 python3-dev python3-pip python3-venv \
  cython3

# 2. Rust toolchain
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"

# 3. Clone the workgraph repo
git clone https://github.com/anthropics/workgraph.git
cd workgraph

# 4. Build and install the wg binary
cargo install --path .

# 5. Verify wg is available
wg --version

# 6. Python dependencies (from terminal-bench/)
pip install pytest httpx
```

> **Note:** Docker is NOT required for this experiment. The 18 custom tasks run host-native (directly on the server in temp directories), not in Docker containers. Docker is only needed for the 89-task TB 2.0 Harbor path, which is a separate experiment.

### API keys and environment variables

```bash
# Required: OpenRouter API key for LLM access
export OPENROUTER_API_KEY="sk-or-v1-your-key-here"

# Ensure cargo binaries are on PATH
export PATH="$HOME/.cargo/bin:$PATH"
```

Add these to `~/.bashrc` or `~/.zshrc` for persistence:

```bash
echo 'export OPENROUTER_API_KEY="sk-or-v1-your-key-here"' >> ~/.bashrc
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

### Repository setup

```bash
# Clone (if not already done above)
git clone https://github.com/anthropics/workgraph.git
cd workgraph

# Build the wg binary
cargo build --release
cargo install --path .

# Verify the build
cargo test  # Should pass 5900+ tests

# Check the terminal-bench task files exist
ls terminal-bench/tasks/condition-a-calibration/  # 8 task instruction files
ls terminal-bench/tasks/hard-benchmarks/          # 10 task instruction files
```

### Pre-flight verification

The runner has built-in preflight checks. You can also verify manually:

```bash
# Check all prerequisites
echo "=== Pre-flight checks ==="
echo -n "OPENROUTER_API_KEY: "; [ -n "$OPENROUTER_API_KEY" ] && echo "OK" || echo "MISSING"
echo -n "wg binary: "; which wg && echo "OK" || echo "MISSING"
echo -n "Python 3.10+: "; python3 --version
echo -n "Disk space: "; df -h /tmp | tail -1
echo -n "API reachable: "; curl -s -o /dev/null -w "%{http_code}" https://openrouter.ai/api/v1/models
echo ""
```

---

## 5. Running the Experiment

### Quick start (smoke test first)

Always run a smoke test before the full experiment to validate the setup:

```bash
cd workgraph

# Smoke test: 3 tasks x 1 replica x 2 conditions = 6 trials (~5 minutes)
python3 terminal-bench/run_scale_experiment.py --smoke
```

Expected output: 6 trials complete, progress bar, final summary showing pass rates for A and F.

### Full experiment

```bash
cd workgraph

# Default: 18 tasks x 5 replicas x 2 conditions (A,F) = 180 trials
python3 terminal-bench/run_scale_experiment.py
```

### Configuring the run

```bash
# Custom conditions, replicas, concurrency
python3 terminal-bench/run_scale_experiment.py \
  --conditions A,F \
  --replicas 5 \
  --max-concurrent 8 \
  --initial-concurrent 4 \
  --ramp-after 20 \
  --model "openrouter:minimax/minimax-m2.7" \
  --timeout 1800 \
  --seed 42

# Run specific tasks only
python3 terminal-bench/run_scale_experiment.py \
  --tasks file-ops,debugging,algorithm \
  --replicas 3

# Add Condition G (context-only, no surveillance)
python3 terminal-bench/run_scale_experiment.py \
  --conditions A,F,G \
  --replicas 5

# Custom results directory
python3 terminal-bench/run_scale_experiment.py \
  --results-dir terminal-bench/results/my-run-001

# Skip preflight checks (if you've already verified)
python3 terminal-bench/run_scale_experiment.py --skip-preflight
```

### CLI reference

| Flag | Default | Description |
|------|---------|-------------|
| `--conditions` | `A,F` | Comma-separated: A, F, G |
| `--replicas` | `5` | Replicas per task per condition |
| `--tasks` | all 18 | Comma-separated task IDs to run |
| `--max-concurrent` | `8` | Max parallel trials (after ramp-up) |
| `--initial-concurrent` | `4` | Starting parallel trials |
| `--ramp-after` | `20` | Ramp up concurrency after N stable trials |
| `--timeout` | `1800` | Per-trial timeout in seconds (30 min) |
| `--max-retries` | `2` | Retries for operational failures (DNS, rate limits) |
| `--model` | `openrouter:minimax/minimax-m2.7` | LLM model |
| `--results-dir` | auto-generated | Output directory |
| `--resume` | — | Resume from a results directory |
| `--seed` | random | Random seed for trial ordering |
| `--smoke` | — | Smoke test mode (3 tasks x 1 replica x 2 conditions) |
| `--skip-preflight` | — | Skip preflight checks |

### Concurrency strategy

The runner uses an **adaptive semaphore**:
1. Starts at `--initial-concurrent` (default 4) parallel trials
2. After `--ramp-after` (default 20) trials with <10% error rate, ramps to `--max-concurrent` (default 8)
3. If errors are high, stays at the initial level

Each trial spawns 1 agent (Condition A) or 2 agents (Condition F: work + surveillance). With 8 concurrent trials in F, that's 16 concurrent API calls max.

### Monitoring progress

The runner prints live progress to stdout:

```
[  45/180] 25.0% | A:23/90 F:22/90 | Pass: 71.1% | ETA: 2.3h | condA-algorithm-r2 PASS (342s, 145,000 tok)
```

For long runs, use `tmux` or `screen` to keep the session alive:

```bash
# Start in tmux
tmux new -s experiment
python3 terminal-bench/run_scale_experiment.py
# Detach: Ctrl+b, d
# Reattach: tmux attach -t experiment
```

Or run in the background with logging:

```bash
nohup python3 terminal-bench/run_scale_experiment.py > experiment.log 2>&1 &
tail -f experiment.log  # Monitor
```

### Resuming after interruption

The runner saves state after every trial completion. To resume:

```bash
# Resume from an interrupted run
python3 terminal-bench/run_scale_experiment.py \
  --resume terminal-bench/results/scale-run-001/
```

This loads `manifest.json`, identifies incomplete trials, and continues from where it stopped. Already-completed trials are not re-run.

### Expected runtime and cost

| Metric | Estimate |
|--------|----------|
| Total trials | 180 (A: 90, F: 90) |
| Mean time per trial | ~270s (A: ~240s, F: ~304s) |
| Total sequential time | ~13.5 hours |
| Total at 8 concurrent | **~1.7 hours** |
| Total at 4 concurrent | **~3.4 hours** |
| Total tokens | ~82M (A: ~18M, F: ~64M) |
| Dollar cost (M2.7) | **~$0** (M2.7 is free on OpenRouter) |
| Dollar cost (if priced at Sonnet rates) | ~$1.5K |

---

## 6. Collecting Results

### Where results land

Results are written to an auto-generated directory under `terminal-bench/results/`:

```
terminal-bench/results/scale-run-001/
├── manifest.json                    # Trial manifest with completion status
├── config.json                      # Run configuration
├── summary.json                     # Aggregate results (main output)
├── comparison.md                    # A vs F markdown comparison report
│
├── condition-A/
│   └── summary.json                 # Condition A aggregate stats
│
├── condition-F/
│   └── summary.json                 # Condition F aggregate stats
│
├── condA-file-ops-r0.json          # Per-trial result files
├── condA-file-ops-r1.json
├── condF-file-ops-r0.json
├── ...
│
├── condA-file-ops-r0/
│   └── workgraph_state/             # Preserved .workgraph for post-hoc analysis
├── condF-file-ops-r0/
│   └── workgraph_state/
└── ...
```

### Key output files

| File | Contents |
|------|----------|
| `summary.json` | Full results: per-condition stats, per-difficulty stats, per-task stats, all trial records |
| `comparison.md` | Human-readable markdown report: overall table, per-difficulty, per-task |
| `manifest.json` | Trial manifest with randomized order and completion status (used for resume) |
| `config.json` | Run configuration for reproducibility |
| `cond{A,F}-{task}-r{N}.json` | Individual trial result: status, timing, token metrics, verify output |
| `condition-{A,F}/summary.json` | Per-condition aggregate statistics |

### Generating the comparison report

The runner generates `comparison.md` automatically at the end of the run. It includes:
- Overall pass rate comparison table
- Per-difficulty breakdown
- Per-task pass rates for each condition

If you need to regenerate it (e.g., after merging resumed runs), the data is in `summary.json`.

### Validating completeness

```bash
# Check all trials completed
python3 -c "
import json, sys
m = json.load(open('terminal-bench/results/scale-run-001/manifest.json'))
total = m['total_trials']
done = sum(1 for t in m['trials'].values() if t['status'] in ('done', 'failed_permanent'))
pending = total - done
print(f'Total: {total}, Completed: {done}, Pending: {pending}')
if pending > 0:
    for tid, t in m['trials'].items():
        if t['status'] not in ('done', 'failed_permanent'):
            print(f'  Incomplete: {tid} — status: {t[\"status\"]}')
    sys.exit(1)
print('All trials complete.')
"

# Check pass rates
python3 -c "
import json
s = json.load(open('terminal-bench/results/scale-run-001/summary.json'))
for cond, stats in s['condition_stats'].items():
    print(f'Condition {cond}: {stats[\"passed\"]}/{stats[\"total\"]} '
          f'({stats[\"pass_rate\"]:.1%}) — mean {stats[\"mean_time_s\"]:.0f}s')
"

# Verify result count matches expectations
python3 -c "
import json
s = json.load(open('terminal-bench/results/scale-run-001/summary.json'))
expected = s['replicas'] * len(s['conditions']) * s['unique_tasks']
actual = s['total_trials']
assert actual == expected, f'Expected {expected} trials, got {actual}'
print(f'OK: {actual} trials as expected ({s[\"unique_tasks\"]} tasks x {s[\"replicas\"]} replicas x {len(s[\"conditions\"])} conditions)')
"
```

---

## 7. Troubleshooting

### DNS failures

**Symptom:** Trials fail with "dns", "connection refused", or "network unreachable" errors.

**Cause:** Network outage or DNS resolution failure. This happened during the pilot (29/90 F trials failed due to a ~02:52 UTC DNS outage).

**Fix:**
- The runner automatically retries operational failures up to 2 times with exponential backoff (30s, 60s).
- If persistent, check `curl -s https://openrouter.ai/api/v1/models` — if it returns HTTP 200, the API is up.
- Resume after network recovery: `python3 run_scale_experiment.py --resume results/scale-run-001/`

### Rate limits (429 errors)

**Symptom:** Trials fail with "rate_limit" or HTTP 429 errors.

**Fix:**
- Reduce concurrency: `--max-concurrent 4 --initial-concurrent 2`
- The adaptive semaphore will stay at the lower concurrency if errors are frequent.
- Resume after rate limit clears: `--resume results/scale-run-001/`

### Model fallback detection

**Symptom:** Results seem inconsistent — some trials use a different model than expected.

**Check:** The runner records `model_used` in each trial's metrics (from the agent's `stream.jsonl`):

```bash
# Verify all trials used the expected model
python3 -c "
import json, os, glob
results_dir = 'terminal-bench/results/scale-run-001'
for f in sorted(glob.glob(os.path.join(results_dir, 'cond*.json'))):
    if os.path.isfile(f) and not os.path.isdir(f):
        r = json.load(open(f))
        model = (r.get('metrics') or {}).get('model_used', 'unknown')
        if 'm2.7' not in (model or '').lower() and 'minimax' not in (model or '').lower():
            print(f'WARNING: {r[\"trial_id\"]} used model: {model}')
"
```

### `/tmp` collision between trials

**Symptom:** Trials of the same task produce wrong results or overwrite each other.

**Cause:** Custom tasks write to shared `/tmp` paths (e.g., `/tmp/project`, `/tmp/kvstore.py`). Two trials of the same task running concurrently would clobber each other.

**Built-in mitigation:** The runner uses a per-task asyncio lock (`task_locks[task_id]`). Two trials of the same task never run simultaneously — they serialize automatically. No action needed unless you modify the runner.

### Service start failures

**Symptom:** "wg service start" fails or graph init errors.

**Fix:**
```bash
# Check wg is working
wg --version

# If wg was rebuilt, reinstall
cd workgraph && cargo install --path .

# Check no stale services are running
ps aux | grep "wg service"
```

### Disk space

**Symptom:** Trials fail with disk-full errors.

**Fix:**
```bash
# Check available space
df -h /tmp

# Clean up old trial temp directories
rm -rf /tmp/tb-cond*

# Clean up old results if needed
du -sh terminal-bench/results/*/
```

Each trial creates a temp directory (~10-50 MB) that is cleaned up after completion. With 8 concurrent trials, you need ~400 MB of `/tmp` space at peak.

### Trial timeout

**Symptom:** Trial shows status "timeout" after 1800s.

**Cause:** The model couldn't complete the task within the 30-minute timeout. This is a genuine model limitation, not infrastructure.

**Options:**
- Increase timeout for specific tasks: edit the `DEFAULT_TIMEOUT` in `run_scale_experiment.py` or use `--timeout 3600`
- Accept it as a failure — the pilot found that `iterative-test-fix` is the only task that consistently times out (~1800s, 137 turns)

### `wg` binary not found

**Fix:**
```bash
# Ensure Rust is installed
rustc --version

# Rebuild and install
cd workgraph
cargo install --path .

# Verify it's on PATH
which wg
# Should show: ~/.cargo/bin/wg

# If not on PATH
export PATH="$HOME/.cargo/bin:$PATH"
```

### Python import errors

**Fix:**
```bash
# Install required Python packages
pip install pytest httpx

# Verify
python3 -c "import pytest; print('pytest OK')"
python3 -c "import httpx; print('httpx OK')"
```

---

## Appendix A: Condition Details

### Condition A (Bare Agent)

The agent receives:
- Task title and description (from instruction file)
- `--verify` command (run automatically by wg after task completion)
- Context scope: `clean` (no graph context, no wg tools)
- Single agent, no surveillance

Config written per trial:
```toml
[coordinator]
max_agents = 1
executor = "native"
model = "openrouter:minimax/minimax-m2.7"
worktree_isolation = false
agent_timeout = "30m"
max_verify_failures = 0
max_spawn_failures = 0

[agent]
model = "openrouter:minimax/minimax-m2.7"
context_scope = "clean"
exec_mode = "full"

[agency]
auto_assign = false
auto_evaluate = false
```

### Condition F (Full wg-native)

The agent receives:
- Task title, description, and the WG Quick Guide
- Graph-scoped context (sees the dependency graph)
- Access to `wg` CLI tools (log, artifact, show, list, done, fail, add)
- A surveillance agent watches and can trigger retry (max 3 iterations)

Task graph per trial:
```
INIT (completed immediately) → WORK (main task) → SURVEILLANCE → WORK (cycle, max 3 iterations)
```

Config written per trial:
```toml
[coordinator]
max_agents = 2
executor = "native"
model = "openrouter:minimax/minimax-m2.7"
worktree_isolation = false

[agent]
model = "openrouter:minimax/minimax-m2.7"
context_scope = "graph"
exec_mode = "full"

[agency]
auto_assign = false
auto_evaluate = false
```

### Condition G (Context-only, optional)

Same as F but without the surveillance loop. Only 1 agent, no cycle. Isolates the value of context injection from surveillance overhead.

---

## Appendix B: Full Task List (18 tasks)

### Calibration Tasks (8)

| # | ID | Title | Difficulty |
|---|-----|-------|-----------|
| 1 | file-ops | File Operations: create project structure | Easy |
| 2 | text-processing | Text Processing: word frequency counter | Easy |
| 3 | debugging | Debugging: fix merge sort bugs | Medium |
| 4 | shell-scripting | Shell Scripting: log file analyzer | Medium |
| 5 | data-processing | Data Processing: JSON to CSV department summary | Medium |
| 6 | algorithm | Algorithm: key-value store with transactions | Hard |
| 7 | ml | ML: k-means clustering from scratch | Hard |
| 8 | sysadmin | Sysadmin: rate-limited HTTP server | Hard |

### Hard Benchmark Tasks (10)

| # | ID | Title | Difficulty |
|---|-----|-------|-----------|
| 1 | configure-git-webserver | Configure Git Webserver | Hard |
| 2 | mailman | Mailman: local mail system | Hard |
| 3 | multi-source-data-merger | Multi-Source Data Merger | Hard |
| 4 | financial-document-processor | Financial Document Processor | Hard |
| 5 | cobol-modernization | COBOL Modernization | Hard |
| 6 | build-cython-ext | Build Cython Extension | Hard |
| 7 | fix-code-vulnerability | Fix Code Vulnerabilities | Hard |
| 8 | constraints-scheduling | Constraints Scheduling | Hard |
| 9 | multi-module-type-migration | Multi-Module Type Migration | Hard |
| 10 | iterative-test-fix | Iterative Test Fix | Hard |

Task instruction files: `terminal-bench/tasks/condition-a-calibration/` and `terminal-bench/tasks/hard-benchmarks/`

---

## Appendix C: Source References

| Document | Path | Contents |
|----------|------|----------|
| Inventory | `terminal-bench/docs/inventory.md` | Task list, runner scripts, infrastructure deps, resource requirements |
| Pilot synthesis | `terminal-bench/docs/pilot-results-synthesis.md` | Complete pilot analysis with CIs, cost, surveillance findings |
| Design doc | `terminal-bench/docs/scale-experiment-design.md` | Experiment architecture, parallelism strategy, analysis plan |
| Runner script | `terminal-bench/run_scale_experiment.py` | The actual experiment runner (this is what you execute) |
| Pilot F-89 runner | `terminal-bench/run_pilot_f_89.py` | Pilot runner for reference |
| Reproduction script | `terminal-bench/reproduce.sh` | Harbor/Docker path for 89 TB 2.0 tasks (separate experiment) |
| Pilot A-89 results | `terminal-bench/results/pilot-a-89/summary.json` | 89 trials, 37 passed |
| Pilot F-89 results | `terminal-bench/results/pilot-f-89/summary.json` | 90 trials, 89 passed |
