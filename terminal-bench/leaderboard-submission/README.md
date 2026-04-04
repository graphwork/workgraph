# Terminal-Bench 2.0 Leaderboard Submission

Prepared submission files for the [Terminal-Bench 2.0 leaderboard](https://huggingface.co/datasets/harborframework/terminal-bench-2-leaderboard).

## Status: NOT YET SUBMITTABLE

The leaderboard requires **minimum 5 trials per task**. Our current experiment has 3 trials per task. Two additional trial runs per condition are needed before submission.

## Conditions

| Directory | Agent | Pass Rate (3 trials) |
|-----------|-------|---------------------|
| `workgraph-condition-a__minimax-m2.7/` | Bare agent (control) | 52.3% |
| `workgraph-condition-b__minimax-m2.7/` | Workgraph stigmergic context | 51.4% |
| `workgraph-condition-c__minimax-m2.7/` | Enhanced planning + snapshots | 49.0% |

## To complete and submit

1. Run 2 additional trials per condition:
   ```bash
   bash terminal-bench/reproduce.sh --trials 2 --condition all
   ```

2. Populate submission directories with all trial data:
   ```bash
   bash terminal-bench/prepare-leaderboard.sh
   ```

3. Fork `harborframework/terminal-bench-2-leaderboard` on HuggingFace

4. Copy each condition directory into `submissions/terminal-bench/2.0/`

5. Open a Pull Request — the validation bot will check format and trial count

## Validation checklist

- [x] `metadata.yaml` present for each condition
- [x] `timeout_multiplier` = 1.0 (default)
- [x] No resource overrides
- [ ] Minimum 5 trials per task (currently: 3)
- [ ] All trial directories populated with result.json
