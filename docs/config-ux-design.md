# Canonical wg config UX — design doc

**Author:** Programmer agent on `design-canonical-wg`  
**Date:** 2026-04-27  
**Source audit:** [`docs/config-canonical.md`](config-canonical.md) (referenced by line/section throughout)  
**Implementation task:** `implement-canonical-wg`

## TL;DR

1. **Built-in defaults Just Work**: a fresh install with no `~/.wg/config.toml` runs `claude:opus` via the claude CLI handler, `max_agents=8`, `coordinator_agent=true`. No user file is required for the common case (audit §2 covers every key — built-ins are already mostly correct; this doc just commits to *not writing them out*).
2. **`wg config init` becomes a real subcommand** (`wg config init --global|--local`). It writes the **minimum** delta vs built-ins: 6 lines for the typical claude-CLI user. Existing `wg config --init` flag stays as a one-release alias.
3. **`wg setup` is route-driven** (already true) — keep the 5 named routes; add an explicit `--scope global|local|both` flag, and a final summary screen showing exactly which keys will be written and why each was picked. Surface `launcher_history` as the picker default for the model/endpoint prompts.
4. **Migration: BOTH A and B**. Keep auto-detect-and-warn (B, current), and ship `wg migrate config` (A) as the opt-in command. The warn-only path stays — `wg migrate config` is the "fix it for me" button users can run when they're ready. No major-version break.
5. **TUI integration: follow-up task only**. In-scope for this design: the CLI surfaces. Out-of-scope (defer): a TUI Settings tab. Reason: cleanly designing `wg setup` first lets a TUI Settings tab reuse the same route + key-edit primitives.

---

## 1. Decision: which keys are global-only, project-only, overridable

The audit (§2 "Full key inventory") already maps every key to G / P / B / N. This doc commits to those scopes as authoritative for `wg config init` and the new `wg config lint` (see §6). Specifically:

| key class | scope | written by `wg config init`? | example keys |
|-----------|-------|------------------------------|--------------|
| **Identity / model defaults** | B (overridable) | global: yes; local: only when overriding | `agent.model`, `[tiers]`, `[models.*]` (audit §2 `[agent]`, `[models.<role>]`, `[tiers]`) |
| **Daemon tuning** | G (global-only in practice) | global: only when non-default; local: never | `dispatcher.max_agents`, `dispatcher.agent_timeout`, `dispatcher.max_coordinators`, `dispatcher.eval_frequency`, `dispatcher.heartbeat_timeout` (audit §2 `[dispatcher]`) |
| **User identity / ergonomics** | G (per-user) | global: yes if non-default | `[tui]`, `[chat]`, `[help]`, `[viz]`, `[log]`, `[checkpoint]`, `[guardrails]` (audit §2) |
| **Project metadata** | P (project-only) | local only | `[project]` (audit §2 `[project]`) |
| **Tag routing** | P (project-only) | local only | `[[tag_routing]]` (audit §2) |
| **MCP servers** | B but typically P | local default; global allowed | `[[mcp.servers]]` (audit §2 `[mcp]`) |
| **Endpoints** | B (G shared, P override) | global: per route; local: only when shadowing | `[[llm_endpoints.endpoints]]` (audit §2 `[[llm_endpoints.endpoints]]`) |
| **Agency identity bindings** | B (per-user content hashes) | global: post-`wg agency init` only | `[agency].assigner_agent` etc. (audit §2 `[agency]`) |
| **External creds** | G-only (separate file) | never written by config init | `~/.config/workgraph/matrix.toml` (audit §2 "Matrix credentials — separate file") |
| **Deprecated / no-op** | N | never written | `coordinator.compactor_*`, `agent.executor`, `dispatcher.executor`, `coordinator.verify_autospawn_enabled` (audit §2 "Code-level soon-to-deprecate" and §3) |

**One-line reason per scope choice (cite audit):**

- `[agent].model`, `[tiers]`, `[models.*]` are **B**: per-project model choices are common (audit §1 "Resolution cascades for model selection" — `agent.model` in *local* config skips tier cascade for `task_agent`).
- `[dispatcher]` is functionally **G**: every key in audit §2 `[dispatcher]` is daemon-wide; project-local override is technically allowed but the daemon runs once for all projects in the workgraph dir, so overrides are surprising.
- `[[tag_routing]]`, `[project]` are **P**: explicitly per-project taxonomy (audit §2 `[[tag_routing]]` "P (project-specific tag taxonomies)").
- `[agency]` identity hashes are **B/G**: content-addressable hashes are user-scoped, but per-project pinning is allowed (audit §2 `[agency]` "B (per project; identity hashes are content-addressable)").
- Deprecated keys (audit §2 status column "deprecated") are **never written** by `wg config init` — see §6.

---

## 2. Built-in defaults policy (no user file needed)

Goal from task description: *"a fresh install with no `~/.wg/config.toml` should Just Work for the most common case (claude CLI executor, opus model, sensible parallelism)."*

The audit (§4 "Minimal global config") shows the built-ins already cover this — `agent.model = "claude:opus"` (audit §2 `[agent]`), `dispatcher.max_agents = 8` (audit §2 `[dispatcher]`), `coordinator_agent = true` (audit §2 `[dispatcher]` `coordinator_agent`). **Decision: do not change any built-in default values.** They are correct. What changes:

1. **Stop emitting restated defaults.** When `wg config init` writes a config, it MUST only write keys whose value differs from the built-in default. The current codepath in `src/commands/setup.rs:121` (`build_config`) and `src/commands/config_cmd.rs` writes a full `Config` and serde keeps everything because `#[serde(skip_serializing_if = "is_default_executor")]` is only set on `agent.executor` (audit §2 footnote). **Rule:** every `default_*()` and `Default for *Config` impl gets a matching `skip_serializing_if` so the on-disk file is the *delta*, not the *snapshot*. Reason: the audit §3 "Confirmed staleness" shows that 75% of Erik's local config is restated defaults — the surface area is what makes it fragile.

2. **Fix `Config::global_dir()` to mirror `main.rs::resolve_workgraph_dir`.** Currently `~/.workgraph` literal (audit §1 "Stale alert"); should be `~/.wg` first, fall back to `~/.workgraph` for legacy. Reason: silent divergence breaks new users — `wg init --global` writes `~/.wg/config.toml` but `Config::load_global()` reads `~/.workgraph/config.toml`.

3. **Add `serde(alias)` for the renamed coordinator keys**: `coordinator_agent` accepts `chat_agent`, `max_coordinators` accepts `max_chats`. Reason: audit §1 "Special merge rules" — "Unknown keys are silently dropped" is the #1 footgun, and both keys are in active misuse in Erik's local config (audit §3).

These three changes are prerequisites for `wg config init` to produce a minimal correct file.

---

## 3. Concrete example files (the "after" state)

### 3.1 `~/.wg/config.toml` — minimal global

For a "claude CLI for everything, opus default" user, this is the entire global config (matches audit §4 "Recommended minimal global config"):

```toml
# ~/.wg/config.toml — written by `wg config init --global` (claude-cli route)

[agent]
model = "claude:opus"

[tiers]
fast = "claude:haiku"
standard = "claude:sonnet"
premium = "claude:opus"

[models.evaluator]
model = "claude:haiku"

[models.assigner]
model = "claude:haiku"
```

That's it. Six meaningful lines. Everything else falls through to built-ins (audit §4 "What you should NOT keep in global"). No `[dispatcher]`, no `[checkpoint]`, no `[chat]`, no `[help]`, no `[guardrails]`, no `[viz]`, no `[log]`, no `[replay]` — those defaults are already correct.

**Validation:** paste this into a fresh `~/.wg/config.toml`, run `wg service start --max-agents 4`. The daemon must start without warnings. The audit §4 already certifies these values are valid.

### 3.2 `~/.wg/config.toml` — with OpenRouter add-on

For a user who *does* want OpenRouter as a fallback path:

```toml
# ~/.wg/config.toml — written by `wg config init --global --route openrouter`

[agent]
model = "openrouter:anthropic/claude-opus-4-6"

[tiers]
fast = "openrouter:anthropic/claude-haiku-4"
standard = "openrouter:anthropic/claude-sonnet-4-6"
premium = "openrouter:anthropic/claude-opus-4-6"

[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
is_default = true
```

(Audit §3 line-280 footnote: replace stale `claude-sonnet-4` with `claude-sonnet-4-6` in `openrouter_default_registry()` — handled in implement task.)

### 3.2b `~/.wg/config.toml` — codex-cli route (updated 2026-04-28)

For users running the OpenAI Codex CLI. Model tier mapping as of codex CLI v0.124.0:

| Tier | Model | Notes |
|------|-------|-------|
| fast (haiku-equiv) | `codex:gpt-5.4-mini` | OpenAI's recommended subagent model; ~3x cheaper than gpt-5.4 |
| standard (sonnet-equiv) | `codex:gpt-5.4` | Codex CLI default as of v0.124.0; 1M context |
| premium (opus-equiv) | `codex:gpt-5.5` | Released 2026-04-23; OpenAI's current frontier model |

**Deprecated model strings** (migrate with `wg migrate config`):
- `codex:o1-pro` → `codex:gpt-5.4` (shutdown 2026-10-23)
- `codex:gpt-5-codex` → `codex:gpt-5.4` (shutdown 2026-07-23)
- `codex:gpt-5-mini` → `codex:gpt-5.4-mini`
- `codex:gpt-5` → `codex:gpt-5.4`
- `codex:gpt-5.4-pro` → `codex:gpt-5.5`

```toml
# ~/.wg/config.toml — written by `wg config init --global --route codex-cli`

[agent]
model = "codex:gpt-5.4"

[tiers]
fast = "codex:gpt-5.4-mini"
standard = "codex:gpt-5.4"
premium = "codex:gpt-5.5"

[models.evaluator]
model = "codex:gpt-5.4-mini"

[models.assigner]
model = "codex:gpt-5.4-mini"
```

The `[models.evaluator]` / `[models.assigner]` sections are critical — without them, agency meta-tasks (`.evaluate-*`, `.flip-*`, `.assign-*`) silently fall back to the built-in `claude:haiku` even on an all-codex project.

### 3.3 `.wg/config.toml` — minimal project (the workgraph repo case)

For a project that wants to override the global default to use claude CLI even when global is openrouter:

```toml
# .wg/config.toml — written by `wg config init --local`

[agent]
model = "claude:opus"

# Shadow the global openrouter endpoint so this repo runs through claude CLI.
# (claude_handler uses CLI auth — no API key needed.)
[[llm_endpoints.endpoints]]
name = "openrouter"
provider = "openrouter"
url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
is_default = false
```

Two sections, ~11 lines, replacing the current 175-line file (audit §3 "Confirmed staleness in current local").

### 3.4 `.wg/config.toml` — empty (the typical project)

```toml
# .wg/config.toml — written by `wg config init --local --bare`

[project]
name = "my-project"
```

**Most projects only need `[project]`.** Audit §5 "For most projects: empty file or no file at all" — the explicit `--bare` flag on `wg config init --local` writes only the `[project]` block.

---

## 4. `wg setup` interactive flow (wireframes)

Existing `wg setup` (`src/commands/setup.rs:1008` `pub fn run()`) already drives the 5 routes. Required changes:

### 4.1 Add `--scope` flag

Today: `wg setup` writes to whichever scope the prompt resolves (mostly local). Add: `--scope global|local|both` (default: prompt the user). Reason: audit §1 — the global vs local distinction is invisible to users today and the same wizard should fill either.

### 4.2 Surface launcher_history in model/endpoint prompts

Per `feedback_launcher_history_in_config_ui.md`: the "what model?" / "what endpoint?" prompts must offer prior CLI/TUI invocations as picker entries.

```
?  Which model do you want as the default for this scope?
   [Recent — from launcher_history]
   ❯ claude:opus  (last used 2 minutes ago, from `wg add`)
     openrouter:anthropic/claude-opus-4-6  (1 day ago, from `wg config -m`)
     local:qwen3-coder @ http://lambda01:30000  (3 days ago, from `wg nex`)
   ─────
   [Choose route default]
     claude:opus       (claude CLI route)
     codex:gpt-5.4     (codex CLI route)
     openrouter:...    (openrouter route)
     local:...         (local nex route)
   [Type custom...]
```

Implementation: `src/commands/setup.rs` already calls `record_use` (line 709). The picker code (`src/commands/setup.rs:1059` `Select::new()`) needs to read `launcher_history::list_recent(N)` and prepend the entries. Both setup.rs and config_cmd.rs already have `record_use` plumbing — only `init.rs` (line 509 already records, good) and `nex.rs` need final verification.

### 4.3 Final summary screen — show the delta, not the snapshot

```
=== Config to write ===

Scope:  global (~/.wg/config.toml)
Route:  claude-cli

Will write 5 keys:
  agent.model              = "claude:opus"
  tiers.fast               = "claude:haiku"
  tiers.standard           = "claude:sonnet"
  tiers.premium            = "claude:opus"
  models.evaluator.model   = "claude:haiku"
  models.assigner.model    = "claude:haiku"

Will NOT write (already built-in defaults):
  dispatcher.max_agents = 8     (default)
  dispatcher.coordinator_agent = true  (default)
  agent.heartbeat_timeout = 5   (default)
  ... (43 more)

[ Confirm — write file ]   [ Edit ]   [ Show all defaults ]   [ Cancel ]
```

Reason: the user sees that the file is intentionally short, not "missing things." Reduces the "is this complete?" anxiety and prevents users from re-typing defaults.

### 4.4 Existing-config handling

Today (`src/commands/setup.rs:510` `check_existing_config`): warns and prompts for confirmation. Keep that. Add: when `--scope global` and `~/.wg/config.toml` exists, offer three branches:

```
?  ~/.wg/config.toml already exists (175 lines, last modified 2 days ago).
   What would you like to do?
   ❯ Run `wg config lint` first (recommended — show what's stale before editing)
     Back up to ~/.wg/config.toml.bak.<timestamp> and rewrite from this route
     Merge new keys into existing file (preserve user customizations)
     Cancel
```

Reason: audit §6 "Migration plan" — the lint pass is the safe first step; rewriting is destructive; merging is the gentle path.

### 4.5 Wireframe — full first-run on fresh machine

```
$ wg setup

Welcome to workgraph. Let's get you configured.

?  Where should I write your config?
   ❯ Global (~/.wg/config.toml) — applies to every wg project
     Local  (./.wg/config.toml)  — applies only to this directory
     Both — minimal global + project-specific overrides

?  Which route?
   ❯ claude-cli   — uses the `claude` CLI you already have authenticated
     codex-cli    — uses the `codex` CLI (OpenAI's local agent)
     openrouter   — single API key, many models
     local        — your own llama.cpp / vllm / ollama endpoint
     nex-custom   — any OpenAI-compatible endpoint

[claude-cli selected]
?  Default model?  (anything from `claude:*`)
   [Recent]
   ❯ claude:opus    (last used 5 min ago, from `wg add`)
     claude:sonnet  (1 hr ago, from `wg config -m`)
   [Defaults]
     claude:opus   ← workhorse, high quality, slow
     claude:sonnet ← typical, balanced
     claude:haiku  ← fast, cheap, simple tasks
     [Type custom...]

?  Enable agency identity layer (auto-assign agents to tasks)? [Y/n] Y

?  Max parallel agents on `wg service start`? [8]

=== Config to write ===
... (summary as in §4.3) ...

[ Confirm ]
```

---

## 5. `wg config init` — the non-interactive command

### 5.1 Surface

```
wg config init [--global | --local] [--route ROUTE] [--bare] [--force]
```

| flag | effect |
|------|--------|
| `--global` | target `~/.wg/config.toml` |
| `--local` | target `./.wg/config.toml` (default if neither given) |
| `--route NAME` | one of `claude-cli` (default), `codex-cli`, `openrouter`, `local`, `nex-custom` |
| `--bare` | write only `[project]` (local) or `[agent].model = "claude:opus"` (global) — the absolute minimum |
| `--force` | overwrite existing file (default: refuse) |

Subcommand vs flag: **subcommand**. Reason: `wg config --init` (current flag at `src/commands/config_cmd.rs`) is a noun-as-flag that's hard to extend with arguments like `--route`, `--bare`. The flag stays for one release as a deprecated alias mapping to `wg config init --local`.

### 5.2 What it writes

For `wg config init --global --route claude-cli`: §3.1 above.  
For `wg config init --global --route openrouter`: §3.2 above.  
For `wg config init --local`: §3.3 above.  
For `wg config init --local --bare`: §3.4 above.

The output is byte-for-byte the example in §3 — every `wg config init` invocation produces a deterministic file given (scope, route, bare). Reason: deterministic output makes `wg migrate config` (§6) idempotent and lets us write tests like "diff against fixture".

### 5.3 Refuse-by-default on existing files

`wg config init --global` against an existing `~/.wg/config.toml` exits with:

```
error: ~/.wg/config.toml already exists.
       Run `wg config lint` to see what's stale, or
       run `wg migrate config --global` to rewrite to canonical form, or
       pass --force to overwrite (a backup is made automatically).
```

Reason: `init` should never silently destroy a user's customizations. The error message points at the right next step.

---

## 6. Migration: A + B (both)

**Decision: ship both.** The auto-detect-and-warn path (B) stays exactly as today. The opt-in `wg migrate config` command (A) is added for users who want to clean up.

**Reason:** B alone (current) is too passive — Erik's own configs (audit §3) have stale keys he hasn't fixed because the warning is one line at startup. A alone would feel sudden. Together: B keeps the warning visible, A gives the user a "fix it" button.

### 6.1 `wg migrate config`

```
wg migrate config [--global | --local | --all] [--dry-run]
```

For each scope, runs:

1. **Drop deprecated/no-op keys** (audit §2 status column "deprecated", §3 "Code-level soon-to-deprecate"):
   - `agent.executor`, `dispatcher.executor` (derived from model)
   - `dispatcher.compactor_*`, `dispatcher.compaction_*` (graph-cycle compactor retired)
   - `dispatcher.verify_autospawn_enabled`, `agency.flip_verification_threshold` (replaced by `.evaluate-*` + `wg rescue`)
   - `dispatcher.verify_mode` (legacy pre-Validation-section)
   - `dispatcher.poll_interval` → renamed to `dispatcher.safety_interval` (audit §2 — alias still loads)
2. **Rename misspelled keys** (audit §3 "Confirmed staleness"):
   - `chat_agent` → `coordinator_agent`
   - `max_chats` → `max_coordinators`
3. **Drop restated defaults** (audit §4 "What you should NOT keep in global"):
   - any key whose serialized value equals the built-in default
4. **Fix known stale model strings** (audit §3 "Confirmed staleness", line 280):
   - `openrouter:anthropic/claude-sonnet-4` → `openrouter:anthropic/claude-sonnet-4-6`
   - `codex:o1-pro` → `codex:gpt-5.4` (o1-pro deprecated, shutdown 2026-10-23)
   - `codex:gpt-5-codex` → `codex:gpt-5.4` (sunset 2026-07-23)
   - `codex:gpt-5-mini` → `codex:gpt-5.4-mini`, `codex:gpt-5` → `codex:gpt-5.4`
   - `codex:gpt-5.4-pro` → `codex:gpt-5.5`
5. **Resolve `[models.default]` mismatch** (audit §3 line 280): if `[models.default].model` is non-default AND the user's `[tiers]` and `[[llm_endpoints.endpoints]]` all use a different provider, leave it alone but emit a one-line warning that says "this looks unintentional" — do NOT silently change a model choice (audit §3 line 281 "internally inconsistent").

**Backup before write:** `~/.wg/config.toml.pre-migrate.<timestamp>`. Always.

**`--dry-run`:** print a unified diff of what would change. The default UX is "show me first."

### 6.2 `wg config lint` (companion command)

```
wg config lint [--global | --local | --merged]
```

Read the merged config (`load_merged`) and emit warnings for everything `wg migrate config` would change, **without rewriting**. This is the "what's stale?" exploration step before committing to the migration.

Reason: audit §6 "Migration plan" item 1 explicitly calls out that `deny_unknown_fields` is too aggressive (would break `flip_*` extensions); a positive allowlist of known keys per section is the implementation strategy. The audit table in §2 is the allowlist source.

### 6.3 The auto-warn path (B) stays

Current behavior: load-time `detect_deprecated_keys` (`src/config.rs:3624`), `LEGACY_SECTION_ALIASES` (`src/config.rs:3450`), and `deprecated_executor_warnings_for_toml` (`src/config.rs:1767`) emit one-shot stderr warnings. **Do not remove or weaken any of these in this implementation.** They're the safety net if a user never runs `wg migrate config`.

---

## 7. TUI integration scope

**Decision: out of scope for this design (and `implement-canonical-wg`).** Create a follow-up task `tui-config-settings-tab` after the CLI surfaces ship. Reason: a TUI Settings tab is a natural next step but it's UI work that benefits from the canonical CLI primitives being stable first (route definitions, key allowlist, lint logic). Trying to do both at once duplicates the route enum and the validation rules.

### 7.1 What the follow-up task would cover

(Listed here so it's not lost — implementer creates it as a follow-up at the end of `implement-canonical-wg`.)

- A `Settings` tab in `wg tui` showing the merged config grouped by section (audit §2 layout is the reference)
- Per-key edit dialogs that:
  - Show source: `built-in default` / `~/.wg/config.toml:42` / `./.wg/config.toml:7` (audit §1 "Per-key precedence")
  - Validate via the same code path as the CLI setters (`src/commands/config_cmd.rs`)
  - Surface launcher_history for model/endpoint fields (per `feedback_launcher_history_in_config_ui.md`)
- A "Run setup wizard" button that opens `wg setup` interactively in a TUI subshell (or replays the wizard inline)
- A "Run `wg config lint`" button that displays the warnings inline

### 7.2 What the in-scope work does for TUI eventually

The `wg config init`, `wg config lint`, and `wg migrate config` commands are designed so the TUI just shells out to them when ready. No TUI-specific config logic in this round. Reason: single source of truth for config rewrites is the audit table; TUI must not re-implement it.

---

## 8. Implementation checklist (for `implement-canonical-wg`)

1. `src/config.rs`: add `skip_serializing_if = "is_default_*"` to every default-having field on every `Config` substruct. (Audit §2 has the full list.)
2. `src/config.rs`: fix `Config::global_dir()` to mirror `main.rs::resolve_workgraph_dir` order (`~/.wg` → `~/.workgraph` fallback). (Audit §1 "Stale alert".)
3. `src/config.rs`: add `serde(alias = "chat_agent")` on `coordinator_agent` field, `serde(alias = "max_chats")` on `max_coordinators`. (Audit §3 "Naming inconsistencies".)
4. `src/cli.rs` + `src/commands/config_cmd.rs`: add `wg config init` subcommand with `--global`, `--local`, `--route`, `--bare`, `--force`. Keep `wg config --init` as deprecated alias.
5. `src/commands/config_cmd.rs`: implement `wg config lint` — walk merged config, flag deprecated/renamed/restated-default keys against the audit allowlist. (Audit §6 item 1.)
6. `src/cli.rs` + `src/commands/migrate.rs`: add `wg migrate config` subcommand with `--global`, `--local`, `--all`, `--dry-run`. Backup before write.
7. `src/commands/setup.rs`: add `--scope global|local|both`, surface `launcher_history::list_recent` in model/endpoint pickers, replace final-summary printer with delta-vs-builtin printer.
8. `src/config_defaults.rs:199-201`: replace `openrouter:anthropic/claude-sonnet-4` with `openrouter:anthropic/claude-sonnet-4-6`. (Audit §3 line 280.)
9. Tests: fixture-based — `wg config init --global --route X` produces a byte-exact match against `tests/fixtures/config-init-X.toml`. Smoke scenario: paste fixture into `~/.wg/config.toml` in a temp HOME, run `wg service start --max-agents 1`, assert no warnings.
10. Smoke gate: add `tests/smoke/scenarios/config-init-global-claude-cli.sh` that performs (9) and lists `implement-canonical-wg` in `owners` (per CLAUDE.md "Smoke gate").
11. Follow-up: `wg add 'TUI Settings tab — config view + edit + lint runner' --after implement-canonical-wg` per §7.

---

## 9. Open questions for the user

**None expected to block implementation.** The decisions in §1–§8 are committed. The following are *nice-to-have* clarifications the implementer may surface as `wg msg` if a corner case appears:

- Q: Should `wg migrate config` rewrite stale model strings without explicit confirmation, or always require `--yes`? (Tentative: require `--yes`, since model choice can be a user preference. The agent mid-implementation may discover this is too restrictive — if so, they should `wg msg` rather than decide.)
- Q: Do we want `wg config init --route nex-custom` to prompt for the URL (interactive) or refuse and require a flag? (Tentative: refuse. `init` is non-interactive by definition; if you need interaction, use `wg setup`.)

These are *the only two* that have a tentative-but-not-final answer. Everything else is a hard decision.

---

## 10. Citations to audit (for traceability)

Every key-scope decision in §1 cites a row in `docs/config-canonical.md` §2 (full key inventory). The audit's §3 stale-value list maps to §6's migration drop list. The audit's §4 minimal-global is the §3.1 example. The audit's §5 minimal-project is the §3.3 example. The audit's §6 migration plan is the basis for §6 of this doc (we adopt items 1, 2, 3, 4, 5).

If a reviewer disagrees with a scope assignment in §1, the resolution path is: open the audit row for that key and re-decide there. The audit is the source of truth; this design doc just commits to a UX over it.
