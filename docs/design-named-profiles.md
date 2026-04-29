# Design: Named Profiles for Runtime Model/Endpoint Switching

**Task:** design-named-profiles
**Date:** 2026-04-28
**Status:** Proposed (commits to decisions; implementer follows §8 checklist)
**Implementation task:** implement-named-profiles
**Depends on:** [config-ux-design.md](config-ux-design.md) (canonical config UX)

---

## TL;DR

1. **Storage: option A — one file per profile** under `~/.wg/profiles/<name>.toml`. Each profile is a partial config that **overlays** onto base `~/.wg/config.toml`.
2. **Active pointer: `~/.wg/active-profile`** (one-line file containing the name). Absent file = no profile active = behave like today.
3. **Command surface extends the existing `wg profile`** subcommand. New verbs: `use`, `create`, `edit`, `delete`, `diff`, `init-starters`. Existing verbs `list`, `show`, `set`, `refresh` stay (with adjusted semantics, see §3).
4. **Hot-reload uses the existing `IpcRequest::Reconfigure`** that `wg config -m` already triggers. `wg profile use <name>` resolves the profile and sends the same IPC. No new IPC verb.
5. **Three starter profiles ship via `wg profile init-starters`** (also auto-invoked the first time `wg setup` finishes): `claude`, `codex`, `wgnext`. Templates baked into the binary (see §7).
6. **Switching semantics**: daemon picks up new profile on the next worker spawn. **In-flight workers keep their original config** (already true today for `wg config -m`). **Chat agents read profile name on startup and stay on that profile** for their lifetime — switching requires `wg chat <id> --restart` (already exists as `SetChatExecutor`).
7. **Migration**: existing `~/.wg/config.toml` stays as the implicit base. `wg profile use <name>` overlays; `wg profile use --clear` reverts to base. No destructive rewrite — your old config is untouched.
8. **`wg init --route codex-cli` no longer mutates global config.** It writes `~/.wg/profiles/codex.toml` and sets it active. (Resolves the open issue called out in the task description.)

---

## 1. Why this shape (and what we're NOT doing)

### What the user asked for

> "profiles that we can switch at runtime quickly... so we could have several profiles, I think, that are available to us, and make them easy to configure. So there should be claude, codex, and there should also now be wgnext, configured with localhost or something, but then we can just change the endpoint there."

Three concrete asks:
- **Bundle**: (model, endpoint, per-role overrides) move together as a unit.
- **Runtime switch**: one command flips the daemon to a new bundle without restart.
- **Easy edit per machine**: `wgnext`'s endpoint differs per host (laptop vs server), so per-machine override has to be a one-line edit.

### Reuse of what already exists

The codebase already has:
- A `wg profile` command (`src/commands/profile_cmd.rs`) — but it currently only switches **tier mappings**, not endpoints, not per-role models, and only from a hardcoded built-in list (`anthropic`, `codex`, `openrouter`, `openrouter-open`, `openai`).
- `IpcRequest::Reconfigure { max_agents, executor, poll_interval, model }` (`src/commands/service/ipc.rs:75`) — already triggered by `wg config -m` to hot-reload the daemon.
- A `Config::load_merged(dir)` that already does global+local layering (`src/config.rs`).
- `launcher_history.jsonl` for surfacing recent (model, endpoint) pairs in pickers.

This design **extends** the existing `wg profile` rather than introducing a new noun. The current `Profile` struct (tier preset, with static/dynamic strategies) becomes the **template** layer; named runtime profiles are the **instance** layer the user can create, edit, switch.

### What we are NOT building

- **No per-task profiles.** A task already has `model` and we're not changing that.
- **No profile inheritance / parent profile** in v1. Each profile is a flat overlay.
- **No env-var profiles** (export FOO=bar etc.) in v1 — profile contents are config keys only. (Listed as a v2 in §10.)
- **No daemon-restart-on-switch.** The whole point is hot-reload; if a switch needs a restart, that's a bug, not a feature.

---

## 2. Storage format (decision: A)

### 2.1 Decision

**Option A — one file per profile** at `~/.wg/profiles/<name>.toml`. Each file is a partial TOML that overlays onto base `~/.wg/config.toml`.

**Rejected: option B (`[profiles.<name>]` sections in main config).**
- Pros (B): single file, no extra paths to remember.
- Cons (B): main config grows linearly with profile count; `wg migrate config` (existing tool) would have to know about profile sections; sharing one profile means sharing the whole config.

**Rejected: option C (both).** Adds a second mental model for no real win — partial overrides under `[profiles.*]` AND full snapshots in files would mean two ways to express the same thing.

**Reason for A**: one file = one profile = trivially shareable / git-tracked / `cp`-able. The user can `scp ~/.wg/profiles/wgnext.toml otherhost:~/.wg/profiles/` and edit the endpoint locally without touching anything else.

### 2.2 Layout

```
~/.wg/
├── config.toml            # base config (unchanged)
├── active-profile         # one-line file: contains profile name, or absent
└── profiles/
    ├── claude.toml        # starter, written by `wg profile init-starters`
    ├── codex.toml         # starter
    ├── wgnext.toml        # starter
    └── <user-defined>.toml
```

**Active pointer is a separate file**, not a key in `config.toml`. Reason:
- Keeps `config.toml` byte-stable when switching profiles (clean diff for users who track it in dotfiles).
- One-line file is trivially readable from shell (`cat ~/.wg/active-profile`).
- Absence of file = unambiguous "no profile active" state.

### 2.3 Profile file schema (the allowlist)

A profile.toml is a **strict subset** of config.toml. Only these top-level keys may appear:

| Key | Why profile-able |
|-----|------------------|
| `description` (top-level string) | Profile docstring; shown by `wg profile list` |
| `[agent].model` | Worker default model |
| `[dispatcher].model` | Dispatcher's own LLM model (used for assignment etc.) |
| `[tiers].fast/standard/premium` | Tier→model mappings |
| `[models.default].model` | Fallback when neither task nor tier nor agent specifies |
| `[models.evaluator].model` | Agency evaluator (`.evaluate-*` tasks) |
| `[models.assigner].model` | Agency assigner (`.assign-*` tasks) |
| `[models.flip].model` | FLIP scorer (`.flip-*` tasks) |
| `[models.creator].model` | Agency creator |
| `[models.evolver].model` | Agency evolver |
| `[[llm_endpoints.endpoints]]` | Endpoint binding (replaces, not merged — see §2.4) |

**Everything else is REJECTED at load time** with a clear error: `"unknown profile field: [tui]; profiles control models and endpoints only"`. Reason: profiles are NOT a kitchen-sink override mechanism. User ergonomics (`[tui]`, `[help]`, `[viz]`), guardrails, agency identities, project metadata, MCP servers, tag routing all stay global. Allowing them in profiles invites confusion ("why did my TUI theme change when I switched profiles?").

The allowlist is implemented as a positive whitelist in `src/profile/named.rs` (new module, see §8). NOT `deny_unknown_fields` on the existing `Config` struct — that would break too much.

### 2.4 Overlay semantics: scalars merge, arrays replace

When the active profile is `codex`, the daemon's effective config is:
- For every scalar key in the profile: profile wins. (e.g., `agent.model`)
- For arrays (`[[llm_endpoints.endpoints]]`): profile **replaces** the entire array, not merges per-name.

Reason: merging endpoints by name would mean "the user has a stale endpoint in base config and profile only adds a new one" produces a mix of stale and current. Replacing the whole array is the simpler and safer rule. If a user wants to **keep** a base endpoint, they restate it in the profile.

For per-role models (`[models.evaluator].model` etc.), the existing cascade rules apply. Profile sets the value the cascade reads.

### 2.5 Example profile files

#### `~/.wg/profiles/claude.toml`

```toml
description = "Claude CLI: opus worker, haiku for agency meta-tasks"

[agent]
model = "claude:opus"

[dispatcher]
model = "claude:opus"

[tiers]
fast = "claude:haiku"
standard = "claude:sonnet"
premium = "claude:opus"

[models.evaluator]
model = "claude:haiku"

[models.assigner]
model = "claude:haiku"

[models.flip]
model = "claude:haiku"
```

(No `[[llm_endpoints.endpoints]]` — claude CLI handler authenticates itself, doesn't need an endpoint URL.)

#### `~/.wg/profiles/codex.toml`

```toml
description = "OpenAI Codex CLI: gpt-5.5 worker, gpt-5.4-mini for agency"

[agent]
model = "codex:gpt-5.5"

[dispatcher]
model = "codex:gpt-5.5"

[tiers]
fast = "codex:gpt-5.4-mini"
standard = "codex:gpt-5.4"
premium = "codex:gpt-5.5"

[models.evaluator]
model = "codex:gpt-5.4-mini"

[models.assigner]
model = "codex:gpt-5.4-mini"

[models.flip]
model = "codex:gpt-5.4-mini"
```

(Same — codex CLI is local, no endpoint needed.)

#### `~/.wg/profiles/wgnext.toml`

```toml
description = "wg-next: in-process nex handler at a localhost endpoint (edit URL per machine)"

[agent]
model = "local:qwen3-coder-30b"

[dispatcher]
model = "local:qwen3-coder-30b"

[tiers]
fast = "local:qwen3-coder-30b"
standard = "local:qwen3-coder-30b"
premium = "local:qwen3-coder-30b"

# NOTE: agency tasks (.evaluate-*, .flip-*, .assign-*) are pinned to claude:haiku
# by design (see CLAUDE.md "Agency tasks run on claude CLI"). Override here only
# if you have a *better* local model for these short calls.

[[llm_endpoints.endpoints]]
name = "default"
provider = "oai-compat"
url = "http://127.0.0.1:8088"
api_key_env = ""
is_default = true
```

This is the profile users edit per-machine (`wg profile edit wgnext` to change the URL).

---

## 3. Command surface

The full verb list. All commands operate on `~/.wg/profiles/` and `~/.wg/active-profile`. None of these touch `~/.wg/config.toml`.

### 3.1 New verbs

#### `wg profile use <name>`
- Read `~/.wg/profiles/<name>.toml`. Error if missing with "Did you mean: <closest match>?"
- Validate against the allowlist (§2.3).
- Write `<name>` to `~/.wg/active-profile`.
- If a daemon is running (`~/.wg/service/state.json` shows pid alive), send `IpcRequest::Reconfigure` with the resolved (model, endpoint, etc.) — same path `wg config -m` uses today (`src/commands/config_cmd.rs:434` reference).
- Print: `"Active profile: codex (was: claude). Daemon reloaded — next worker will use codex models."` (or `"Daemon not running — change applies on next start."`)

Flag: `--no-reload` (skip the IPC; just write the active pointer). Mirrors `wg config -m --no-reload`.
Flag: `--clear` (remove active pointer, revert to base config). `wg profile use --clear` unsets without picking another profile.

#### `wg profile create <name> [-m MODEL] [-e ENDPOINT] [--from <existing>] [--description STR]`
- Build a new profile file. With `--from <existing>`, copy the existing profile as starting point. Without `--from`, start from a minimal template (just `agent.model`).
- Refuse if `~/.wg/profiles/<name>.toml` already exists, unless `--force`.
- After write, do NOT auto-`use` it. User runs `wg profile use <name>` separately. Reason: create-and-switch is two distinct decisions; conflating them is the same kind of footgun `wg add` avoids.
- `wg profile create wgnext -m local:llama3 -e http://127.0.0.1:8088` produces a working wgnext-style profile.

#### `wg profile edit <name>`
- Open `~/.wg/profiles/<name>.toml` in `$EDITOR` (default `vi`).
- After save, validate the file. If invalid, leave the file (don't silently corrupt) and print the error.
- If `<name>` is the active profile and a daemon is running, send `IpcRequest::Reconfigure` automatically (same as `use`).

Flag: `--no-reload` to skip the IPC.

#### `wg profile delete <name>`
- Refuse if `<name>` is the active profile, unless `--force`.
- Remove `~/.wg/profiles/<name>.toml`.
- If `--force` and was active, also clear `~/.wg/active-profile` and warn the user that the daemon is now back on base config.

#### `wg profile diff <a> [<b>]`
- Two-form: `wg profile diff codex` shows base-config-vs-codex (one arg = compare base to that profile).
- Two-form: `wg profile diff claude codex` shows claude-vs-codex.
- Output format: unified TOML diff (similar to `git diff`). Per-key, marked `-` (removed in b) / `+` (added in b) / `~` (changed in b).
- Reason: when a user has 5+ profiles, "what's actually different between these two" is a real question.

#### `wg profile init-starters [--force]`
- Write the three starters (`claude`, `codex`, `wgnext`) to `~/.wg/profiles/` if missing.
- `--force` overwrites existing starters (e.g., to pick up upstream model-string updates).
- Auto-invoked at the end of `wg setup` if `~/.wg/profiles/` is empty.

### 3.2 Adjusted verbs

#### `wg profile list`
- Today: lists hardcoded built-in profiles.
- After: lists files in `~/.wg/profiles/` AND the built-in templates (clearly marked `[builtin]` vs `[user]`). Marks the active one with `*`.
- New flag `--installed` filters to only files in `~/.wg/profiles/`.

#### `wg profile show [<name>]`
- Today: shows currently-set profile and effective tier mappings.
- After: with no arg, shows the active profile's full resolved config (the merged-with-base view). With an arg, shows that profile's contents.
- New flag `--diff-base` to also show what changes vs base config.

#### `wg profile set <name>`
- **Deprecated alias for `wg profile use <name>`.** Stays for one release with a stderr warning. Reason: "set" suggests "set the value of"; "use" matches Python venv / nvm / rbenv vocabulary which is closer to what users expect. Removing in next major.

#### `wg profile refresh`
- Unchanged (refreshes OpenRouter benchmarks; orthogonal to named profiles).

### 3.3 What `wg setup` and `wg init` do now

- `wg setup` interactive flow ends with: "Save these choices as a named profile? [y/N]". If yes, prompts for the profile name (default: route name, e.g., `claude-cli` → `claude`). Writes `~/.wg/profiles/<name>.toml` and offers to `use` it.
- `wg init --route codex-cli` (currently writes to global config): rewires to write `~/.wg/profiles/codex.toml` and set it active. Old `~/.wg/config.toml` is left intact. (Resolves the open issue noted in the task description.)
- First-run `wg setup` (no existing config) auto-invokes `wg profile init-starters` so the three starters always exist out of the box.

---

## 4. Switching semantics (the hot-reload contract)

### 4.1 Daemon (already-running)

`wg profile use <name>` translates to `IpcRequest::Reconfigure` with the profile's resolved values. The daemon's existing reconfigure handler (`src/commands/service/ipc.rs:453`) already accepts `model`, and we extend it for the new fields:

```rust
IpcRequest::Reconfigure {
    max_agents: Option<usize>,
    executor: Option<String>,
    poll_interval: Option<u64>,
    model: Option<String>,
    // NEW (v2 of this IPC):
    profile: Option<String>,           // for telemetry / audit log only
    endpoint_default_url: Option<String>,  // applies to the named "default" endpoint
    role_models: Option<RoleModelOverrides>,  // evaluator/assigner/flip
}
```

The daemon applies these to its in-memory config and persists nothing (in-memory only). The active-pointer file is the persistent source of truth — a daemon restart re-reads it.

**Backward compat**: old `wg config -m` calls send only `model`. Daemon reconfigure path treats unset Option fields as no-op (already does). Old IPC clients keep working.

### 4.2 In-flight workers

**No effect.** A worker spawned at T0 with profile `claude` finishes the task with claude config, even if at T0+5s the user runs `wg profile use codex`. Reason:
- Worker has already authenticated, set up worktree, started LLM stream — yanking config mid-flight is a category of bug we don't want.
- This already matches `wg config -m` behavior today.
- The graph-level cost is low: the next worker the dispatcher spawns picks up the new profile.

For an aggressive user who wants in-flights killed on switch: `wg profile use <name> --kill-in-flight` is a v2 nice-to-have, NOT shipping in v1.

### 4.3 Chat agents

A chat agent is a long-lived LLM session in a separate process (TUI tab or `wg chat ...`). When it starts, it reads `~/.wg/active-profile` and binds to that profile.

**Decision: chat agents pin to their startup profile for their lifetime.** Switching the active profile does NOT affect existing chat agents. Reason:
- Multiple chat agents may be running concurrently in different terminals (the user even quotes "we have several profiles available"). Mixing-and-matching profiles per chat is the natural use case.
- A chat mid-conversation has model-specific context (token budget, prior turns). Re-binding to a different model mid-conversation could exceed context windows or break tool-use formats.
- The user can already switch a chat's executor per-instance via `IpcRequest::SetChatExecutor` (`src/commands/service/ipc.rs:154`), which terminates and respawns the chat handler. Profile-switching for a single chat reuses this path: `wg chat <id> --profile codex` calls `SetChatExecutor` with the codex-resolved model.

**Implication**: a chat agent's identity gains a `profile` field in `CoordinatorState` (one-time write at create-time). New chat-create dialogs in the TUI gain a profile picker (default: current active profile).

### 4.4 Summary table

| Actor | What sees the new profile? |
|-------|---------------------------|
| Daemon (dispatcher) | Yes, immediately via IPC reconfigure |
| Workers in-flight | No — they finish on their original config |
| Workers spawned after switch | Yes — daemon resolves profile at spawn time |
| Chat agents already running | No — they keep their startup profile |
| Chat agents created after switch | Yes — pick up the active profile at create time |
| `wg add` from a new shell | Yes — reads merged config which is base + active profile |

---

## 5. Migration: existing users

### 5.1 First-run on a machine with existing `~/.wg/config.toml`

Running `wg profile use claude` (or any other profile) for the first time on a machine that has a populated `~/.wg/config.toml`:
- Detects the existing config.
- Computes which keys it would override (the overlap of profile.toml's keys with config.toml's keys).
- Prints a one-time warning:
  ```
  Note: ~/.wg/config.toml currently sets agent.model = "openrouter:..."
        Active profile 'claude' overrides this with claude:opus.
        Run `wg profile show --diff-base` to see all overrides.
        To clear the active profile and revert to base, run `wg profile use --clear`.
  ```
- Continues with the switch.

**No destructive rewrite.** The user's hand-curated `~/.wg/config.toml` is sacred. Profiles are layered on top.

### 5.2 Existing v1 `wg profile` users (current `set`)

Today `wg profile set anthropic` writes `profile = "anthropic"` to config and sets `[tiers]` accordingly. This path stays working but emits a one-release deprecation:
- `wg profile set` works as an alias for `wg profile use` (see §3.2).
- The hardcoded built-in profiles (anthropic, openrouter, openai, openrouter-open, codex) become **templates** that `wg profile init-starters` and `wg profile create --from <builtin>` can materialize. They are no longer reachable via `wg profile use` directly — the user must materialize first.
- One exception: `openrouter` (the dynamic profile) stays as a "template" that resolves at create-time by snapshotting current rankings into a profile file. Subsequent `wg profile refresh` updates the file.

### 5.3 No automatic migration of current `[tiers]` etc.

We do NOT auto-extract the user's current `~/.wg/config.toml` into a "default" or "current" profile. Reason: most users have a working config they understand; auto-extraction creates a phantom profile they didn't ask for. If they want one, `wg profile create my-current --from-config` is the explicit path (v2 — not in scope for v1).

For v1: existing users keep their config exactly as is. They opt into profiles only when they `wg profile init-starters` or `wg profile use <name>`.

---

## 6. Validation criteria → smoke scenarios

The implementation task's smoke gate (`tests/smoke/scenarios/`) MUST include at least these scenarios. Each is a script invoked by `tests/smoke/manifest.toml` with `owners = ["implement-named-profiles"]`:

### 6.1 `profile-create-and-list.sh`
1. Fresh `$HOME` (temp).
2. `wg profile create test1 -m claude:opus`
3. Assert: `~/.wg/profiles/test1.toml` exists.
4. Assert: `wg profile list` output contains `test1` and marks it `[user]`.
5. Assert: `wg profile show test1` prints `agent.model = "claude:opus"`.

### 6.2 `profile-use-without-daemon.sh`
1. Fresh `$HOME` with no daemon running.
2. `wg profile init-starters`
3. `wg profile use codex`
4. Assert: `cat ~/.wg/active-profile` prints `codex`.
5. Assert: `wg config show --merged` reports `agent.model = "codex:gpt-5.5"`.
6. `wg profile use --clear`
7. Assert: `~/.wg/active-profile` does not exist; `wg config show --merged` reports the base config's model.

### 6.3 `profile-use-hotreload.sh`
1. Fresh `$HOME`. `wg profile init-starters`. `wg profile use claude`. `wg service start --max-agents 1` (background).
2. Assert: daemon log line "Reconfigured: ... model=claude:opus" appeared on first startup (already true today).
3. `wg profile use codex`
4. Assert: daemon log line "IPC Reconfigure: ... model=codex:gpt-5.5" appeared within 2s.
5. Add a task: `wg add 'Trivial: print hello'`.
6. Wait for spawn. Assert: spawned agent's `WG_MODEL` env var is `codex:gpt-5.5` (assert via `wg agents --json`).
7. `wg service stop`.

### 6.4 `profile-edit-applies.sh`
1. Fresh `$HOME`. `wg profile init-starters`. `wg profile use wgnext`. `wg service start --max-agents 1` (background).
2. Programmatically rewrite `~/.wg/profiles/wgnext.toml` to change endpoint URL from `http://127.0.0.1:8088` to `http://127.0.0.1:9999` (simulate `wg profile edit`).
3. Run `wg profile use wgnext` (re-issue same name) — this is the explicit "I edited, please reload" path.
4. Assert: daemon log shows reconfigure with the new URL.

### 6.5 `profile-diff.sh`
1. `wg profile diff claude codex` outputs a diff containing both `-claude:opus` and `+codex:gpt-5.5` (in their respective `agent.model` lines).

### 6.6 `profile-allowlist-rejects-tui.sh`
1. Manually write `~/.wg/profiles/badprof.toml` with a `[tui]` section.
2. Assert: `wg profile use badprof` exits non-zero with a message naming `[tui]` as the rejected key.

### 6.7 SKIP-allowed scenarios
- The hot-reload scenario MAY emit a loud SKIP (exit 77) if the LLM endpoint is unreachable, since the daemon may refuse to start. The test should NOT silently pass — the smoke gate's loud-skip rule applies.

---

## 7. Starter profile contents (final)

The three starters, baked into the binary as `include_str!` templates and written by `wg profile init-starters`:

### 7.1 `claude.toml`
See §2.5 example. Mirrors `wg init --route claude-cli` output minus the daemon-tuning keys.

### 7.2 `codex.toml`
See §2.5 example. Model strings cross-checked against `docs/config-ux-design.md` §3.2b (codex CLI v0.124.0 mapping):
- worker: `codex:gpt-5.5` (premium tier — newest frontier, per bump-codex-defaults 2026-04-28)
- standard: `codex:gpt-5.4` (sonnet-equivalent)
- premium: `codex:gpt-5.5`
- agency / fast / FLIP: `codex:gpt-5.4-mini`

### 7.3 `wgnext.toml`
See §2.5 example. Defaults to `local:qwen3-coder-30b` at `http://127.0.0.1:8088`. The user is expected to edit both fields to match their local nex setup.

**Per-machine workflow:**
```bash
# On a fresh machine:
wg profile init-starters
wg profile edit wgnext      # change url to http://desktop.local:30000 etc.
wg profile use wgnext
```

---

## 8. Implementation checklist (for `implement-named-profiles`)

In recommended order. Each item is a discrete commit.

1. **`src/profile/mod.rs` reorganize.** Move existing `src/profile.rs` to `src/profile/template.rs` (preserves `Profile` / `ProfileStrategy` / `escalate_model`). Add `src/profile/named.rs` for the new file-based profiles. Re-export both from `src/profile/mod.rs`.

2. **`src/profile/named.rs`: file I/O + schema.**
   - `pub struct NamedProfile { description: Option<String>, agent_model: Option<String>, dispatcher_model: Option<String>, tiers: TierConfig, models: HashMap<String, String>, endpoints: Option<Vec<EndpointConfig>> }`.
   - `serde(deny_unknown_fields)` ON THIS STRUCT (positive allowlist — see §2.3).
   - `pub fn load(name: &str) -> Result<NamedProfile>` — reads `~/.wg/profiles/<name>.toml`.
   - `pub fn save(name: &str, prof: &NamedProfile) -> Result<()>` — writes (with backup of existing).
   - `pub fn list_installed() -> Result<Vec<String>>` — lists files in `~/.wg/profiles/`.
   - `pub fn active() -> Result<Option<String>>` — reads `~/.wg/active-profile`.
   - `pub fn set_active(name: Option<&str>) -> Result<()>` — writes or removes the file.
   - `pub fn overlay_onto(base: &mut Config, prof: &NamedProfile)` — applies overlay rules (§2.4).

3. **`src/config.rs`: integrate overlay into `Config::load_merged`.**
   - After the existing global+local merge, check `profile::named::active()`. If `Some(name)`, load and overlay before returning.
   - Add `Config::base_only(dir) -> Config` for `wg profile show --diff-base` (skips the overlay).
   - Tests: round-trip — profile + base → merged → overlay's keys win.

4. **`src/cli.rs`: extend `Profile` subcommand.**
   - Add `Use { name: Option<String>, no_reload: bool, clear: bool }`.
   - Add `Create { name: String, model: Option<String>, endpoint: Option<String>, from: Option<String>, description: Option<String>, force: bool }`.
   - Add `Edit { name: String, no_reload: bool }`.
   - Add `Delete { name: String, force: bool }`.
   - Add `Diff { a: String, b: Option<String> }`.
   - Add `InitStarters { force: bool }`.
   - Adjust `List` to show installed + builtin.
   - Adjust `Show` to default to active profile, accept optional `name`, add `--diff-base`.
   - Keep `Set` as deprecated alias for `Use`.

5. **`src/commands/profile_cmd.rs`: implement the new verbs.**
   - Reuse `service::ipc::send_request` for the reconfigure call.
   - The reconfigure call needs `model` filled (already supported) and the new optional `endpoint_default_url` / `role_models` fields once the IPC is extended (step 6).

6. **`src/commands/service/ipc.rs`: extend `IpcRequest::Reconfigure`.**
   - Add `#[serde(default)] profile: Option<String>` (telemetry only; daemon prints it in the reconfigure log).
   - Add `#[serde(default)] endpoint_default_url: Option<String>` (applies to the endpoint named `default` in the in-memory config).
   - Add `#[serde(default)] role_models: Option<RoleModelOverrides>` where `RoleModelOverrides { evaluator: Option<String>, assigner: Option<String>, flip: Option<String>, ... }`.
   - Update the reconfigure handler (line 453) to apply each field when present.
   - All new fields are `Option<>` with `serde(default)` so old clients (workers, scripts) keep sending the old shape and pass schema validation.

7. **`src/commands/setup.rs`: post-route hook.**
   - At the end of the existing route flow (after writing config), prompt: `Save as named profile?`. If yes, write `~/.wg/profiles/<name>.toml` containing only the route's contributed keys, and offer to `wg profile use` it.
   - On first-run (no `~/.wg/profiles/` directory) auto-call `wg profile init-starters`.

8. **`src/commands/init.rs`: `--route codex-cli` rewrite.**
   - Today this mutates `~/.wg/config.toml`. Replace with: write `~/.wg/profiles/codex.toml` from the codex template; if no active profile yet, set `codex` as active.
   - Same for `--route claude-cli` → `claude` profile, `--route nex-custom` → user-named (prompt) profile.

9. **Starter templates.**
   - `src/profile/templates/claude.toml`, `codex.toml`, `wgnext.toml` (literal files).
   - `pub fn starter_template(name: &str) -> Option<&'static str>` returns `include_str!` content.
   - `init-starters` writes each template to `~/.wg/profiles/<name>.toml` if missing.

10. **Tests.**
    - Unit: `NamedProfile` deny-unknown-fields rejects `[tui]`.
    - Unit: overlay merges scalars, replaces endpoint arrays.
    - Integration: `wg profile create / use / show / diff / delete` round-trip in a temp HOME.
    - Integration: hot-reload via running daemon — see §6.3.

11. **Smoke scenarios (`tests/smoke/scenarios/profile-*.sh`).** All seven scenarios from §6.

12. **Documentation.**
    - Update `docs/COMMANDS.md` `wg profile` section with the new verbs.
    - Update `docs/AGENT-GUIDE.md` (or AGENT-SERVICE.md) to mention named profiles where it currently mentions `wg config -m`.
    - Add a one-paragraph blurb to `docs/config-ux-design.md` linking to this design (the canonical config UX doc).
    - Update `wg quickstart` output to mention `wg profile init-starters` as a first-run step.

13. **`cargo install --path .`** before claiming done, per CLAUDE.md.

---

## 9. Open questions

These are NOT blocking — listed so the implementer can `wg msg` if they hit a corner case.

### Q1: Should `wg profile use codex` warn if a daemon is using a different profile across multiple chat agents?
A chat agent running on `claude` profile is unaffected by switching the active profile to `codex`. But the user's mental model may be "I'm on codex now everywhere." Should `use` print a one-line note about which chat agents are still on the previous profile? **Tentative: yes**, but only at warning-level if there are >0 chat agents bound to a different profile. Implementer judges UX cost.

### Q2: Profile inheritance / parent profile.
Some users will want `production = base + extra-careful-eval-models`. v1 says no inheritance — flat overlays only. v2 could add `[inherits = ["base"]]` at the top of a profile. NOT in scope for v1.

### Q3: `wg profile use` from inside a project with `.wg/config.toml`.
Project-local config is currently strictly more specific than global. If global has profile A active and project local sets `agent.model = X`, what wins? **Decision: project-local wins** (matches existing precedence). Profile is part of the global-layer compute, then project-local overlays on top of that. Document this clearly in `wg profile show`'s output: `"Note: project-local config in ./.wg/config.toml overrides this profile's [agent].model"`.

### Q4: Endpoint identity matching.
When a profile's `[[llm_endpoints.endpoints]]` array has `name = "default"`, does it replace the base config's `default` endpoint or add a new one? **Decision: replace by name** (matches the existing endpoint resolution which keys on name). Document this. If a user wants two `default` endpoints they're already in trouble in the current code.

---

## 10. Future extensions (deferred — NOT in v1)

- **Env var profiles**: `[env]` section in profile.toml exported to spawned handlers. Useful for `OPENROUTER_API_KEY` rotation.
- **Profile inheritance**: see Q2.
- **Per-task profile**: `wg add ... --profile codex` would override the active profile for that task only. Possible but the per-task `--model` already covers most cases.
- **TUI profile switcher**: dropdown in the TUI status bar to switch profiles without dropping to shell. Builds on the CLI primitives — a clear v2.
- **`wg profile use --kill-in-flight`**: aggressive switch. See §4.2.
- **`wg profile create --from-config`**: snapshot current config into a profile. See §5.3.

These are listed for traceability so the implementer doesn't accidentally build them in v1.

---

## 11. Citations

- Existing tier-preset profiles: `src/profile.rs` (renamed to `src/profile/template.rs` in §8.1).
- Existing IPC reconfigure: `src/commands/service/ipc.rs:75` (Reconfigure variant), :453 (handler), :1092 (log line format).
- `wg config -m` hot-reload reference: `wg config --help` output documents `--no-reload` flag (default reloads).
- Canonical config UX (model strings, route definitions): [`docs/config-ux-design.md`](config-ux-design.md), specifically §3.1 (claude-cli), §3.2b (codex-cli).
- CLAUDE.md "Agency tasks run on claude CLI": justifies pinning `[models.evaluator]` etc. to a cheap fast model in every starter (claude:haiku for claude profile, codex:gpt-5.4-mini for codex profile, claude:haiku as the documented fallback for wgnext profile).
- Chat-executor hot-swap: `src/commands/service/ipc.rs:154` (`SetChatExecutor`) — basis for §4.3 per-chat profile switching.
