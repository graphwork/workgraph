# Remote Host Setup for Terminal Bench Experiments

Step-by-step guide for setting up a Linux server to run Terminal Bench (TB)
experiments (Conditions A, G, and optionally F) unattended. The experiment
uses `minimax/minimax-m2.7` via OpenRouter, with tasks running host-native
(no Docker required for custom tasks).

> **Example host:** `bot@ulivo` — Debian/Ubuntu server, 241 GB free,
> repo already cloned at `~/workgraph`. Adjust the SSH target and paths
> for your own host.

---

## 1. Prerequisites Checklist

Verify these are available on the remote host before proceeding:

| Dependency | Purpose | Install section |
|------------|---------|-----------------|
| Git | Clone / sync the repo | [2.1](#21-system-packages) |
| Python 3.10+ | Experiment runner (`run_scale_experiment.py`) | [2.1](#21-system-packages) |
| Rust / Cargo | Build the `wg` binary | [2.2](#22-rust-toolchain) |
| `build-essential`, `gcc`, `g++` | Compile Rust + Cython tasks | [2.1](#21-system-packages) |
| `cython3` | Required by `build-cython-ext` task | [2.1](#21-system-packages) |
| `jq`, `curl` | Preflight checks and debugging | [2.1](#21-system-packages) |
| `tmux` or `screen` | Keep long runs alive after SSH disconnect | [2.1](#21-system-packages) |
| `pip` packages: `pytest`, `httpx` | Python test/HTTP dependencies | [2.5](#25-python-dependencies) |
| `OPENROUTER_API_KEY` | LLM access (M2.7 is free-tier) | [3](#3-environment-variables) |

**Not required for the custom-task experiment:** Docker, Node.js, Harbor.
Those are only needed for the 89-task TB 2.0 leaderboard submission path
(see `terminal-bench/reproduce.sh`).

---

## 2. Installation Commands

### 2.1 System packages

```bash
sudo apt update && sudo apt install -y \
  build-essential gcc g++ git curl jq \
  python3 python3-dev python3-pip python3-venv \
  cython3 tmux
```

### 2.2 Rust toolchain

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
rustc --version   # confirm installation
```

### 2.3 Repo sync

If the repo is already cloned:

```bash
cd ~/workgraph
git pull --ff-only
```

If cloning fresh:

```bash
git clone <your-repo-url> ~/workgraph
cd ~/workgraph
```

### 2.4 Build and install `wg`

```bash
cd ~/workgraph
cargo install --path .
wg --version
```

> **Why this matters (from CLAUDE.md):** The global `wg` command is installed
> via `cargo install`. After making changes to the code, you must re-run
> `cargo install --path .` to update the global binary. Forgetting this step
> is a common source of "why isn't this working" issues.

### 2.5 Python dependencies

```bash
pip install pytest httpx
python3 -c "import pytest; print('pytest OK')"
python3 -c "import httpx; print('httpx OK')"
```

---

## 3. Environment Variables

Set `OPENROUTER_API_KEY` and ensure Cargo binaries are on `PATH`.
**Do NOT commit actual keys to the repo.**

```bash
# Add to ~/.bashrc (or ~/.zshrc)
echo 'export OPENROUTER_API_KEY="sk-or-v1-YOUR-KEY-HERE"' >> ~/.bashrc
echo 'export PATH="$HOME/.cargo/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
```

Verify:

```bash
[ -n "$OPENROUTER_API_KEY" ] && echo "OPENROUTER_API_KEY: OK" || echo "MISSING"
which wg && echo "wg binary: OK" || echo "wg binary: MISSING"
```

---

## 4. Docker Images (Only If Running Harbor Path)

The 18-task custom experiment (Conditions A/G) runs **host-native** and does
not need Docker. Skip this section unless you need the 89-task TB 2.0
leaderboard path.

If you do need Docker:

```bash
# Install Docker
bash terminal-bench/setup-docker.sh

# Download TB task data
harbor download terminal-bench@2.0

# Pre-pull all images (~75 GB)
bash terminal-bench/pre-pull-images.sh
```

See `terminal-bench/pre-pull-images.sh --help` for rate-limit tips and
GHCR mirror support.

---

## 5. Smoke Test

Run a single quick test to verify everything works end-to-end:

```bash
cd ~/workgraph

python3 terminal-bench/run_scale_experiment.py --smoke
```

This runs **3 tasks x 1 replica x 2 conditions (A, G) = 6 trials** and takes
about 5 minutes. Expected output: a progress bar, then a summary table
showing pass rates for each condition.

If the smoke test fails, check:

1. `OPENROUTER_API_KEY` is set and valid:
   ```bash
   curl -s -o /dev/null -w "%{http_code}" https://openrouter.ai/api/v1/models
   # Should print 200
   ```
2. `wg` is on PATH: `which wg`
3. Python deps: `python3 -c "import pytest, httpx; print('OK')"`
4. Disk space: `df -h /tmp` (need ~400 MB free at peak)

---

## 6. Full Experiment Launch

### Default run (A vs G, 18 tasks, 5 replicas = 180 trials)

```bash
cd ~/workgraph

# Start in a tmux session so the run survives SSH disconnect
tmux new -s experiment

python3 terminal-bench/run_scale_experiment.py
```

### Custom configuration

```bash
# Conditions A and G with specific concurrency and seed
python3 terminal-bench/run_scale_experiment.py \
  --conditions A,G \
  --replicas 5 \
  --max-concurrent 8 \
  --initial-concurrent 4 \
  --ramp-after 20 \
  --model "openrouter:minimax/minimax-m2.7" \
  --timeout 1800 \
  --seed 42

# Include historical Condition F
python3 terminal-bench/run_scale_experiment.py \
  --conditions A,G,F \
  --replicas 5

# Run specific tasks only
python3 terminal-bench/run_scale_experiment.py \
  --tasks file-ops,debugging,algorithm \
  --replicas 3
```

### CLI reference

| Flag | Default | Description |
|------|---------|-------------|
| `--conditions` | `A,G` | Comma-separated: A, G, F |
| `--replicas` | `5` | Replicas per task per condition |
| `--tasks` | all 18 | Comma-separated task IDs |
| `--max-concurrent` | `8` | Max parallel trials (after ramp) |
| `--initial-concurrent` | `4` | Starting parallelism |
| `--ramp-after` | `20` | Ramp to max after N stable trials |
| `--timeout` | `1800` | Per-trial timeout in seconds |
| `--max-retries` | `2` | Retries for operational failures |
| `--model` | `openrouter:minimax/minimax-m2.7` | LLM model |
| `--results-dir` | auto-generated | Output directory |
| `--resume` | — | Resume from a results directory |
| `--seed` | random | Random seed for trial ordering |
| `--smoke` | — | Smoke test (3 tasks x 1 replica x 2 conditions) |
| `--skip-preflight` | — | Skip preflight checks |

### Expected runtime and cost

| Metric | Estimate |
|--------|----------|
| Total trials (default) | 180 (A: 90, G: 90) |
| Mean time per trial | ~270s |
| Wall clock at 8 concurrent | ~1.7 hours |
| Wall clock at 4 concurrent | ~3.4 hours |
| Dollar cost (M2.7 on OpenRouter) | $0 (free-tier model) |

---

## 7. Monitoring

### Live progress

The runner prints a progress line per trial:

```
[  45/180] 25.0% | A:23/90 G:22/90 | Pass: 71.1% | ETA: 2.3h | condA-algorithm-r2 PASS (342s)
```

### tmux session management

```bash
# Detach from tmux (keeps the run alive)
# Press: Ctrl+b, then d

# Reattach later
tmux attach -t experiment

# List sessions
tmux ls
```

### Background with log file

```bash
nohup python3 terminal-bench/run_scale_experiment.py > experiment.log 2>&1 &
tail -f experiment.log
```

### Resume after interruption

The runner saves state after every trial. To resume:

```bash
python3 terminal-bench/run_scale_experiment.py \
  --resume terminal-bench/results/<run-directory>/
```

Already-completed trials are skipped.

---

## 8. Preflight Check Script

Run this to verify the full environment before launching a long run:

```bash
echo "=== Pre-flight checks ==="
echo -n "OPENROUTER_API_KEY: "; [ -n "$OPENROUTER_API_KEY" ] && echo "OK" || echo "MISSING"
echo -n "wg binary: "; which wg && wg --version || echo "MISSING"
echo -n "Python: "; python3 --version
echo -n "pytest: "; python3 -c "import pytest; print('OK')" 2>/dev/null || echo "MISSING"
echo -n "httpx: "; python3 -c "import httpx; print('OK')" 2>/dev/null || echo "MISSING"
echo -n "Disk (/tmp): "; df -h /tmp | tail -1
echo -n "Disk (home): "; df -h ~ | tail -1
echo -n "API reachable: "; curl -s -o /dev/null -w "%{http_code}" https://openrouter.ai/api/v1/models
echo ""
echo -n "Task files (calibration): "; ls terminal-bench/tasks/condition-a-calibration/ | wc -l
echo -n "Task files (hard): "; ls terminal-bench/tasks/hard-benchmarks/ | wc -l
```

Expected: API key OK, `wg` available, Python 3.10+, both pip packages present,
8 calibration tasks, 10 hard benchmark tasks.

---

## 9. Results Structure

Results land in `terminal-bench/results/<auto-named-directory>/`:

```
terminal-bench/results/scale-run-001/
  manifest.json          # Trial manifest + completion status (for resume)
  config.json            # Run configuration (for reproducibility)
  summary.json           # Aggregate results (main output)
  comparison.md          # Human-readable A vs G comparison report
  condition-A/summary.json
  condition-G/summary.json
  condA-file-ops-r0.json           # Per-trial result file
  condA-file-ops-r0/workgraph_state/  # Preserved .workgraph (post-hoc analysis)
  ...
```

### Validate completeness

```bash
python3 -c "
import json, sys
m = json.load(open('terminal-bench/results/<run-dir>/manifest.json'))
total = m['total_trials']
done = sum(1 for t in m['trials'].values() if t['status'] in ('done', 'failed_permanent'))
print(f'Total: {total}, Completed: {done}, Pending: {total - done}')
"
```

### Check pass rates

```bash
python3 -c "
import json
s = json.load(open('terminal-bench/results/<run-dir>/summary.json'))
for cond, stats in s['condition_stats'].items():
    print(f'Condition {cond}: {stats[\"passed\"]}/{stats[\"total\"]} '
          f'({stats[\"pass_rate\"]:.1%})')
"
```

---

## 10. Master Experiment Plan

The full experiment design, hypothesis, task list, statistical approach, and
troubleshooting guide live in:

**`terminal-bench/docs/experiment-handoff.md`**

That document is the single source of truth for what we're testing and why.

### Related documentation

| Document | Path | Contents |
|----------|------|----------|
| Experiment handoff | `terminal-bench/docs/experiment-handoff.md` | Master plan, conditions, task list, troubleshooting |
| Leaderboard submission | `terminal-bench/docs/HOWTO-submit-to-leaderboard.md` | Submission format and rules |
| Remote vs batched research | `terminal-bench/docs/research-remote-host-vs-batched.md` | Batching rationale and cost analysis |
| Turn budget research | `terminal-bench/docs/research-tb-agent-turn-budget.md` | Agent timeout and turn limit findings |
| Scale experiment design | `terminal-bench/docs/scale-experiment-design.md` | Experiment architecture and analysis plan |
| Pilot results synthesis | `terminal-bench/docs/pilot-results-synthesis.md` | Complete pilot analysis |

---

## 11. Syncing Results Back

After the experiment completes, push results back to the repo:

```bash
cd ~/workgraph

# Check what's new
git status

# Stage only the results directory (never git add -A)
git add terminal-bench/results/<run-directory>/

# Commit with a descriptive message
git commit -m "data: TB scale experiment results — A vs G, 180 trials"

# Push
git push
```

For large result sets, consider pushing incrementally (one condition at a time)
to keep commit sizes manageable:

```bash
git add terminal-bench/results/<run-dir>/condition-A/
git add terminal-bench/results/<run-dir>/condA-*.json
git commit -m "data: TB condition A results (90 trials)"
git push

git add terminal-bench/results/<run-dir>/condition-G/
git add terminal-bench/results/<run-dir>/condG-*.json
git commit -m "data: TB condition G results (90 trials)"
git push
```

Also push the summary and comparison files:

```bash
git add terminal-bench/results/<run-dir>/summary.json \
       terminal-bench/results/<run-dir>/comparison.md \
       terminal-bench/results/<run-dir>/manifest.json \
       terminal-bench/results/<run-dir>/config.json
git commit -m "data: TB scale experiment summary and comparison"
git push
```
