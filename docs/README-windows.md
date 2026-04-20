# Running workgraph on Windows

Workgraph runs natively on Windows (tested on Windows 11 ARM64; x86_64 via
emulation works too). This page is the "what's different" guide — if you've
already run workgraph on Linux or macOS, read it top-to-bottom once, then keep
it as a reference for gotchas.

## TL;DR

1. Install prerequisites: Rust, Git for Windows (bash + git), MSVC Build
   Tools, LLVM (`clang.exe`), CMake.
2. Clone the repo and run `cargo install --path . --force --locked` from a
   shell that has the MSVC env loaded (`VsDevCmd.bat -arch=arm64` or
   `-arch=x64`). For native ARM64 also set `OPENSSL_NO_ASM=YES`; see
   [the boring-sys2 note](#boring-sys2-and-native-arm64) below.
3. Configure authentication (one of):
   - `claude login` — writes `~/.claude/credentials.json`; the daemon picks
     it up automatically.
   - Headless: put your `sk-ant-oat01-…` token in a file and reference it
     from `.workgraph/config.toml` under `[auth]`. See
     [Auth](#authentication) below.
4. `wg init` in your project, then `wg service start` — daemon runs, agents
   dispatch, graph progresses.

## What works

| Area | Status |
|---|---|
| Task graph CLI (`wg init`, `add`, `list`, `show`, `done`, `artifact`, etc.) | ✅ |
| Service daemon (`start`, `status`, `stop`, `restart`, `reload`, `pause`, `resume`) | ✅ |
| IPC over Windows named pipes (`interprocess` crate) | ✅ |
| Coordinator agent spawn + clean interrupt via `GenerateConsoleCtrlEvent(CTRL_BREAK_EVENT)` | ✅ |
| Per-agent isolated git worktrees | ✅ |
| `wg` CLI from inside a worktree (directory junction instead of symlink — no Developer Mode needed) | ✅ |
| Key-file ACLs via `icacls` (instead of `chmod 600`) | ✅ |
| TUI (`wg tui`) | ✅ |

## What doesn't work yet

| Feature | Why | Workaround |
|---|---|---|
| `wg service freeze` / `thaw` | Uses `SIGSTOP` / `SIGCONT`. No clean Windows equivalent. `NtSuspendProcess` is the rough analog but hasn't been wired up. | Use `wg service pause` (doesn't stop running agents but blocks new spawns). |
| `wg service install` | Generates a systemd user unit file — Linux-only. | Use Windows Task Scheduler. See [Autostart](#autostart) below. |
| Native-executor `ANTHROPIC_API_KEY` with `sk-ant-oat01-…` tokens | The native Anthropic client uses the `x-api-key` header, which only accepts `sk-ant-api03-…` console keys. | Use the `claude` executor (the default) with `CLAUDE_CODE_OAUTH_TOKEN` in `[auth]`. See [Auth](#authentication). |

## Shell requirements

**Git for Windows bash is required.** The daemon shells out to `bash` to run
spawned task-agent wrapper scripts (`.workgraph/agents/<id>/run.sh`). The
bash binary is typically at `C:\Program Files\Git\usr\bin\bash.exe`; make
sure it's on your `PATH`. WSL bash won't work — it runs inside the WSL VFS
and can't see your Windows working tree the way Git for Windows' mingw
bash does.

The wrapper scripts also use GNU `timeout` (as a command wrapper) and
standard coreutils (`cat`, `tee`, `grep`); all of these ship in
`C:\Program Files\Git\usr\bin\`. Windows' own `TIMEOUT.EXE` is an
interactive wait utility, not a command wrapper — **do not** put
`C:\Windows\System32` ahead of Git's bin in PATH in the daemon's
environment.

## Authentication

The daemon spawns three kinds of `claude` subprocesses (coordinator,
lightweight LLM calls, task agents). All three need credentials. Your
options, in order of preference:

### Option A: `claude login` (recommended)

Run `claude login` once on the machine. It writes `~/.claude/credentials.json`
with a refreshable token. The CLI reads that file automatically — workgraph
doesn't need to know about your token at all. This is the normal path when
you're a Claude Code subscriber.

### Option B: headless OAuth token via `[auth]` in config.toml

If you can't run `claude login` (e.g., the daemon starts at boot before any
user is logged in), store a bare `sk-ant-oat01-…` token somewhere workgraph
can read and point `.workgraph/config.toml` at it:

```toml
[auth]
claude_code_oauth_token_file = "~/.config/workgraph/oauth-token"
```

The file should contain the token on a single line, with nothing else. Lock
it down:

```
icacls "%USERPROFILE%\.config\workgraph\oauth-token" /inheritance:r /grant "%USERNAME%:R"
```

(On Unix: `chmod 600 ~/.config/workgraph/oauth-token`.)

Inline form (`claude_code_oauth_token = "..."`) also works, but means the
token lives in `.workgraph/config.toml` — keep that file out of version
control if you use this form.

### Option C: env var (ad-hoc)

Export `CLAUDE_CODE_OAUTH_TOKEN=sk-ant-oat01-…` before `wg service start`.
The daemon propagates it to all child processes. Fine for interactive
development; doesn't survive reboot.

### The `ANTHROPIC_API_KEY` trap

Do **not** put `sk-ant-oat01-…` tokens in `ANTHROPIC_API_KEY`. The CLI
sends `ANTHROPIC_API_KEY` values as the `x-api-key` header — correct for
`sk-ant-api03-…` console keys but rejected by the server as invalid for
OAuth tokens. Symptom is a 401 on every Claude call and "Invalid API key"
showing up in outbox messages (an SDK-side synthetic placeholder, not a
real model response).

`ANTHROPIC_API_KEY` is the right variable only for keys you pulled from
[console.anthropic.com](https://console.anthropic.com/) (pay-per-token
billing), not for subscription OAuth tokens.

## Autostart

No Windows autostart is installed by default. Two options:

### Task Scheduler (recommended)

Create a scheduled task that runs `wg service start` at logon. A minimal
trigger+action XML can be imported via `schtasks /create /xml`. The task
should run **as the current user** (not SYSTEM) so it finds your
`~/.claude/credentials.json` or `[auth]`-configured token file. A Windows
port of `wg service install` that emits this XML is tracked — until it
lands, wire it up by hand.

### Windows Service (heavier, needs admin)

Wrap `wg service start` with [WinSW](https://github.com/winsw/winsw) or
`sc create` if you need it to start before login or run under LocalService.
Remember credentials need to live where that service account can read them.

## boring-sys2 and native ARM64

The `boring-sys2` crate (via `rquest`) pulls in a BoringSSL build that
requires a vendored patch on native ARM64. You need
`OPENSSL_NO_ASM=YES` set at build time, and a small tweak to
`~/.cargo/registry/src/index.crates.io-*/boring-sys2-4.15.15/build/main.rs`
that skips a native-ARM64 asm path. The escape hatch is to target
`x86_64-pc-windows-msvc` and let Windows' x64 emulation run the binary —
that build is clean.

## Extended-length paths (`\\?\C:\…`)

`PathBuf::canonicalize` on Windows returns paths in verbatim form
(`\\?\C:\src\…`). Most Windows APIs accept that, but two consumers don't:

- Git for Windows `bash.exe` can't open such a path passed as a command
  argument (`cat '\\?\C:\…'` fails with "No such file or directory") and
  can't use it as a redirect target.
- `git worktree add` bails trying to create leading directories because
  it renders the prefix as forward slashes (`//?/C:/…`).

Workgraph strips the `\\?\` prefix before handing paths to either. If you
see it in a log line, that's fine for diagnostic context; if you see it
on the *left* of a command failing with "No such file or directory",
file a bug — something missed the sanitizer.

## Performance notes

- The `wg` binary is ~45 MB. First-run startup is ~300 ms on a reasonable
  machine, dominated by env var + config parsing.
- The daemon keeps a long-running `claude` subprocess for the coordinator.
  The Anthropic prompt cache has a 5-minute TTL; with the default 60s
  heartbeat interval the cached ~10-15 k token block stays warm and each
  heartbeat turn is cheap (~1-3 k new cache tokens).
- Agent spawn takes ~2 s: create worktree → generate run.sh → start bash
  subprocess → bash starts claude CLI → claude loads tools.

## Troubleshooting

**`wg init` says "HOME environment variable not set"**
Fixed in #21. If you're on an older build, set
`HOME=%USERPROFILE%` in the env first.

**Daemon log full of `Invalid argument` on worktree create**
Fixed in #25. Symptom is also "falling back to shared working directory"
on every agent spawn.

**Agents dying immediately, empty `output.log`, daemon log shows nothing useful**
The `claude` CLI emits its synthetic error placeholders (including
"Invalid API key · Fix external API key") on stdout as model-less
"assistant" messages when auth fails. They look like real model output
but `model` is `"<synthetic>"` and `isApiErrorMessage` is `true`. Enable
#23 for better logging of CLI failures, and re-check your [auth
config](#authentication) — the most common cause is an `sk-ant-oat01-…`
token in `ANTHROPIC_API_KEY`.

**Coordinator stuck repeating a phrase every heartbeat**
Its conversation context has gotten into a local attractor. Stop the
service, archive `.workgraph/chat/0/chat.log` and `outbox.jsonl`, rewrite
`context-summary.md` with fresh accurate state (avoid quoting whatever
phrase it's stuck on — the model pattern-completes on salient strings in
its context), restart.
