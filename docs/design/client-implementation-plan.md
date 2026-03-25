# Work Plan: Web & Mobile Terminal Clients

**Task:** mu-plan-clients
**Date:** 2026-03-25
**Depends on:** [Unified Multi-User Platform Architecture](multi-user-platform-architecture.md)

---

## Design Principle

**The TUI IS the interface** (ADR-S6). All platforms connect to the same `wg tui` running server-side via terminal wrapping. No platform-specific frontends. This means every client task is about the *connection layer*, not the application layer.

---

## User Journeys

### Web — First-Time User

1. User receives URL: `https://wg.example.com`
2. Browser presents OAuth2 login (or basic auth for simple deployments)
3. After auth, xterm.js terminal appears with `wg tui` already running inside tmux
4. User works. Closes browser tab → tmux session persists
5. User reopens URL → reattaches to existing session, state intact
6. Optional: "Add to Home Screen" → PWA launches without browser chrome

### Android — First-Time User

1. User installs Termux from F-Droid (Google Play version is unmaintained)
2. Runs one-time setup: `curl -sL wg.example.com/setup/termux | bash`
3. Script installs mosh, tmux, openssh; writes `~/.shortcuts/workgraph` with server address
4. User taps "workgraph" shortcut (via Termux:Widget) or types `workgraph` in Termux
5. mosh connects → tmux attach → TUI appears
6. Phone sleeps / switches to Wi-Fi → mosh reconnects transparently

### iOS — First-Time User

1. User installs Blink Shell ($15.99) from App Store
2. In Blink, adds host: `mosh user@server -- tmux new-session -A -s $USER-wg "wg tui"`
3. Taps the saved host → TUI appears
4. App backgrounded → mosh-server persists → foregrounded → ~2s reconnect
5. Alternative: use web URL (wg.example.com) in Safari for free, mosh-less access

### Desktop — First-Time User (baseline)

1. `ssh user@server` or `mosh user@server`
2. `tmux new-session -A -s $USER-wg "wg tui"`
3. Done. This already works today.

---

## Implementation Tasks

### Task 1: Responsive TUI Breakpoints

- **Platform:** Cross-platform (benefits all clients, required for mobile)
- **Decision:** BUILD — modify existing TUI code
- **Complexity:** M
- **Description:** Implement responsive layout breakpoints in the TUI renderer:
  - **< 50 cols:** Single-panel mode — show graph OR detail, not both. Tab/key to switch.
  - **50–80 cols:** Narrow split — graph list (left), compact detail (right). Hide non-essential columns.
  - **> 80 cols:** Current full layout (no change).
  - Detect terminal resize events (`SIGWINCH`) and switch layouts dynamically.
- **Files:** `src/tui/viz_viewer/render.rs`, `src/tui/viz_viewer/state.rs`
- **Dependencies:** None (can start immediately)
- **Test strategy:**
  - Unit tests: render to a virtual terminal at various widths (40, 60, 100 cols), assert layout invariants
  - Manual: run `wg tui` in terminals resized to phone-like dimensions (40x25, 50x30)
  - Termux validation: screenshot tests on a real Android device or emulator
- **Verify:** `cargo test test_responsive_ passes; TUI renders without panic at 40-col width`

### Task 2: Single-Panel Navigation Mode

- **Platform:** Cross-platform (critical for mobile)
- **Decision:** BUILD — new navigation logic
- **Complexity:** M
- **Description:** When in single-panel mode (< 50 cols), implement panel-switching navigation:
  - Key binding (e.g., Tab or `]`/`[`) to cycle between: graph list → task detail → log/output → back to graph list
  - Breadcrumb or header indicator showing current panel
  - All existing keybindings work within each panel
  - Panel state persists across switches (cursor position, scroll offset)
- **Files:** `src/tui/viz_viewer/state.rs`, `src/tui/viz_viewer/render.rs`
- **Dependencies:** Task 1 (breakpoint detection)
- **Test strategy:**
  - Unit tests: simulate key events in single-panel mode, verify panel transitions
  - Manual: use in 40-col terminal, verify all panels reachable and functional
- **Verify:** `cargo test test_single_panel_ passes`

### Task 3: ttyd Deployment Guide + Configuration

- **Platform:** Web
- **Decision:** INTEGRATE — ttyd + Caddy, zero workgraph code changes
- **Complexity:** S
- **Description:** Write deployment documentation for web access:
  - Minimal setup (LAN, no auth): single `ttyd` command
  - Production setup: Caddy reverse proxy + TLS + basic auth
  - OAuth2 setup: Caddy + OAuth2 Proxy → GitHub/Google provider
  - Multi-user: ttyd session management (one tmux session per authenticated user)
  - Systemd unit files for ttyd and Caddy
  - Troubleshooting: xterm.js rendering quirks, WebSocket timeout tuning
- **Files:** `docs/guides/web-access.md`, example `Caddyfile`, example systemd units
- **Dependencies:** None
- **Test strategy:**
  - Follow the guide on a fresh VPS (Ubuntu 24.04). Verify `wg tui` loads in Chrome, Firefox, Safari.
  - Test reconnection: close tab, reopen → tmux session reattaches
  - Test auth: verify unauthenticated requests are rejected

### Task 4: PWA Manifest + Service Worker

- **Platform:** Web (mobile browsers)
- **Decision:** BUILD — small web assets, no Rust changes
- **Complexity:** S
- **Description:** Create PWA assets for "Add to Home Screen" on mobile:
  - `manifest.json` with `"display": "standalone"`, app name, theme color, icons
  - App icons at required sizes (192x192, 512x512)
  - Minimal service worker: cache the shell, show "Reconnecting..." when offline
  - Instructions for hosting alongside ttyd (Caddy serves static assets)
  - This is cosmetic — the terminal session is still server-side
- **Files:** `docs/guides/web-access.md` (addendum), `assets/pwa/manifest.json`, `assets/pwa/sw.js`, `assets/pwa/icons/`
- **Dependencies:** Task 3 (deployment guide exists first)
- **Test strategy:**
  - Test "Add to Home Screen" on Android Chrome and iOS Safari
  - Verify standalone mode launches without browser chrome
  - Verify offline screen appears when server unreachable

### Task 5: xterm.js TUI Rendering Validation

- **Platform:** Web
- **Decision:** BUILD — may require TUI rendering adjustments
- **Complexity:** S
- **Description:** Systematically test the TUI in xterm.js (ttyd's terminal emulator) and fix rendering issues:
  - Color rendering (true color, 256 color)
  - Box-drawing characters and Unicode
  - Mouse support (click, scroll)
  - Keyboard shortcuts (Ctrl combinations, function keys)
  - Resize behavior
  - Document any necessary `TERM` or `COLORTERM` env var settings
  - File bugs / fixes for any rendering differences vs native terminal
- **Files:** Potentially `src/tui/` files if rendering fixes needed; `docs/guides/web-access.md` (known issues section)
- **Dependencies:** Task 3 (need ttyd deployed to test)
- **Test strategy:**
  - Visual comparison: screenshot native terminal vs xterm.js at same size
  - Automated: ttyd supports headless mode; could script Playwright to take screenshots
  - Test on Chrome, Firefox, Safari — xterm.js rendering varies slightly

### Task 6: Termux Setup Script + Guide

- **Platform:** Android
- **Decision:** INTEGRATE — Termux + mosh, we provide setup automation
- **Complexity:** S
- **Description:** Create the Android onboarding experience:
  - `wg-termux-setup.sh`: installs mosh, tmux, openssh; creates `~/.shortcuts/workgraph`
  - Guide: F-Droid install instructions (NOT Google Play — the Play version is outdated)
  - Guide: Termux:Widget setup for home screen shortcut
  - Guide: Termux:Styling for font/color customization
  - Connection template: `mosh user@server -- tmux new-session -A -s $USER-wg "wg tui"`
  - Troubleshooting: storage permissions, battery optimization whitelist, SSH key generation
- **Files:** `scripts/wg-termux-setup.sh`, `docs/guides/android-access.md`
- **Dependencies:** None (Termux + mosh works today)
- **Test strategy:**
  - Run setup script on fresh Termux install (real device or Android emulator)
  - Verify shortcut launches and connects
  - Test mosh reconnection: toggle airplane mode on/off
  - Test with responsive TUI (Task 1) at phone screen sizes

### Task 7: Blink Shell Configuration Guide

- **Platform:** iOS
- **Decision:** INTEGRATE — Blink Shell, we provide configuration docs
- **Complexity:** S
- **Description:** Create iOS onboarding experience:
  - Step-by-step Blink Shell host configuration (mosh + tmux command)
  - SSH key setup (Blink generates keys, copy pubkey to server)
  - Recommended Blink settings for TUI (font, theme, keyboard shortcuts)
  - Document iOS background limitations and mosh reconnection behavior
  - Alternative: web access via Safari (for users who don't want to pay for Blink)
  - Brief mention of iSH (free but slower due to x86 emulation)
- **Files:** `docs/guides/ios-access.md`
- **Dependencies:** None
- **Test strategy:**
  - Follow guide on a real iOS device with Blink Shell
  - Test reconnection: background app for 5 minutes, foreground, verify reconnect
  - Test at iPhone screen sizes (375x667 logical → ~45x25 terminal)

### Task 8: Server-Side Connection Dispatcher

- **Platform:** Cross-platform (server)
- **Decision:** BUILD — shell script or small binary
- **Complexity:** S
- **Description:** Create a connection dispatcher script that all transports use:
  - `wg-connect.sh`: determines `WG_USER` from SSH user / ttyd auth / env var, then runs `tmux new-session -A -s "${WG_USER:-$USER}-wg" "wg tui"`
  - Ensures consistent session naming across all platforms
  - Handles first-run: if `wg` binary not found, prints setup instructions
  - Optional: creates `~/.workgraph-user` config for per-user coordinator settings
  - Used by: ttyd launch command, SSH `ForceCommand`, mosh connection command
- **Files:** `scripts/wg-connect.sh`, referenced in all platform guides
- **Dependencies:** None
- **Test strategy:**
  - Test with SSH, mosh, and ttyd — all should land in consistent tmux session
  - Test idempotency: run twice → attaches to existing session, doesn't create duplicate

### Task 9: mosh Server Configuration Guide

- **Platform:** Cross-platform (server)
- **Decision:** INTEGRATE — mosh-server, we provide configuration docs
- **Complexity:** S
- **Description:** Document server-side mosh setup:
  - Install mosh-server on the shared VPS
  - Firewall: open UDP 60000-61000
  - Systemd configuration for mosh (if needed beyond default)
  - Performance tuning: `MOSH_PREDICTION_DISPLAY` settings
  - Security: mosh uses AES-128-OCB, document the security model
  - Integration with `wg-connect.sh` (Task 8)
- **Files:** `docs/guides/server-setup.md` (new or addendum to existing)
- **Dependencies:** None
- **Test strategy:**
  - Test from each platform (desktop, Termux, Blink Shell)
  - Test network resilience: kill network for 30s, verify reconnection

### Task 10: Distribution & Hosting Strategy

- **Platform:** Cross-platform
- **Decision:** INTEGRATE — leverage existing distribution channels
- **Complexity:** S
- **Description:** Document how users get access to workgraph on each platform:

  | Platform | Distribution | Method |
  |----------|-------------|--------|
  | Server   | `cargo install workgraph` or prebuilt binary | Installed once on shared VPS |
  | Desktop  | SSH client (pre-installed) + mosh (`brew install mosh`, `apt install mosh`) | User's own machine |
  | Web      | URL (e.g., `wg.example.com`) | Zero-install, just a browser |
  | Android  | Termux (F-Droid) + setup script | One-time install |
  | iOS      | Blink Shell (App Store, $15.99) OR Safari to web URL | One-time install or zero-install |

  - Server binary distribution: GitHub Releases with prebuilt linux-x86_64, linux-aarch64
  - Setup landing page: `wg.example.com/setup` with platform detection → shows relevant guide
  - No app store submissions for v0.x — Termux and Blink Shell are the clients
- **Files:** `docs/guides/getting-started.md` (cross-platform quickstart)
- **Dependencies:** Tasks 3, 6, 7 (platform guides exist to link to)
- **Test strategy:**
  - Follow each platform's path end-to-end from zero to working TUI
  - Verify all links and commands in docs are correct

### Task 11: Connection Resilience Testing Suite

- **Platform:** Cross-platform
- **Decision:** BUILD — test infrastructure
- **Complexity:** M
- **Description:** Create a systematic test suite for connection resilience across platforms:
  - Script that simulates: network drop (iptables/pfctl), high latency (tc netem), Wi-Fi→cellular switch
  - Test matrix: [SSH, mosh, ttyd] × [network drop, high latency, IP change]
  - Measure: reconnection time, state preservation (cursor position, scroll, modal dialogs)
  - Document results in a connection resilience matrix
  - Identify any TUI state that doesn't survive reconnection (and file bugs)
- **Files:** `tests/connection/` directory, `docs/design/connection-resilience-results.md`
- **Dependencies:** Tasks 3, 8, 9 (all connection paths set up)
- **Test strategy:** This IS the test — the output is the test results and any bugs filed

### Task 12: Unified Server Deployment Script

- **Platform:** Cross-platform (server)
- **Decision:** BUILD — automation script
- **Complexity:** M
- **Description:** One-command server setup that configures everything for multi-user + all client access:
  - `wg-server-setup.sh`:
    - Installs workgraph binary (from GitHub Releases)
    - Installs and configures: tmux, mosh-server, ttyd, Caddy
    - Generates Caddyfile with TLS + basic auth
    - Creates systemd units for ttyd and wg service
    - Opens firewall ports (22/SSH, 443/HTTPS, 60000-61000/mosh)
    - Creates `wg-connect.sh` (Task 8) in the right location
    - Prints per-platform connection instructions
  - Supports: Ubuntu 22.04/24.04, Debian 12. Others: manual setup via docs.
- **Files:** `scripts/wg-server-setup.sh`, `docs/guides/server-setup.md`
- **Dependencies:** Tasks 3, 8, 9 (individual components designed first)
- **Test strategy:**
  - Run on fresh VPS (DigitalOcean, Hetzner) from each supported OS
  - Verify all platforms can connect after setup completes
  - Test idempotency: run twice, verify no breakage

---

## Dependency Graph

```
                    ┌─────────────────┐
                    │  Task 1:        │
                    │  Responsive TUI │
                    │  Breakpoints    │
                    │  [M, cross]     │
                    └────────┬────────┘
                             │
                    ┌────────▼────────┐
                    │  Task 2:        │
                    │  Single-Panel   │
                    │  Navigation     │
                    │  [M, cross]     │
                    └─────────────────┘

┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐   ┌──────────┐
│ Task 3:  │   │ Task 6:  │   │ Task 7:  │   │ Task 8:  │   │ Task 9:  │
│ ttyd     │   │ Termux   │   │ Blink    │   │ Connect  │   │ mosh     │
│ Deploy   │   │ Setup    │   │ Guide    │   │ Dispatch │   │ Server   │
│ Guide    │   │ Script   │   │ [S, iOS] │   │ Script   │   │ Guide    │
│ [S, web] │   │ [S, Andr]│   └──────────┘   │ [S, svr] │   │ [S, svr] │
└────┬─────┘   └──────────┘                   └──────────┘   └──────────┘
     │                                              │
┌────▼─────┐                                        │
│ Task 4:  │                                        │
│ PWA      │                                        │
│ Manifest │                                        │
│ [S, web] │                                        │
└──────────┘                                        │
     │                                              │
┌────▼─────┐                                        │
│ Task 5:  │                                        │
│ xterm.js │                                        │
│ Validate │                                        │
│ [S, web] │                                        │
└──────────┘                                        │
                                                    │
┌───────────────────────────────────────────────────┘
│        After Tasks 3, 6, 7, 8, 9:
│
│  ┌──────────┐   ┌──────────┐   ┌──────────┐
│  │ Task 10: │   │ Task 11: │   │ Task 12: │
│  │ Distrib  │   │ Resili-  │   │ Server   │
│  │ Strategy │   │ ence     │   │ Deploy   │
│  │ [S, all] │   │ Testing  │   │ Script   │
│  └──────────┘   │ [M, all] │   │ [M, svr] │
│                 └──────────┘   └──────────┘
```

---

## Build vs. Integrate Summary

| Task | Platform | Decision | Rationale |
|------|----------|----------|-----------|
| 1. Responsive TUI | Cross | **BUILD** | Core TUI must adapt to small screens — no existing tool solves this |
| 2. Single-Panel Nav | Cross | **BUILD** | Navigation logic for single-panel mode is application-specific |
| 3. ttyd Deployment | Web | **INTEGRATE** | ttyd + Caddy are mature, zero code changes to workgraph |
| 4. PWA Manifest | Web | **BUILD** (small) | Static web assets — trivial to create, but don't exist yet |
| 5. xterm.js Validation | Web | **BUILD** (small) | May require minor TUI rendering fixes |
| 6. Termux Setup | Android | **INTEGRATE** | Termux + mosh work today, we just automate setup |
| 7. Blink Guide | iOS | **INTEGRATE** | Blink Shell is the only viable mosh client on iOS |
| 8. Connect Dispatcher | Server | **BUILD** (small) | Simple shell script, unifies all entry points |
| 9. mosh Server Guide | Server | **INTEGRATE** | mosh-server is standard; just document configuration |
| 10. Distribution | Cross | **INTEGRATE** | Leverage existing channels: GitHub Releases, F-Droid, App Store |
| 11. Resilience Tests | Cross | **BUILD** | No existing test infrastructure for this |
| 12. Server Deploy | Server | **BUILD** | Automation script; individual pieces are integrated, the glue is built |

**Bottom line:** 4 BUILD tasks (responsive TUI, single-panel nav, resilience tests, server script), 2 BUILD-small tasks (PWA, connect dispatcher), 6 INTEGRATE tasks. The vast majority of client access works by integrating existing tools — the main engineering effort is making the TUI responsive.

---

## Complexity Summary

| Complexity | Count | Tasks |
|-----------|-------|-------|
| **S** (days) | 7 | 3, 4, 5, 6, 7, 8, 9 |
| **M** (1-2 weeks) | 5 | 1, 2, 10, 11, 12 |
| **L** | 0 | — |

Total estimated effort: ~8-10 weeks with 1-2 agents, or ~4-5 weeks with parallelism (Tasks 1-2 are the critical path; Tasks 3-9 can run in parallel).

---

## Distribution Strategy Per Platform

| Platform | v0.x Distribution | v1.0+ Distribution | Notes |
|----------|-------------------|-------------------|-------|
| **Server** | `cargo install workgraph` or GitHub Release binary | Same + distro packages (deb, rpm) | Single install per project |
| **Desktop** | SSH/mosh (pre-installed or one `brew`/`apt` command) | Same | Zero-install for most users |
| **Web** | URL (`wg.example.com`) | Same, possibly hosted offering | True zero-install |
| **Android** | Termux (F-Droid) + setup script | Consider custom app if demand | F-Droid, not Google Play |
| **iOS** | Blink Shell ($15.99, App Store) or Safari web | Consider custom app if demand | Cost barrier for native; web is free fallback |

**Key decision: no custom mobile apps in v0.x.** Termux and Blink Shell provide 90% of the UX at 0% of the maintenance burden. Custom apps (Kotlin/Swift with embedded mosh) are v1.0+ only, justified by user demand.

---

## Open Questions for Implementation

1. **ttyd multi-user session binding:** How does ttyd map an authenticated user to the correct tmux session? Options: (a) ttyd launched per-user by the reverse proxy, (b) single ttyd instance with a dispatcher script that reads the auth header. Needs prototyping.

2. **Termux:Widget reliability:** Does the shortcut mechanism work reliably across Android versions and manufacturers? Need testing on Samsung, Pixel, and budget Android devices.

3. **Blink Shell free alternative:** Is there a free iOS mosh client that's good enough? If not, the web fallback (Safari → ttyd) is the only free option on iOS. This may limit iOS adoption.

4. **PWA push notifications:** Can the PWA receive push notifications for task completions? Requires server-side push endpoint + VAPID keys. Worth implementing only if web is a primary access method.

5. **Responsive TUI breakpoint thresholds:** The architecture doc suggests <50/50-80/>80. Need to validate against real device terminal sizes (Termux on various phones, Blink on iPhone/iPad) before committing to exact breakpoints.
