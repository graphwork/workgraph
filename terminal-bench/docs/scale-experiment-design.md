# Design: Full-Scale Parallel Experiment Architecture

**Date:** 2026-04-07
**Status:** Design (pre-implementation)
**Depends on:** [inventory.md](inventory.md), [pilot-results-synthesis.md](pilot-results-synthesis.md)
**Consumed by:** `tb-scale-runner-impl` (implementation task)

---

## 1. Experiment Goals

Run a **matched-set comparison** of Conditions A and F across the full Terminal-Bench task suite on a dedicated always-on server, producing statistically adequate data for a treatment-effect claim.

### Primary hypothesis
Workgraph context injection (Condition F) improves pass rates on coding tasks vs. bare agents (Condition A).

### What the pilots proved and what remains open

| Question | Pilot answer | What the full run must resolve |
|----------|-------------|-------------------------------|
| Does F outperform A on matched tasks? | Yes (4/8 tasks, 50 pp gap) | Replicate across all 89 TB 2.0 tasks |
| Is surveillance valuable? | No (0 activations in 95 trials) | Resolved: surveillance removed from F (see surveillance-audit.md) |
| What is F's token overhead? | 3.5x per trial, but 3.6x more cost-effective per pass on matched tasks | Measure at full scale with varied difficulty |
| Is the effect robust across difficulty levels? | Unknown (only 8-task overlap) | Measure per-difficulty treatment effect |

---

## 2. Conditions

### Primary conditions (must run)

| Condition | Label | Context scope | WG tools | Surveillance | Description |
|-----------|-------|--------------|----------|-------------|-------------|
| **A** | Bare agent | `clean` | No | No | Baseline: task description + verify command only |
| **F** | Full wg-native | `graph` | Yes | No | Full treatment: graph context + WG Quick Guide + wg CLI |

**Note:** Surveillance loops were removed from Condition F after the pilot showed 0 activations across 95 trials. All benefit came from context injection alone. Condition F is now purely about wg context injection. See `terminal-bench/docs/surveillance-audit.md` for the full analysis.

### Optional condition (historical)

| Condition | Label | Context scope | WG tools | Surveillance | Description |
|-----------|-------|--------------|----------|-------------|-------------|
| **G** | Context-only | `graph` | Yes | **No** | Originally an ablation condition; now identical to F. Kept for label compatibility. |

**Note:** With surveillance removed from F, Condition G is identical to F. The "which component drives the improvement" question from the pilot synthesis is resolved: it's context injection, not surveillance (0 activations in 95 pilot trials).

---

## 3. Task Set

### TB 2.0: 89 tasks (canonical set)

Both A and F run the same 89 tasks from `terminal-bench@2.0`. This eliminates the task-set mismatch that invalidated the pilot aggregate comparison.

**Execution path:** Host-native (`wg service start` + `wg native-exec`), not Harbor/Docker. Rationale:
- All `run_*.py` scripts use this path and it's proven at pilot scale
- No Docker Hub rate limits (no container pulls needed)
- Custom tasks use host-native verify commands; TB 2.0 tasks need adaptation (see Section 3.1)
- Simpler isolation model (temp dirs vs. Docker containers)

### 3.1 TB 2.0 task adaptation

The 89 TB 2.0 tasks are defined in Harbor's package format (`task.toml` + Docker images). For host-native execution, each task needs:
1. **Instructions extracted** from `task.toml` prompt field
2. **Verify commands adapted** from Harbor's container-based verifier to host-side shell commands
3. **Dependencies installed** on the host (per-task system packages, Python libs, etc.)

Two approaches:

| Approach | Effort | Fidelity | Risk |
|----------|--------|----------|------|
| **A: Harbor path** (`reproduce.sh`) | Low — already works | High — uses original Docker containers | Docker Hub rate limits; LiteLLM in the path (not native executor) |
| **B: Host-native adaptation** | High — 89 tasks to adapt | Medium — verify commands may differ | Some tasks may not run without Docker (e.g., `install-windows-3.11` needs QEMU in Docker) |
| **C: Hybrid** — Harbor for container execution, native executor for LLM calls | Medium | High | Needs adapter changes |

**Recommendation:** Use **Harbor path (approach A)** for the 89 TB 2.0 tasks. The `reproduce.sh` infrastructure already handles all 89 tasks with Docker containers and Harbor's built-in verifier. The key adaptation is injecting Condition F's context/tools into the Harbor agent class.

The existing `wg/adapter.py` already supports conditions A-F via `ConditionAAgent` through `ConditionFAgent` classes. Condition F needs the `graph` context scope and WG Quick Guide injected into the Harbor agent's system prompt.

For the **18 custom tasks** (8 calibration + 10 hard benchmarks), continue using host-native execution as in the pilots — these already have proven verify commands.

### 3.2 Task execution timeline

Run tasks in **randomized order** within each condition to prevent systematic position bias (as recommended by the pilot synthesis after the DNS outage exposed this vulnerability).

```python
import random
trial_order = [(task, replica) for task in tasks for replica in range(replicas)]
random.shuffle(trial_order)
```

---

## 4. Replica Count and Statistical Power

### Power analysis

From the pilot synthesis (Section 6):
- To detect a **30 pp treatment effect** (A=50%, F=80%) with 80% power at alpha=0.05:
  - Per-task: ~23 replicas per condition
  - Aggregate (same tasks): ~30 tasks with 3-5 replicas each

### Recommendation: 5 replicas per task per condition

| Replicas | Total trials (A+F, 89 tasks) | Per-task CI width (Wilson) | Aggregate power | Cost multiplier |
|----------|------------------------------|---------------------------|-----------------|-----------------|
| 1 | 178 | N/A (point estimate) | Moderate (n=89) | 1x |
| 3 | 534 | ~40 pp | Good (n=267) | 3x |
| **5** | **890** | **~30 pp** | **Strong (n=445)** | **5x** |
| 10 | 1,780 | ~20 pp | Very strong (n=890) | 10x |

**5 replicas** provides:
- Per-task: A task passing 5/5 has Wilson CI [56.6%, 100%]. Not tight, but sufficient to identify tasks where one condition dominates (0/5 vs 5/5 is p < 0.01, Fisher exact).
- Aggregate: 445 trials per condition gives a 95% CI width of ~±4 pp. Very precise for overall pass-rate comparison.
- Matches the pilot-F-89 design (5 replicas), enabling direct comparison.
- Budget: ~$50 at M2.7's current ($0) pricing, measured in tokens.

---

## 5. Parallelism Strategy

### Constraints

| Constraint | Source | Limit |
|-----------|--------|-------|
| OpenRouter rate limits (M2.7) | Empirical from pilots | Not hit at 4 concurrent trials, untested beyond |
| Docker Hub image pulls | 100/6h anonymous, 200/6h authenticated | Pre-pull all images; GHCR mirror unlimited |
| Host CPU/memory | Per-trial: 1-4 cores, 2-8 GB RAM | Server-dependent |
| `/tmp` space | ~10 GB per active trial | Server-dependent |
| API concurrency | OpenRouter server-side | Unknown exact limit |

### Recommended: 8 concurrent trials (ramp-up strategy)

```
Phase 1 (first 20 trials):  4 concurrent  — validate infrastructure, confirm rate limits
Phase 2 (remaining trials):  8 concurrent  — full throughput
Fallback:                     4 concurrent  — if rate-limit errors appear in Phase 2
```

**Why 8, not 16:**
- The pilot used 4 concurrent with no issues. Doubling to 8 is conservative.
- Each trial spawns 1 agent (Condition A/F both use single-agent mode for TB tasks). So 8 concurrent trials = 8 concurrent API calls.
- If we add Condition G or increase to 16, we risk hitting undocumented OpenRouter limits.
- 8 concurrent is sufficient to complete 890 trials in a reasonable time (see Section 8).

**Agents per trial:** 1 for all conditions (A, F, G). The 8-agent condition from `run_condition_a.py` is a different experiment (multi-agent coordination). For A vs F comparison, both conditions use 1 agent per task to isolate the context-injection variable.

### Ramp-up implementation

```python
class AdaptiveSemaphore:
    """Start conservative, increase after confirmed stability."""
    
    def __init__(self, initial=4, target=8, ramp_after=20):
        self._sem = asyncio.Semaphore(initial)
        self._target = target
        self._ramp_after = ramp_after
        self._completed = 0
        self._errors = 0
    
    async def acquire(self):
        await self._sem.acquire()
    
    def release(self, success: bool):
        self._completed += 1
        if not success:
            self._errors += 1
        self._sem.release()
        # Ramp up after initial phase if error rate is low
        if (self._completed == self._ramp_after 
            and self._errors / self._completed < 0.1
            and self._sem._value < self._target):
            # Increase semaphore capacity
            for _ in range(self._target - 4):
                self._sem.release()
```

---

## 6. Runner Architecture

### Approach: Extend existing infrastructure

Build `run_scale_experiment.py` by composing proven components from existing runners. Do NOT rewrite from scratch.

### Component reuse

| Component | Source | Modifications needed |
|-----------|--------|---------------------|
| Trial isolation (temp dir + wg init + config) | `run_condition_a.py` | None — reuse as-is |
| F condition setup (work task with graph context) | `run_pilot_f_89.py` | Extract as reusable function; surveillance removed |
| Harbor integration (89 TB 2.0 tasks) | `reproduce.sh` + `wg/adapter.py` | Add condition F agent class to Harbor path |
| Metrics collection (`stream.jsonl` parsing) | `run_condition_a.py` | None — reuse as-is |
| 3-layer verification | `run_condition_a.py` | None — reuse as-is |
| Retry logic | `rerun_pilot_f_89_dns.py` | Generalize to any failure |
| Report generation | `run_condition_a.py` | Extend for multi-condition comparison |

### Architecture diagram

```
run_scale_experiment.py
├── Config (CLI args, trial manifest)
│   ├── --conditions A,F [,G]
│   ├── --replicas 5
│   ├── --max-concurrent 8
│   ├── --task-set tb2  (or custom, or both)
│   ├── --model openrouter:minimax/minimax-m2.7
│   └── --resume results/scale-run-001/  (crash recovery)
│
├── Trial Manifest Generator
│   ├── Enumerate: tasks × conditions × replicas
│   ├── Randomize order
│   └── Write manifest.json (crash-safe checkpoint)
│
├── Execution Engine (asyncio + adaptive semaphore)
│   ├── For each trial in manifest:
│   │   ├── Acquire semaphore
│   │   ├── Dispatch to condition-specific runner
│   │   │   ├── Condition A: bare agent (clean context)
│   │   │   └── Condition F/G: wg-native (graph context + WG Quick Guide)
│   │   ├── Poll for completion (with timeout)
│   │   ├── Collect metrics + verification
│   │   ├── Write per-trial result to disk (crash-safe)
│   │   ├── Release semaphore
│   │   └── Network health check (on failure)
│   └── Retry queue (failed trials, up to 2 retries)
│
├── Progress Reporter
│   ├── Live: stdout progress bar (trials completed / total, ETA)
│   ├── Periodic: incremental.json (every trial completion)
│   └── Final: summary.json + comparison.md
│
└── Results Collector
    ├── Per-trial JSON + workgraph state archive
    ├── Aggregate statistics (pass rate, tokens, time by condition/difficulty/task)
    ├── Comparison report (A vs F, optionally vs G)
    └── Raw data export for external analysis
```

### 6.1 Trial isolation

Each trial gets:
- **Own temp directory:** `tempfile.mkdtemp(prefix=f"tb-{condition}-{task}-r{replica}-")`
- **Own `.workgraph/`:** Independent graph, config, service socket, agent state
- **Own `/tmp` namespace:** Clean up task-specific paths before each trial (via `cleanup_tmp_paths()`)
- **Stripped environment:** No `WG_*` or `CLAUDECODE` env vars leak from parent

**Critical for `/tmp` isolation:** Custom tasks write to shared `/tmp` paths (e.g., `/tmp/project`, `/tmp/kvstore.py`). Two trials of the same task CANNOT run concurrently because they'd clobber each other's `/tmp` state. The manifest generator must ensure no two trials of the same custom task run simultaneously.

For TB 2.0 tasks (Harbor path), Docker containers provide automatic `/tmp` isolation.

```python
# Mutex per task_id for custom tasks
task_locks = defaultdict(asyncio.Lock)

async def run_trial_isolated(trial):
    if trial.task_set == "custom":
        async with task_locks[trial.task_id]:
            return await run_trial(trial)
    else:
        # Harbor/Docker tasks have container isolation
        return await run_trial(trial)
```

### 6.2 Failure handling and retry

Three categories of failure:

| Category | Detection | Action |
|----------|----------|--------|
| **Operational** (DNS, rate limit, service crash) | Non-zero exit from wg, network error in stream.jsonl | Auto-retry (up to 2 retries) |
| **Model failure** (wrong answer, timeout) | `status == "failed"` or `status == "timeout"` | Record as failure, no retry |
| **Infrastructure** (disk full, OOM) | Exception in runner | Pause all trials, alert, manual intervention |

```python
async def run_with_retry(trial, max_retries=2):
    for attempt in range(max_retries + 1):
        result = await run_trial(trial)
        if result["status"] == "done":
            return result
        if is_operational_failure(result):
            # Network/rate-limit: retry after backoff
            await asyncio.sleep(30 * (2 ** attempt))
            continue
        # Model failure or infrastructure error: don't retry
        return result
    return result  # exhausted retries

def is_operational_failure(result):
    """Distinguish operational failures from model failures."""
    error = (result.get("error") or "").lower()
    return any(keyword in error for keyword in [
        "dns", "connection", "rate_limit", "429", "503", "timeout",
        "network", "socket", "ssl",
    ])
```

### 6.3 Crash recovery (resume)

The runner writes state to disk after every trial completion:

```python
# After each trial:
trial_result_path = f"{results_dir}/{trial_id}.json"
with open(trial_result_path, "w") as f:
    json.dump(result, f, indent=2)

# Manifest tracks completion status:
manifest["trials"][trial_id]["status"] = result["status"]
manifest["trials"][trial_id]["attempts"] = attempt + 1
with open(manifest_path, "w") as f:
    json.dump(manifest, f, indent=2)
```

On resume (`--resume`), the runner:
1. Loads the manifest
2. Identifies incomplete trials (not yet `done` or `failed` after max retries)
3. Resumes execution from where it left off
4. Merges results into the existing summary

### 6.4 Progress reporting

```
[  45/890] 5.1% | A:23 F:22 | Pass: 71.1% (A:56.5% F:86.4%) | ETA: 14h32m
             ├── Running: 8 concurrent | Queue: 845
             ├── Rate limit status: OK (0 retries in last 10 trials)
             └── Last completed: condA-build-pmars-r2 PASS (342s, 145K tokens)
```

### 6.5 Network health checks

After any operational failure, run a lightweight health check before retrying:

```python
async def check_api_health():
    """Quick probe to OpenRouter API."""
    try:
        proc = await asyncio.create_subprocess_exec(
            "curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
            "https://openrouter.ai/api/v1/models",
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        stdout, _ = await asyncio.wait_for(proc.communicate(), timeout=10)
        return stdout.decode().strip() == "200"
    except Exception:
        return False
```

If the health check fails, pause all new trial launches and retry the check every 30 seconds until it passes (with a 10-minute timeout before alerting).

---

## 7. Server Requirements

### Hardware

| Resource | Minimum | Recommended | Notes |
|----------|---------|-------------|-------|
| CPU | 8 cores | 16 cores | 8 concurrent trials × 1-2 cores each |
| RAM | 16 GB | 32 GB | 8 concurrent trials × 2-4 GB each |
| Disk | 100 GB | 200 GB | Docker images (~50 GB) + trial state + results |
| Network | 10 Mbps sustained | 100 Mbps | API calls are small; Docker pulls are the bottleneck |

### Operating System

Ubuntu 22.04 LTS or 24.04 LTS (amd64). The TB 2.0 Docker images target Linux amd64.

### Required Software

| Package | Version | Installation |
|---------|---------|-------------|
| Docker CE | 24.0+ | `apt install docker.io docker-compose-v2` |
| Python | 3.10+ | System Python or pyenv |
| Rust toolchain | stable | `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs \| sh` |
| `wg` binary | latest | `cargo install --path .` (from workgraph repo) |
| `harbor-bench` | >= 0.3.0 | `pip install harbor-bench` |
| `litellm` | latest | `pip install litellm` (Harbor path) |
| `httpx` | >= 0.24 | `pip install httpx` |
| Git | 2.30+ | `apt install git` |

### System packages for custom tasks

Some custom tasks require host-level packages:
- `python3-dev`, `python3-pip`, `python3-venv`
- `build-essential`, `gcc`, `g++`
- `cython3` (for build-cython-ext)
- `pytest` (via pip)
- `curl`, `jq` (for verify commands)

### Environment Variables

| Variable | Required | Purpose |
|----------|----------|---------|
| `OPENROUTER_API_KEY` | Yes | LLM API access |
| `DOCKER_BUILDKIT=1` | Recommended | Faster Docker builds |
| `PATH` | Must include | `~/.cargo/bin` (for `wg`) |

### Pre-flight checklist (automated)

The runner should verify all of these before starting:

```python
def preflight_checks():
    """Verify all prerequisites before launching experiment."""
    checks = [
        ("OPENROUTER_API_KEY set", bool(os.environ.get("OPENROUTER_API_KEY"))),
        ("wg binary found", shutil.which("wg") is not None),
        ("Docker running", subprocess.run(["docker", "info"], capture_output=True).returncode == 0),
        ("Python 3.10+", sys.version_info >= (3, 10)),
        ("harbor installed", importlib.util.find_spec("harbor") is not None),
        ("Disk space > 50GB", shutil.disk_usage("/").free > 50 * 1024**3),
        ("Docker images cached", check_docker_images_cached()),
        ("API reachable", asyncio.run(check_api_health())),
    ]
    for name, ok in checks:
        status = "OK" if ok else "FAIL"
        print(f"  [{status}] {name}")
    if not all(ok for _, ok in checks):
        sys.exit("Pre-flight checks failed. Fix the issues above and retry.")
```

---

## 8. Cost and Runtime Estimates

### Per-trial costs (from pilots)

| Metric | Condition A | Condition F | Source |
|--------|-------------|-------------|--------|
| Tokens/trial | ~204K | ~710K | pilot-a-89, pilot-f-89 |
| Time/trial (mean) | ~240s (4 min) | ~304s (5 min) | pilot-a-89, pilot-f-89 |
| Time/trial (max) | ~1800s (30 min) | ~1805s (30 min) | timeout cap |
| Dollar cost/trial | $0.00 | $0.00 | M2.7 pricing ($0) |

### Full-scale projections (89 tasks × 5 replicas × 2 conditions = 890 trials)

| Metric | Estimate | Calculation |
|--------|----------|-------------|
| **Total tokens** | ~407M | (89×5×204K) + (89×5×710K) |
| **Total time (sequential)** | ~67 hours | 890 × 270s mean |
| **Total time (8 concurrent)** | ~8.4 hours | 67h / 8 |
| **Total time (4 concurrent)** | ~16.8 hours | 67h / 4 |
| **Dollar cost (M2.7)** | ~$0 | M2.7 charges nothing on OpenRouter |
| **Dollar cost (if priced at Sonnet rates, $3/$15 per M tok)** | ~$7.3K | For budget planning if model changes |

### With Condition G (optional, identical to F — for historical comparison only)

| Metric | Additional cost |
|--------|----------------|
| Tokens | ~316M (identical to F — no surveillance overhead in either) |
| Time (8 concurrent) | ~4.2 hours |
| Dollar cost (M2.7) | ~$0 |

### Server cost

| Provider | Instance | Cost/hour | Cost for 24h run |
|----------|---------|-----------|-------------------|
| Hetzner AX41 | AMD Ryzen 5, 64GB, 1TB NVMe | ~$0.06/h (dedicated) | ~$50/month |
| AWS c5.4xlarge | 16 vCPU, 32GB | ~$0.68/h | ~$16 |
| GCP n2-standard-16 | 16 vCPU, 64GB | ~$0.76/h | ~$18 |
| Existing server | — | $0 | $0 |

**Recommendation:** Use an existing always-on server if available. Otherwise, a $50/month Hetzner dedicated server is the most cost-effective option for repeated runs.

---

## 9. Results Structure

```
terminal-bench/results/scale-run-{NNN}/
├── manifest.json                    # Trial manifest with randomized order + completion status
├── config.json                      # Run configuration (conditions, replicas, model, etc.)
├── summary.json                     # Aggregate results
├── comparison.md                    # A vs F (vs G) comparison report
│
├── condition-A/
│   ├── summary.json                 # Condition A aggregate
│   ├── {task-id}-r{N}.json         # Per-trial result
│   └── {task-id}-r{N}/
│       └── workgraph_state/         # Preserved .workgraph for post-hoc analysis
│
├── condition-F/
│   ├── summary.json
│   ├── {task-id}-r{N}.json
│   └── {task-id}-r{N}/
│       └── workgraph_state/
│
└── analysis/
    ├── per-task-comparison.csv      # Task × condition pass rates
    ├── per-difficulty.csv           # Difficulty × condition aggregates
    ├── token-usage.csv              # Token usage per trial
    └── statistical-tests.json       # Fisher exact, Wilson CIs per task
```

---

## 10. Analysis Plan

### Automated analysis (generated by runner)

1. **Per-task Fisher exact test:** For each task, 2×2 table (pass/fail × A/F), compute p-value. Identify tasks with significant treatment effects.
2. **Aggregate pass rates with Wilson CIs:** Overall, per-difficulty, per-category.
3. **Token efficiency:** Tokens per pass (total tokens / passes) by condition.
4. **Context overhead:** Token cost of wg context injection (F tokens vs A tokens per trial).
5. **Time distribution:** Per-condition, per-difficulty histograms.
6. **Failure taxonomy:** Categorize failures as model-capability vs. operational.

### Questions the full run answers

| Question | Analysis |
|----------|----------|
| Does F improve pass rates overall? | Aggregate pass rate comparison, Fisher exact on pooled data |
| On which tasks does F help most? | Per-task Fisher exact, rank by effect size |
| Does F help on hard tasks more than easy ones? | Stratified analysis by difficulty |
| What is the per-trial token overhead of context injection? | F tokens/trial vs A tokens/trial |
| What is F's cost per additional pass? | (F_tokens - A_tokens) / (F_passes - A_passes) |
| Does F ever hurt? | Count tasks where A outperforms F |

---

## 11. Implementation Phases

### Phase 1: Runner implementation (`tb-scale-runner-impl`)
1. Extract reusable components from `run_condition_a.py` and `run_pilot_f_89.py`
2. Build manifest generator with randomization
3. Implement adaptive semaphore and retry logic
4. Add crash recovery (resume from manifest)
5. Add progress reporting
6. Smoke test: 3 tasks × 1 replica × 2 conditions = 6 trials

### Phase 2: TB 2.0 task adaptation
1. For Harbor path: verify `ConditionFAgent` injects graph context correctly
2. For host-native path: extract instructions and verify commands from 89 `task.toml` files
3. Run 5-task smoke test through both paths

### Phase 3: Server setup
1. Provision server (or use existing)
2. Install prerequisites (Docker, Rust, Python, Harbor)
3. Pre-pull all Docker images (`pre-pull-images.sh`)
4. Run preflight checks
5. Smoke test: 5 tasks × 2 replicas × 2 conditions = 20 trials

### Phase 4: Full run
1. Start with Phase 1 (4 concurrent, first 20 trials)
2. Ramp to Phase 2 (8 concurrent)
3. Monitor progress (~8.4 hours expected)
4. Run analysis pipeline
5. Generate comparison report

### Phase 5: Optional Condition G (historical)
Note: With surveillance removed from F, Condition G is now identical to F.
Running G is unnecessary unless comparing against historical pilot data that used the G label.

---

## 12. Risk Mitigation

| Risk | Likelihood | Impact | Mitigation |
|------|-----------|--------|------------|
| OpenRouter rate limits at 8 concurrent | Medium | Trials fail, need retry | Adaptive semaphore with ramp-up; automatic retry with backoff |
| DNS/network outage | Low (but happened in pilots) | Batch failures | Health check + pause; randomized order prevents systematic bias |
| Server disk fills up | Low | Run halts | Pre-check disk space; clean up temp dirs after each trial |
| Docker image pull failures | Low | Some tasks can't run | Pre-pull all images; use GHCR mirror |
| M2.7 model unavailable on OpenRouter | Low | Entire run blocked | Check model availability in preflight; have fallback model config |
| `/tmp` collision (custom tasks) | Medium | Wrong results | Per-task mutex for custom tasks; Docker isolation for TB 2.0 |
| Server reboot/crash mid-run | Low | Lost progress | Crash recovery from manifest; systemd service or tmux |

---

## 13. Open Decisions for Implementer

1. **Harbor path vs. host-native for TB 2.0 tasks?** This design recommends Harbor, but the implementer should validate that `ConditionFAgent` can inject wg context into the Harbor agent loop. If not, host-native adaptation of all 89 tasks is needed (significant effort).

2. **Should the 18 custom tasks be included?** They provide direct pilot-to-scale comparison but add implementation complexity (host-native path + `/tmp` mutex). Recommendation: include them as a separate task set that runs after the TB 2.0 set.

3. **Should we use Docker-in-Docker for `/tmp` isolation of custom tasks?** Wrapping custom tasks in Docker would eliminate the `/tmp` collision problem but adds complexity. The per-task mutex is simpler and sufficient.

4. **Notification on completion?** For an 8-hour unattended run, the runner should notify on completion/failure. Options: email, Slack webhook, or Matrix (all supported by wg's notification system).
