# Review: Documentation Quality

**Date:** 2026-02-11
**Scope:** All documentation files — README.md, docs/, CLAUDE.md, NOTES.md, .claude/skills/wg/SKILL.md, and all research docs.

---

## Executive Summary

The documentation is **extensive but unevenly maintained**. The top-level README.md is the strongest doc — comprehensive, well-structured, and mostly accurate. The docs/ directory contains a mix of (a) reference docs that are partially stale, (b) design docs for features that have been implemented (but the docs weren't updated to reflect reality), (c) early research docs that served their purpose but are now historical artifacts, and (d) recent review docs from the ongoing code review effort. The SKILL.md is accurate and well-focused. CLAUDE.md is appropriately terse.

**Key finding:** The biggest gap isn't missing documentation — it's **stale documentation that describes the system as it was designed, not as it was built**. Several docs describe features in their design-phase state and haven't been updated post-implementation.

---

## 1. Document-by-Document Assessment

### 1.1 README.md (512 lines) — GOOD, minor issues

**Accuracy: 8/10**

The README is the best documentation in the project. It covers installation, setup, service mode, agent management, TUI, configuration, and troubleshooting. It reads like a real product README.

**Issues found:**

| Issue | Severity | Details |
|-------|----------|---------|
| Model name `opus-4-5` in config examples | Low | Config examples show `model = "opus-4-5"` — this works but the default model was changed to just `"opus"` per commit 2a6763e. May confuse users who see different default vs example. |
| Missing commands in "Analysis commands" section | Medium | Lists 8 commands (`ready`, `list`, `show`, `why-blocked`, `impact`, `bottlenecks`, `critical-path`, `forecast`, `analyze`) but omits `status`, `dag`, `velocity`, `aging`, `structure`, `workload`, `loops`. The `status` and `dag` commands are among the most-used. |
| Missing `edit` command | Medium | The `wg edit` command (added in 8279a35) is not mentioned anywhere in the README. This is a significant workflow command. |
| Missing `approve`/`reject`/`submit` commands | Medium | The verified task workflow (`submit` → `approve`/`reject`) is undocumented in the README. |
| Missing `evaluate`/`evolve` in workflow | Low | The agency system's evaluate/evolve workflow isn't mentioned in the README (though AGENCY.md covers it). |
| `wg actor add` example has `-c` flag | OK | This is accurate — `-c` is the short form of `--capability`. |
| Missing `--model` flag on `wg add` | Low | The README shows `--skill`, `--hours`, etc. but doesn't mention `--model` for per-task model selection. |
| Missing `--verify` flag on `wg add` | Low | Flag exists but undocumented in README. |
| Missing `--desc` alias for `--description` | Low | The recently added alias isn't shown. |
| "More docs" section is incomplete | Medium | Only links to COMMANDS.md and AGENT-GUIDE.md. Should also link to AGENCY.md and the SKILL.md. |

**What's good:**
- Service section is thorough and accurate (start/stop/status/reload/install all correct)
- Agent management section (agents, kill, dead-agents) is accurate
- TUI section matches the actual implementation
- Configuration section is accurate (config.toml structure, CLI overrides)
- Troubleshooting section is genuinely useful

### 1.2 docs/README.md (233 lines) — GOOD, minor staleness

**Accuracy: 7/10**

Good conceptual overview of core concepts (tasks, actors, resources, dependencies, context flow, trajectories). The status flow diagram is accurate.

**Issues found:**

| Issue | Severity | Details |
|-------|----------|---------|
| Missing `pending-review` status | Medium | The status flow diagram shows open→in-progress→done/failed/abandoned but omits the `pending-review` state used by verified tasks (submit→approve/reject). |
| Missing `agent` field on tasks | Low | Task fields list shows `assigned` (actor) but not the `agent` field from the agency system. |
| Config example shows `model = "opus-4-5"` | Low | Same model name issue as README. |
| No mention of agency system | Medium | The core concepts section doesn't mention roles, motivations, or agents — a major feature of the system. |
| No link to AGENCY.md | Low | See Also section only links to COMMANDS.md and AGENT-GUIDE.md. |

### 1.3 docs/COMMANDS.md (988 lines) — STALE, needs significant update

**Accuracy: 5/10**

This was a good reference when written but has fallen significantly behind the actual CLI. The CLI now has **67 commands** (65 without Matrix), but COMMANDS.md documents roughly 35.

**Missing commands (not documented at all):**

| Command | Purpose | Priority to document |
|---------|---------|---------------------|
| `wg edit` | Edit task fields after creation | High |
| `wg submit` | Submit verified task for review | High |
| `wg approve` | Approve pending-review task | High |
| `wg reject` | Reject pending-review task | High |
| `wg status` | Quick one-screen overview | High |
| `wg quickstart` | Agent onboarding cheat sheet | High |
| `wg dag` | ASCII DAG visualization | High |
| `wg reclaim` | Reclaim task from dead agent | Medium |
| `wg evaluate` | Trigger evaluation of completed task | Medium |
| `wg evolve` | Trigger evolution cycle | Medium |
| `wg role` | Manage agency roles | Medium |
| `wg motivation` | Manage agency motivations | Medium |
| `wg agent create/list/show/rm/...` | Manage agents | Medium |
| `wg assign` | Assign agent to task | Medium |
| `wg agency` | Agency management (init, stats) | Medium |
| `wg notify` | Send Matrix notification | Low |
| `wg matrix` | Matrix integration commands | Low |
| `wg dead-agents` | Dead agent detection/cleanup | Medium |
| `wg service pause/resume` | Pause/resume coordinator | Medium |

**Inaccurate entries:**

| Issue | Details |
|-------|---------|
| `wg config` flags incomplete | Missing `--max-agents`, `--coordinator-interval`, `--poll-interval`, `--coordinator-executor`, `--auto-evaluate`, `--auto-assign`, `--assigner-model`, `--evaluator-model`, `--evolver-model`, `--retention-heuristics`, and Matrix config flags. |
| `wg add` flags incomplete | Missing `--model`, `--verify`, `--desc` alias. |
| `wg service` subcommands incomplete | Missing `pause`, `resume`. `tick` and `daemon` are internal but exist. |
| `wg agent` conflated | COMMANDS.md documents `wg agent` as the autonomous agent loop. In reality, `wg agent` is now a subcommand group for agency agent management (`create`, `list`, `show`, `rm`, `lineage`, `performance`), with `run` as the agent loop subcommand. |

### 1.4 docs/AGENT-GUIDE.md (695 lines) — PARTIALLY STALE

**Accuracy: 6/10**

Good conceptual guide for agent operation, but predates the service layer refactoring and agency system.

**Issues found:**

| Issue | Severity | Details |
|-------|----------|---------|
| `wg agent --actor` is outdated | High | The autonomous agent loop is now `wg agent run`, not `wg agent --actor`. The `wg agent` command is now a subcommand group. |
| No mention of agency system | Medium | The guide describes agents as actors with capabilities but doesn't mention roles, motivations, or the identity system. |
| Config example shows old model names | Low | `opus-4-5` references. |
| Missing service mode integration | Medium | The guide doesn't describe how agents work with the service daemon — it still describes the old standalone `wg agent` loop pattern. |
| Score calculation may be stale | Medium | The task selection scoring algorithm (lines 237-244) may not match current implementation. |

### 1.5 docs/AGENT-SERVICE.md (368 lines) — MOSTLY DESIGN DOC, PARTIALLY STALE

**Accuracy: 4/10**

This reads as the **original design document** for the service layer, not as documentation of the actual implementation. Many specifics don't match reality:

| Issue | Severity | Details |
|-------|---------|---------|
| IPC protocol description | High | Describes a JSON-over-socket protocol that may not match actual implementation. The real daemon uses a different approach (see review-service-layer.md). |
| Executor plugin TOML files | High | Describes `.workgraph/executors/claude.toml` config files — these don't exist in the actual implementation. Executors are hardcoded in `src/service/executor.rs`. |
| Notification config | High | Describes `[service.notifications]` with `on_agent_done`, `on_agent_failed`, etc. — this config section doesn't exist. Matrix notifications are the actual mechanism. |
| Output routing to `artifacts/` | Medium | Describes `.workgraph/agents/agent-N/artifacts/` — the actual output goes to `.workgraph/service/agents/agent-N/`. |
| `wg service start --port --socket` flags | Medium | Port flag may not be implemented; socket path handling differs. |
| Coordinator pattern pseudocode | Low | Conceptually correct but doesn't reflect the actual daemon implementation. |
| Future extensions section | Low | Lists features not yet implemented (web UI, remote agents, resource limits, priority queue, agent pools). |

**Verdict:** This document should either be rewritten as accurate documentation or clearly labeled as a historical design document.

### 1.6 docs/AGENCY.md (499 lines) — GOOD, accurate

**Accuracy: 9/10**

This is the most recently written reference doc and is well-aligned with the actual implementation. The agency system (roles, motivations, agents, evaluation, evolution, lineage) is accurately described.

**Minor issues:**

| Issue | Severity | Details |
|-------|---------|---------|
| Missing `wg agency init` details | Low | Mentions `wg agency init` creates starter roles/motivations but doesn't list what the starters are. |
| Evaluation dimension percentages | Low | States correctness=40%, completeness=30%, efficiency=15%, style_adherence=15%. Should verify these match current implementation. |

### 1.7 docs/tui-design.md (730 lines) — DESIGN DOC, useful for context

**Accuracy: N/A (design doc)**

This is a design document for the TUI, written before implementation. The actual TUI has been implemented but diverged from this design in several ways:

- The README describes 3 views (Dashboard, Graph Explorer, Log Viewer) with different keybindings than this design doc describes (Task List, Ready Queue, Graph with different keybindings).
- The design doc proposes features like file watching, add-task modal, and split-pane layout — unclear which were implemented.

**Verdict:** Useful as historical context but should be labeled as a design document, not current documentation.

### 1.8 docs/dynamic-help-design.md (401 lines) — DESIGN DOC, implemented

**Accuracy: N/A (design doc)**

The dynamic help ordering has been implemented — `wg --help` now shows tiered command grouping based on usage stats, exactly as this design doc proposed. The `--alphabetical` and `--help-all` flags are live.

**Verdict:** Could be labeled as "implemented design doc" or archived. The behavior it describes is now live.

### 1.9 docs/architectural-issues.md (262 lines) — HISTORICAL, all issues resolved

**Accuracy: N/A (historical)**

Lists 7 architectural issues from early in the project. **All have been resolved:**

1. Agent identity model → Implemented via agency system
2. Task-agent matching → Implemented via skill matching + agency assignment
3. Context inheritance → Implemented via artifacts/deliverables + `wg context`
4. Execution model → Hybrid approach implemented via service + executors
5. Failure handling → `failed` status, retry, max_retries all exist
6. Coordination pattern → Service daemon with coordinator implemented
7. Progress visibility → `wg log` implemented

**Verdict:** Should be archived or removed. All issues are resolved.

### 1.10 docs/ROLES-IDEA.md (208 lines) — HISTORICAL, superseded by agency system

**Accuracy: N/A (historical)**

This was the original proposal for declarative role definitions. The agency system (AGENCY.md) is the evolved version of this proposal. Key differences:
- ROLES-IDEA proposed markdown files in `.workgraph/roles/` → Agency uses YAML in `.workgraph/agency/roles/`
- ROLES-IDEA proposed `--role` on `wg add` → Agency uses `wg assign <task> <agent-hash>`
- ROLES-IDEA proposed role weight via token counting → Not implemented as described

**Verdict:** Should be archived or removed. Superseded by the agency system.

### 1.11 CLAUDE.md (21 lines) — GOOD, accurate

Correctly instructs agents to use `wg quickstart`, `wg service start`, `cargo install --path .`, and warns against built-in task tools.

### 1.12 NOTES.md (178 lines) — HISTORICAL research notes

Early brainstorming document. Contains the original vision and research synthesis for workgraph. Useful as historical context but not as documentation.

### 1.13 .claude/skills/wg/SKILL.md — GOOD, accurate

**Accuracy: 9/10**

Well-focused skill definition that correctly teaches agents the coordinator pattern. Accurately covers:
- `wg quickstart` orientation
- Coordinator role (don't claim, don't spawn, let service handle it)
- Spawned agent workflow (show, context, log, done)
- Manual mode fallback
- Quick reference table

**Minor issue:** The quick reference table includes `wg submit` with the description "Submit verified task for review" — this is accurate and good.

---

## 2. Research Documents Assessment

### Relevance Matrix

| Document | Lines | Still Relevant? | Recommendation |
|----------|-------|----------------|----------------|
| `NOTES.md` | 178 | Historical | Archive or keep as-is (founding vision) |
| `rust-ecosystem-research.md` | ~500 | Partially | Technology choices were made; useful if revisiting dependencies |
| `csp-process-algebra-research.md` | ~500 | Partially | Formal verification layer not yet built; relevant if pursued |
| `petri-nets-research.md` | ~500 | Partially | Same as above |
| `task-format-research.md` | ~400 | No | Format decisions made (JSONL chosen and implemented) |
| `beads-gastown-research.md` | ~500 | Partially | Competitive analysis; useful for positioning |
| `workgraph-analysis-research.md` | ~400 | Yes | Analysis commands design — still useful for extending analysis features |
| `human-interface-research.md` | ~500 | Partially | TUI chosen and built; web/IDE sections still relevant for future |
| `human-summoning-research.md` | ~500 | Yes | Matrix integration built, but other notification channels not yet explored |
| `agent-simulation-findings.md` | ~200 | No | Findings from early agent testing; all identified gaps have been fixed |
| `token-counting-research.md` | ~300 | Partially | Role weight via token counting not implemented; relevant if pursued |
| `usage-stats-research.md` | ~300 | No | Dynamic help ordering is implemented |
| `dynamic-help-design.md` | ~400 | No | Implemented |
| `survey-context-management.md` | ~300 | Yes | Recent audit of context management; findings likely still actionable |
| `tui-design.md` | ~730 | Partially | TUI implemented but may have diverged; useful for future TUI work |
| `architectural-issues.md` | ~262 | No | All issues resolved |
| `ROLES-IDEA.md` | ~208 | No | Superseded by agency system |

### Recent Review Documents

| Document | Lines | Purpose |
|----------|-------|---------|
| `review-matrix.md` | ~300 | Code review of Matrix integration |
| `review-cli-commands.md` | ~300 | Code review of CLI structure |
| `review-analysis-commands.md` | ~300 | Code review of analysis commands |
| `review-service-layer.md` | ~300 | Code review of service daemon |
| `review-config-build.md` | ~300 | Code review of config and build system |

These are outputs from the current review sprint and should be kept as-is (they document code quality findings).

---

## 3. Undocumented Features

Commands and features that exist in code but have **no documentation**:

| Feature | Where it exists | Impact |
|---------|----------------|--------|
| Verified task workflow (`--verify`, `submit`, `approve`, `reject`) | CLI + graph logic | High — users can't discover this workflow |
| `wg edit` command | CLI | High — essential for modifying tasks after creation |
| `wg status` command | CLI | High — one of the most-used commands, no docs |
| `wg quickstart` command | CLI | Medium — mentioned in CLAUDE.md but not in COMMANDS.md |
| `wg dag` command | CLI | Medium — most-used command per usage stats, no docs |
| `wg service pause/resume` | CLI | Medium — operational commands with no docs |
| `wg reclaim` command | CLI | Medium — useful for dead agent recovery |
| `wg dead-agents` command | CLI | Low — documented in README but not COMMANDS.md |
| `flock`-based graph locking | graph.rs | Low — internal mechanism, but good to note |
| `--model` flag on `wg add` | CLI | Medium — per-task model selection |
| Agency config flags (`--auto-evaluate`, `--auto-assign`, etc.) | CLI config | Medium — agency automation not documented |
| Matrix notification integration | CLI + config | Low — feature-gated, specialized |
| `wg edit` command with flock locking | CLI | Medium |

---

## 4. Documentation Strategy Recommendations

### Immediate Actions (High Priority)

1. **Update docs/COMMANDS.md** — This is the most impactful change. Add all missing commands (edit, submit, approve, reject, status, quickstart, dag, reclaim, dead-agents, evaluate, evolve, role, motivation, agent, assign, agency, service pause/resume). Update existing entries with missing flags.

2. **Update README.md "Analysis commands" section** — Add `status`, `dag`, `velocity`, `aging`, `structure`, `workload`, `loops`. These are real commands users need to discover.

3. **Add verified task workflow to README.md** — The submit→approve/reject pattern needs a brief section.

4. **Add `wg edit` to README.md** — It's a core workflow command.

5. **Link AGENCY.md from README** — The "More docs" section should include it.

### Medium Priority

6. **Rewrite or retire AGENT-SERVICE.md** — Either update it to match the actual implementation (using review-service-layer.md findings as a source) or move it to a `docs/archive/` directory with a note that it was the original design doc.

7. **Rewrite AGENT-GUIDE.md** — Update the `wg agent` command references (now `wg agent run`), add agency system integration, update service mode description.

8. **Update docs/README.md** — Add `pending-review` status to flow diagram, mention agency system in core concepts, link to AGENCY.md.

9. **Add `wg edit` documentation** — Probably deserves mention in both README and COMMANDS.md since it was just added.

### Low Priority

10. **Archive historical docs** — Move to `docs/archive/`:
    - `architectural-issues.md` (all resolved)
    - `ROLES-IDEA.md` (superseded)
    - `agent-simulation-findings.md` (all gaps fixed)
    - `task-format-research.md` (decisions made)
    - `usage-stats-research.md` (implemented)

11. **Label design docs** — Add a header to design docs noting their status:
    - `tui-design.md` → "Design doc. TUI has been implemented; actual behavior may differ."
    - `dynamic-help-design.md` → "Design doc. Implemented in current release."

12. **Keep research docs** — The following are still useful references:
    - `survey-context-management.md` (recent, actionable findings)
    - `workgraph-analysis-research.md` (useful for extending analysis)
    - `human-summoning-research.md` (notification channels not yet fully explored)
    - `csp-process-algebra-research.md` and `petri-nets-research.md` (relevant if formal verification is pursued)

### Documentation Gaps to Fill (Future)

| Gap | Priority | Notes |
|-----|----------|-------|
| Architecture overview doc | Medium | High-level doc showing how main.rs, graph.rs, service/, agency.rs, and commands/ fit together |
| Configuration reference | Medium | Complete reference for config.toml with all sections ([agent], [coordinator], [agency], [matrix]) |
| Agency quick-start guide | Low | AGENCY.md is thorough but dense; a quick-start would help adoption |
| Contributing guide | Low | For external contributors: how to build, test, add commands |

---

## 5. Summary Scorecard

| Document | Accuracy | Completeness | Action Needed |
|----------|----------|--------------|---------------|
| README.md | 8/10 | 7/10 | Update analysis commands, add edit/submit/approve/reject, link AGENCY.md |
| docs/README.md | 7/10 | 6/10 | Add pending-review status, mention agency system |
| docs/COMMANDS.md | 5/10 | 4/10 | **Major update needed** — 30+ missing commands |
| docs/AGENT-GUIDE.md | 6/10 | 5/10 | Update wg agent → wg agent run, add service integration |
| docs/AGENT-SERVICE.md | 4/10 | 3/10 | Rewrite or archive (design doc, not accurate documentation) |
| docs/AGENCY.md | 9/10 | 9/10 | Minor updates only |
| CLAUDE.md | 10/10 | 10/10 | No changes needed |
| .claude/skills/wg/SKILL.md | 9/10 | 9/10 | No changes needed |
| docs/tui-design.md | N/A | N/A | Label as design doc |
| docs/dynamic-help-design.md | N/A | N/A | Label as implemented design doc |
| docs/architectural-issues.md | N/A | N/A | Archive (all resolved) |
| docs/ROLES-IDEA.md | N/A | N/A | Archive (superseded) |

**Overall documentation health: 6/10** — Strong in some areas (README, AGENCY.md, SKILL.md) but the command reference is seriously behind and several docs are stale.
