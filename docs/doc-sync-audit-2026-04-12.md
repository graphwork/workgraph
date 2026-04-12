# Documentation Audit — 2026-04-12

Comparison of all key documentation files against current CLI help output (`wg --help-all`, per-command `--help`), code features, and runtime behavior.

Last verified: 2026-04-12

---

## Summary

- **Total doc files in `docs/`**: 271
- **Total CLI commands**: 97 (from `wg --help-all`)
- **Key user-facing docs audited**: 16 (plus CLAUDE.md, SKILL.md, quickstart.rs)
- **Commands missing from COMMANDS.md**: 4
- **New `wg add` flags missing from COMMANDS.md**: 6
- **Dead file references in KEY_DOCS.md**: 4
- **Docs in `docs/` not indexed in KEY_DOCS.md**: 80 (mostly archive/research; 15 non-archive)
- **README features not mentioned**: 4 (telegram, cron, profile, spend/user-board)

---

## Per-Document Delta Checklist

### 1. `docs/COMMANDS.md` — Command Reference

**Missing commands** (exist in CLI, not documented):
- [ ] `wg spend` — Token usage and cost summaries (`--today`, `--json`)
- [ ] `wg profile` — Provider profiles: `set`, `show`, `list`, `refresh`
- [ ] `wg openrouter` — OpenRouter cost monitoring: `status`, `session`, `set-limit`
- [ ] `wg user` — Per-user conversation boards: `init`, `list`, `archive`

**Missing `wg add` flags** (exist in `wg add --help`, not in COMMANDS.md):
- [ ] `--exec <CMD>` — Shell command to execute (auto-sets exec_mode=shell)
- [ ] `--independent` / `--no-after` — Suppress implicit `--after` dependency on creating task
- [ ] `--allow-phantom` — Allow phantom (forward-reference) dependencies
- [ ] `--propagation <POLICY>` — Retry propagation: conservative, aggressive, conditional
- [ ] `--retry-strategy <STRATEGY>` — Retry strategy: same-model, upgrade-model, escalate-to-human
- [ ] `--cron <EXPR>` — Cron schedule expression (6-field)

**Missing `wg edit` flags** (in `wg edit --help` but not in COMMANDS.md edit section):
- [ ] `--allow-phantom` — Allow phantom dependencies
- [ ] `--allow-cycle` — Allow cycle creation without CycleConfig

**Accuracy issues:**
- [ ] `wg edit` section doesn't mention `--provider` changes being deprecated (matches `wg config --coordinator-provider` deprecation pattern)

### 2. `README.md` — Project Overview

**Missing feature coverage:**
- [ ] Telegram integration (`wg telegram send/listen/poll/ask`) not mentioned
- [ ] Cron scheduling (`wg add --cron`) not mentioned
- [ ] Provider profiles (`wg profile`) not mentioned
- [ ] `wg spend` (cost monitoring) not mentioned
- [ ] `wg user` (per-user boards) not mentioned

**Staleness:**
- [ ] Model names reference `opus-4-5` in config help but README examples show `opus`/`sonnet`/`haiku` shorthand — this is correct (shorthand is canonical), but the config help text says `opus-4-5` which is a specific alias
- [ ] Config section mentions `--creator-model` but that flag doesn't appear in `wg config --help` — verify this is valid or stale

**Otherwise accurate:** Install instructions, setup flow, service management, agent management, analysis commands, cycles, patterns, verification workflow, skill install — all verified current.

### 3. `docs/AGENT-GUIDE.md` — Agent Guide

**Missing:**
- [ ] Telegram escalation not mentioned (agents can use `wg telegram send` for human contact)
- [ ] User boards not mentioned
- [ ] Screencast commands not mentioned (minor — not agent-relevant)
- [ ] `wg spend` not mentioned for cost awareness

**Staleness:**
- [ ] Placement flow section describes merged placement-in-assignment model — verify this matches current coordinator code (the `auto_place` config flag is current)

**Accurate:** Pattern recognition table, task lifecycle states, validation flow, retry workflow, cascade abandon, supersession, decomposition patterns — all verified current against CLI help.

### 4. `docs/AGENT-SERVICE.md` — Service Architecture

**Missing:**
- [ ] Multi-coordinator sessions not mentioned in detail (only mentioned in COMMANDS.md and README)
- [ ] `wg service interrupt-coordinator` not mentioned
- [ ] Heartbeat interval config (`--heartbeat-interval`) for autonomous coordinator mode not mentioned
- [ ] Compaction (`.compact-0` task) not mentioned

**Otherwise accurate:** Coordinator tick steps, zero-output detection, auto-checkpoint, cycle iteration, message-triggered resurrection — all verified current.

### 5. `docs/AGENCY.md` — Agency System

**Missing:**
- [ ] Creator agent (`wg agency create`, `--auto-create`) not mentioned
- [ ] Agency stats `--by-model` flag not mentioned
- [ ] Provider profiles interaction with agency (model tier routing) not described

**Accurate:** Core concepts (role, tradeoff, agent), content-hash IDs, full agency loop, CLI commands — all verified current.

### 6. `docs/LOGGING.md` — Logging & Provenance

**Accurate overall.** Operation log format, agent archives, rotation — all match current behavior.

**Minor:**
- [ ] Doesn't mention `wg spend` as a way to query cost data from logs
- [ ] Doesn't mention checkpoint integration with logging

### 7. `docs/DEV.md` — Development Notes

**Accurate:** Build/test commands, typst→markdown conversion, service operations, worktree isolation.

**Missing:**
- [ ] Provider profiles (`wg profile`) not in function catalog
- [ ] `wg cleanup nightly` not mentioned in service operations

### 8. `docs/models.md` — Model/Endpoint/Key Management

**Missing:**
- [ ] Provider profiles (`wg profile set/show/list/refresh`) — this is a major new feature for model management that belongs here
- [ ] `wg models benchmarks` and `wg models fetch` commands
- [ ] OpenRouter cost monitoring (`wg openrouter status/session/set-limit`)

**Otherwise accurate:** Quick start, concepts diagram, endpoint management, key management.

### 9. `docs/WORKTREE-ISOLATION.md` — Worktree Isolation

**Accurate.** Research report format, describes the model correctly. This is a design/research document, not a usage guide — staleness is expected and acceptable.

### 10. `docs/COORDINATOR_ENTITY.md` — Coordinator Entity Design

**Status:** Design document. Should be checked for accuracy against current coordinator implementation (multi-coordinator sessions, chat, compaction).

### 11. `docs/MODEL_REGISTRY.md` — Model Registry

**Missing:**
- [ ] `wg models benchmarks` command
- [ ] `wg models fetch` command
- [ ] Provider profiles as a higher-level routing layer

### 12. `docs/SECURITY.md` — Security Guide

**Accurate.** Pre-commit hooks, GitGuardian, env-based config, notify.toml — all current.

### 13. `docs/agent-git-hygiene.md` — Git Hygiene for Agents

**Accurate.** Rules match current agent prompt injection (surgical staging, no stash, no force push).

### 14. `docs/guides/openrouter-setup.md` — OpenRouter Setup

**Missing:**
- [ ] `wg openrouter status/session/set-limit` commands
- [ ] Provider profiles for OpenRouter model selection

### 15. `docs/guides/server-setup.md` — Server Setup

**Should be audited:** Multi-user server features may have evolved.

### 16. `docs/manual/` — Manual Chapters (Typst + Markdown)

The manual (01-overview through 05-evolution) is a deep reference. The Typst files are ground truth; markdown is derived.

**Missing from KEY_DOCS.md:** All markdown chapter files (`01-overview.md` through `05-evolution.md`, `workgraph-manual.md`) are not indexed — only the `.typ` files are.

**Potential staleness:**
- [ ] Manual chapters may not reflect newest commands (spend, profile, openrouter, user, cron, telegram)
- [ ] New task flags (--exec, --independent, --cron, --propagation, --retry-strategy) likely not in manual

### 17. `CLAUDE.md` — Project Instructions

**Accurate.** Matches current `wg quickstart` output and service model. The "orchestrating agent role" section is current.

### 18. `.claude/skills/wg/SKILL.md` — Claude Code Skill

**Should be audited for:** Newest commands. Currently teaches core workflow but may miss newer commands like `wg telegram`, `wg user`, `wg spend`, `wg profile`.

### 19. `src/commands/quickstart.rs` — Quickstart Text

**Verified current** against `wg quickstart` output. Covers skill setup, agency setup, service mode, manual mode, multi-coordinator, patterns, and full command reference.

---

## KEY_DOCS.md Index Issues

### Dead references (files no longer exist):
- [ ] `docs/research/primitive-pool-sync.md`
- [ ] `docs/research/ranked-model-list.md`
- [ ] `docs/reports/bug-report-dynamic-model-list-browsing.md`
- [ ] `docs/plans/federation-and-distributed-sync.md`
- [ ] `~/.claude/CLAUDE.md` — may not exist (note says "may not exist" which is correct, but listing it as embedded doc is misleading)

### Non-archive docs missing from KEY_DOCS.md index:
- [ ] `docs/AGENT-LIFECYCLE.md` — Hardened agent lifecycle documentation
- [ ] `docs/SECURITY.md` — Security guide (pre-commit, secret management)
- [ ] `docs/manual/01-overview.md` through `05-evolution.md` — Markdown manual chapters
- [ ] `docs/manual/workgraph-manual.md` — Full assembled manual (markdown)
- [ ] `docs/design/bare-coordinator.md` — Bare coordinator design
- [ ] `docs/design/coordinator-id-assignment.md` — Coordinator ID assignment
- [ ] `docs/design/design-autopoietic-task-agency.md` — Autopoietic task agency
- [ ] `docs/design/native-graph-iteration.md` — Native graph iteration
- [ ] `docs/design/phantom-edge-prevention.md` — Phantom edge prevention
- [ ] `docs/design/safe-coordinator-cycle.md` — Safe coordinator cycle
- [ ] `docs/design-shell-executor.md` — Shell executor design
- [ ] `docs/designs/chat-message-ordering-and-delivery.md` — Chat ordering
- [ ] `docs/designs/failed-dep-triage.md` — Failed dep triage
- [ ] `docs/designs/quality-pass.md` — Quality pass design
- [ ] `docs/designs/tui-iteration-history-and-viz-selfloop.md` — TUI iteration
- [ ] `docs/audit/agent-work-integrity.md` — Agent work integrity audit
- [ ] `docs/terminal-bench/` — Terminal bench docs (4 files)
- [ ] `docs/plans/assignment-time-placement-guard.md`
- [ ] `docs/plans/model-registry-and-update-trace.md`
- [ ] `docs/plans/provider-profiles.md`
- [ ] `docs/plans/spiral-unrolling-design.md`
- [ ] `docs/plans/user-board-design.md`
- [ ] `docs/reports/bug-report-assign-task-not-blocking.md`
- [ ] `docs/reports/bug-report-user-board-leak.md`
- [ ] `docs/reports/openrouter-new-repo-setup-guide.md`
- [ ] `docs/reports/research-coordinator-chat-ordering.md`
- [ ] `docs/reports/smoke-test-cycle-lifecycle.md`
- [ ] `docs/reports/triage-task-naming-investigation.md`
- [ ] 15+ research docs added after last KEY_DOCS.md update

---

## Downstream Task Routing

Based on this audit, here is what each downstream doc-sync task needs to address:

### `doc-sync-commands` — Command Reference
- Add `wg spend`, `wg profile`, `wg openrouter`, `wg user` commands
- Add missing `wg add` flags: `--exec`, `--independent`, `--allow-phantom`, `--propagation`, `--retry-strategy`, `--cron`
- Add missing `wg edit` flags: `--allow-phantom`, `--allow-cycle`

### `doc-sync-readme` — README
- Add Telegram integration section
- Add cron scheduling mention
- Add provider profiles mention
- Add `wg spend` and `wg user` mentions
- Verify `--creator-model` config reference

### `doc-sync-quickstart` — Quickstart
- Quickstart (`wg quickstart`) is current — no changes needed
- Verify skill install mentions are current

### `doc-sync-agent` — Agent/Service Guides
- `AGENT-SERVICE.md`: Add multi-coordinator details, interrupt-coordinator, heartbeat-interval, compaction
- `AGENT-GUIDE.md`: Add telegram escalation mention
- `AGENT-LIFECYCLE.md`: Verify current (appears comprehensive)

### `doc-sync-agency` — Agency Subsystem
- `AGENCY.md`: Add creator agent, stats --by-model, profile interaction
- `models.md`: Add provider profiles, benchmarks, fetch, openrouter monitoring
- `MODEL_REGISTRY.md`: Add benchmarks/fetch commands

### `doc-sync-skill` — Skill Definition
- Audit `SKILL.md` for newest commands coverage (telegram, user, spend, profile)

### `doc-sync-manual` — Manual Chapters
- Audit typst chapters for newest features (cron, telegram, profiles, spend, user boards)
- Regenerate markdown from typst after updates

### KEY_DOCS.md Update (this task)
- Remove 4 dead file references
- Add ~15 non-archive missing docs
- Add markdown manual chapter entries
- Update "Last updated" timestamp
