# Key Documentation Files

Canonical list of all key documentation files and their purpose. Used as the reference for future doc-sync runs.

Last updated: 2026-03-07 (verified by doc-audit-inventory)

---

## User-Facing Documentation

| File | Purpose | Audience |
|------|---------|----------|
| `README.md` | Project overview, install, setup, usage patterns, feature summary | Everyone (entry point) |
| `.claude/skills/wg/SKILL.md` | Claude Code skill definition — teaches AI agents to use workgraph | AI agents (Claude Code) |
| `docs/COMMANDS.md` | Complete CLI command reference with examples | Users, agents |
| `docs/AGENT-GUIDE.md` | How spawned agents should think about task graphs: patterns, structures, anti-patterns | AI agents, advanced users |
| `docs/AGENT-SERVICE.md` | Service daemon architecture: coordinator tick, dispatch cycle, agent lifecycle | Operators, contributors |
| `docs/AGENCY.md` | Agency system: roles, tradeoffs, evaluation, evolution, skill system | Users setting up agency |
| `docs/LOGGING.md` | Logging and provenance system: operation log, agent archives, rotation | Operators, integrators |
| `docs/DEV.md` | Development notes: build, test, reusable functions, common pitfalls | Contributors |
| `docs/WORKTREE-ISOLATION.md` | Worktree-based isolation for concurrent agents | Operators, contributors |
| `docs/COORDINATOR_ENTITY.md` | Design: coordinator as visible entity | Contributors |
| `docs/models.md` | Model, endpoint, and API key management guide | Users, agents |
| `docs/MODEL_REGISTRY.md` | Model provider registry with quality tiers | Contributors |
| `docs/AGENCY_AUDIT.md` | Agency audit | Contributors, operators |

## Embedded Documentation

| File | Purpose | Audience |
|------|---------|----------|
| `src/commands/quickstart.rs` | Quickstart text shown by `wg quickstart` — onboarding cheat sheet | AI agents, new users |
| `CLAUDE.md` | Project-level Claude Code instructions | AI agents |
| `~/.claude/CLAUDE.md` | Global Claude Code instructions | AI agents |

## Manual (Typst)

| File | Purpose | Audience |
|------|---------|----------|
| `docs/manual/01-overview.typ` | System overview: graph, agency, core loop, agency loop | Deep readers |
| `docs/manual/02-task-graph.typ` | Task graph: nodes, status, dependencies, cycles, readiness, analysis | Deep readers |
| `docs/manual/03-agency.typ` | Agency model: roles, tradeoffs, agents, content-hash IDs, skills, trust, federation | Deep readers |
| `docs/manual/04-coordination.typ` | Coordination & execution: daemon, tick, dispatch, wrapper, IPC, watch, custom executors | Deep readers |
| `docs/manual/05-evolution.typ` | Evolution & improvement: evaluation, performance, strategies, lineage, autopoiesis | Deep readers |
| `docs/manual/workgraph-manual.typ` | Manual entry point (imports all chapters) | Deep readers |
| `docs/manual/README.md` | Manual build instructions | Contributors |
| `docs/manual/PLAN.md` | Manual chapter planning notes | Contributors |
| `docs/manual/UPDATE-SPEC.md` | Spec for manual updates | Contributors |

## Report Documents

| File | Purpose | Status |
|------|---------|--------|
| `docs/reports/spark-v3-retrospective.md` | SPARK v3 retrospective: 9-day cycle analysis | Current |
| `docs/reports/autopoietic-validation.md` | Capstone synthesis: autopoietic validation | Current |
| `docs/reports/validate-core-dispatch.md` | Dispatch validation report | Current |
| `docs/reports/validate-safety-resilience.md` | Safety and resilience validation report | Current |
| `docs/reports/validate-agency-pipeline.md` | Agency pipeline validation report | Current |
| `docs/reports/validate-tui-observability.md` | TUI and observability validation report | Current |
| `docs/reports/self-hosting-integration-validation.md` | Self-hosting integration validation | Current |
| `docs/reports/messaging-research-report.md` | Messaging system research report | Current |
| `docs/reports/amplifier-research-report.md` | Amplifier research report | Current |

## Design Documents

| File | Purpose | Status |
|------|---------|--------|
| `docs/design/trace-function-protocol.md` | Three-layer function protocol (static/generative/adaptive) | Implemented |
| `docs/design/agency-federation.md` | Agency federation: scan/pull/push/remote/merge | Implemented (except global store) |
| `docs/design/cycle-aware-graph.md` | Cycle-aware graph design | Implemented |
| `docs/design/loop-convergence.md` | Loop convergence design | Implemented |
| `docs/design/cross-repo-communication.md` | Cross-repo peer communication | Implemented |
| `docs/design/provenance-system.md` | Provenance system design | Implemented |
| `docs/design/spec-patterns-vocab.md` | Pattern vocabulary spec (referenced by AGENT-GUIDE.md) | Reference |
| `docs/design/spec-cycle-integration.md` | Cycle integration spec | Implemented |
| `docs/design/spec-edge-rename.md` | Edge rename spec (blocked_by → after) | Implemented |
| `docs/design/func-rename-spec.md` | Function rename spec (trace → func) | Implemented |
| `docs/design/doc-sync-system.md` | Doc sync system design | Reference |
| `docs/design/smooth-integration.md` | Smooth integration design | Reference |
| `docs/design/vx-integration-response.md` | Veracity exchange integration | Design |
| `docs/design/spec-vx-integration-impl.md` | VX integration implementation spec | Design |

## Research Documents

| File | Purpose | Status |
|------|---------|--------|
| `docs/research/arena-evaluation/spec.md` | FLIP-style backward-inference evaluation research | Research (not shipped) |
| `docs/research/arena-evaluation/arena-evaluation-report.typ` | Arena evaluation research report | Research |
| `docs/research/arena-evaluation/context-selection.md` | Arena eval: context selection design | Research |
| `docs/research/arena-evaluation/model-selection.md` | Arena eval: model selection design | Research |
| `docs/research/arena-evaluation/evolution-input.md` | Arena eval: evolution input design | Research |
| `docs/research/arena-evaluation/eval-integration.md` | Arena eval: evaluation integration design | Research |
| `docs/research/amplifier-integration-proposal.md` | Amplifier executor integration | Research |
| `docs/research/amplifier-architecture.md` | Amplifier architecture deep dive | Research |
| `docs/research/amplifier-executor-gap.md` | Amplifier executor gap analysis | Research |
| `docs/research/amplifier-context-transfer.md` | Amplifier context transfer research | Research |
| `docs/research/logging-gaps.md` | Logging gap analysis | Research |
| `docs/research/logging-veracity-gap-analysis.md` | Logging veracity gap analysis | Research |
| `docs/research/cyclic-processes.md` | Cyclic processes research | Research |
| `docs/research/cycle-detection-algorithms.md` | Cycle detection algorithm survey | Research |
| `docs/research/file-locking-audit.md` | File locking audit | Research |
| `docs/research/veracity-exchange-integration.md` | Veracity exchange integration research | Research |
| `docs/research/veracity-exchange-deep-dive.md` | Veracity exchange deep dive | Research |
| `docs/research/agent-context-awareness.md` | Agent context awareness research | Research |
| `docs/research/agent-context-scopes.md` | Configurable agent context scopes design | Implemented (shipped in `wg add --context-scope`) |
| `docs/research/organizational-patterns.typ` | Organizational patterns research (Typst) | Research |
| `docs/research/organizational-patterns.md` | Organizational patterns research (Markdown) | Research |
| `docs/research/flip-pipeline-ordering.md` | FLIP pipeline ordering research | Research |
| `docs/research/gitbutler-virtual-branches.md` | GitButler virtual branches research | Research |
| `docs/research/git-worktrees-agent-isolation.md` | Git worktrees for agent isolation | Research |
| `docs/research/human-in-the-loop-channels.md` | Human-in-the-loop communication channels | Research |
| `docs/research/validation-current-mechanisms.md` | Validation: current mechanisms survey | Research |
| `docs/research/validation-graph-structure.md` | Validation: graph structure analysis | Research |
| `docs/research/validation-cycles.md` | Validation: cycle handling analysis | Research |
| `docs/research/validation-evaluation-quality.md` | Validation: evaluation quality | Research |
| `docs/research/validation-agent-self-checks.md` | Validation: agent self-check mechanisms | Research |
| `docs/research/validation-teaching-agents.md` | Validation: teaching agents validation skills | Research |
| `docs/research/validation-synthesis.md` | Validation: synthesis and recommendations | Research |

## Other Documentation

| File | Purpose |
|------|---------|
| `docs/README.md` | Docs directory overview |
| `docs/ADR-actor-vs-agent-identity.md` | Architecture decision record |
| `docs/REVIEW-SYNTHESIS.md` | Review synthesis |
| `docs/task-id-namespacing.md` | Task ID namespacing notes |
| `docs/cycle-support-audit.md` | Cycle support audit |
| `docs/spec-bugfixes.md` | Bug fix specs |
| `docs/fix-dag-terminology.md` | DAG terminology fix notes |
| `docs/design-cyclic-workgraph.md` | Cyclic workgraph design |
| `docs/survey-context-management.md` | Context management survey |
| `docs/test-specs/trace-replay-test-spec.md` | Test specifications |

## Archive

| Directory | Purpose |
|-----------|---------|
| `docs/archive/research/` | Historical research documents |
| `docs/archive/reviews/` | Historical review documents |
