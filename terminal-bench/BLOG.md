# Stigmergic Memory Doesn't Help (Yet): A Terminal-Bench Experiment

**tl;dr** — We gave an AI agent access to workgraph, an external task-graph memory system, and ran it against 89 Terminal-Bench tasks. Pass rate didn't change. The scaffolding neither helped nor hurt. This is a null result, and we think it's interesting.

## The thesis

There's a seductive idea floating around AI-agent research: the bottleneck isn't the model, it's the memory architecture. Give a small model the right external scaffolding — persistent task state, structured logging, the ability to decompose work — and it should punch above its weight. Ants do it. Termite colonies do it. The term of art is *stigmergy*: coordination through shared environmental traces rather than direct communication.

[Workgraph](https://github.com/graphwork/workgraph) is an implementation of this idea. It gives AI agents a persistent task graph they can read and write: log progress, decompose tasks into subtasks, record artifacts, and resume from where they left off. The question is whether this external memory actually changes what a model can accomplish.

## The experiment

We used [Terminal-Bench 2.0](https://terminal-bench.org), a benchmark of 89 real-world terminal tasks ranging from "compile this C project" to "write a MIPS interpreter" to "train a FastText model." Each task runs inside a Docker container with a binary pass/fail verifier. No partial credit.

We tested three conditions, all using the same model (**Minimax M2.7** via OpenRouter):

| Condition | What the agent gets |
|-----------|-------------------|
| **A** (control) | Bash + file tools. No memory, no planning, no decomposition. |
| **B** (stigmergic) | Everything in A, plus workgraph tools: `wg_log`, `wg_done`, `wg_add` (decomposition), `wg_artifact`, `wg_show`. Graph-aware context injection. |
| **C** (enhanced) | Everything in B, plus skill-injected planning prompts, work snapshots, and enhanced context management. |

Each condition ran all 89 tasks with 3 trials per task (267 trials per condition, ~800 total). Same model weights, same temperature, same timeout. The only variable was the scaffolding.

## The result

| Condition | Pass Rate | 95% CI |
|-----------|-----------|--------|
| A (bare) | 52.3% | [43.4, 61.6] |
| B (stigmergic) | 51.4% | [42.0, 60.4] |
| C (enhanced) | 49.0% | [39.4, 58.2] |

**No significant difference.** Pairwise sign tests: all p > 0.3. The confidence intervals overlap broadly. This is a clean null result.

### Where scaffolding helped (a little)

On medium-difficulty tasks (those that A solves 33–66% of the time), B and C gained +9–10 percentage points:

| Tier | # Tasks | A | B | C |
|------|---------|---|---|---|
| Easy | 33 | 100% | 84% | 83% |
| Medium | 22 | 55% | 64% | 64% |
| Hard | 34 | 0% | 7% | 2% |

B cracked two hard tasks that A never solved: `fix-ocaml-gc` (100% vs 0%) and `password-recovery` (100% vs 0%). These were cases where decomposition or persistent state plausibly helped. But these gains were offset by losses on easy tasks — the overhead of workgraph bookkeeping apparently introduced enough friction to cause failures on tasks the bare agent handles cleanly.

### Decomposition was rare

Only 6–8% of B/C trials used `wg_add` to decompose tasks. When they did, the pass rate was similar to non-decomposed trials. Terminal-Bench tasks are mostly single-scope — there's nothing to fan out. The decomposition capability existed but had no leverage.

### Overhead was modest

Workgraph tool calls consumed ~9% of total tool calls in B and C. Most were bookkeeping (`wg_log`, `wg_done`) rather than substantive (`wg_add`, `wg_show`). The model dutifully logged and recorded but rarely used the graph for strategic purposes.

## What this means

### 1. Scaffolding is not a free upgrade

Adding structure to an agent's environment is not inherently helpful. On easy tasks, it's overhead. On hard tasks that exceed the model's capabilities, no amount of scaffolding matters — the model can't solve them regardless. The sweet spot (medium tasks) showed modest gains, but not enough to shift the overall distribution.

### 2. Terminal-Bench may not be the right benchmark for memory effects

TB tasks are largely independent, single-session problems. They don't require multi-step planning across sessions, don't benefit from remembering prior attempts, and rarely have natural decomposition points. A benchmark designed around *long-horizon, multi-step, resumable* tasks might show different results.

### 3. The model matters

Minimax M2.7 is a capable but not frontier model. It's possible that a model with stronger instruction-following and planning abilities would make better use of the scaffolding. We tested this because the original thesis was about helping *small* models — but small models may also be too small to effectively use complex tools.

### 4. Null results are results

The dominant narrative in AI scaffolding research is "we added X and got Y% improvement." Publication bias hides the negatives. This experiment cost roughly $20 in API credits and 45 hours of compute. We're reporting it because knowing what *doesn't* work is at least as valuable as knowing what does.

## What's next

- **Different benchmark**: Multi-step tasks where persistent memory has structural advantage (e.g., SWE-bench with multi-file context, long-horizon planning benchmarks)
- **Different model**: Test with a frontier model to disentangle "can't use tools well" from "tools don't help"
- **Ablation on easy tasks**: The ~16pp drop on easy tasks for B/C deserves investigation — is it prompt length, tool-call overhead, or something else?
- **More trials**: We ran 3 trials per task. The leaderboard requires 5. With more data, the medium-tier signal might clarify.

## Reproduction

All data, scripts, and harness code are in the [workgraph repository](https://github.com/graphwork/workgraph) under `terminal-bench/`:

```bash
# Re-run the analysis
cd terminal-bench/results
python3 analyze.py

# Re-run a single condition (requires OPENROUTER_API_KEY)
cd terminal-bench
harbor run \
  --agent-import-path wg.adapter:ConditionAAgent \
  -m minimax/minimax-m2.7 \
  -d terminal-bench@2.0 \
  -k 3
```

Raw trial data, figures, and the full statistical report are at `terminal-bench/results/`.

## Appendix: Token and cost summary

| Condition | Total Tokens | Tokens/Solve | Error Rate |
|-----------|-------------|-------------|------------|
| A | 155M | 1.28M | 15.7% |
| B | 148M | 1.23M | 15.0% |
| C | 140M | 1.19M | 13.7% |

C was slightly more token-efficient per solve (269K tokens vs 310K for A on passing trials), but the difference is modest and doesn't translate to more solves.

---

*This post is part of the [workgraph](https://github.com/graphwork/workgraph) project. Workgraph is a task coordination graph for humans and AI agents, MIT licensed.*
