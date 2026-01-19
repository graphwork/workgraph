# Human Interface Options for Workgraph

This document researches human interface options for workgraph, covering TUI, web interfaces, IDE integrations, and other integrations.

---

## Table of Contents

1. [TUI (Terminal UI)](#1-tui-terminal-ui)
2. [Web Interface](#2-web-interface)
3. [IDE Integrations](#3-ide-integrations)
4. [Other Integrations](#4-other-integrations)
5. [Recommendations](#5-recommendations)

---

## 1. TUI (Terminal UI)

### 1.1 Rust TUI Libraries

#### ratatui (Recommended)

**Crate**: [ratatui](https://crates.io/crates/ratatui) | [Documentation](https://docs.rs/ratatui)

Ratatui is the actively maintained fork of tui-rs, and has become the de facto standard for terminal UIs in Rust.

**Key Features:**
- Immediate-mode rendering (draw every frame, stateless widgets)
- Rich widget library: tables, lists, paragraphs, charts, gauges, sparklines
- Flexible layouts with constraint-based positioning
- Backend-agnostic: supports crossterm (cross-platform), termion, termwiz
- Active community and maintenance

**Example structure for workgraph TUI:**

```rust
use ratatui::{
    prelude::*,
    widgets::{Block, Borders, List, ListItem, Paragraph, Table, Row},
};

// Main layout could be:
// +------------------+------------------+
// |   Task List      |   Task Details   |
// |   (scrollable)   |                  |
// +------------------+------------------+
// |          Status Bar / Log           |
// +-------------------------------------+

fn ui(frame: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(10),     // Main area
            Constraint::Length(3),   // Status bar
        ])
        .split(frame.area());

    let main_chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),  // Task list
            Constraint::Percentage(60),  // Details
        ])
        .split(chunks[0]);

    // Task list with status indicators
    let tasks: Vec<ListItem> = app.tasks.iter().map(|t| {
        let status = match t.status {
            Status::Ready => "[R]",
            Status::InProgress => "[>]",
            Status::Blocked => "[B]",
            Status::Done => "[+]",
        };
        ListItem::new(format!("{} {}", status, t.title))
    }).collect();

    let task_list = List::new(tasks)
        .block(Block::default().title("Tasks").borders(Borders::ALL))
        .highlight_style(Style::default().add_modifier(Modifier::REVERSED));

    frame.render_stateful_widget(task_list, main_chunks[0], &mut app.list_state);
}
```

**Cargo.toml:**

```toml
[dependencies]
ratatui = "0.29"
crossterm = "0.28"
```

#### cursive

**Crate**: [cursive](https://crates.io/crates/cursive)

Dialog-based TUI library with a retained-mode API (more like traditional GUI programming).

**Key Features:**
- Retained-mode: create views once, update via callbacks
- Built-in dialog system for prompts/confirmations
- Theme support
- Multiple backends (ncurses, pancurses, termion, crossterm)

**When to use:** Better for form-heavy interfaces with lots of dialogs. Less suitable for real-time dashboards.

#### tui-rs (Deprecated)

The original library that ratatui forked from. No longer maintained - use ratatui instead.

### 1.2 What Would a Workgraph TUI Look Like?

**Core Views:**

1. **Task List View**
   - Scrollable list of all tasks
   - Visual status indicators: `[R]` ready, `[>]` in-progress, `[B]` blocked, `[+]` done
   - Filtering by status, actor, tags
   - Sorting by priority, age, impact

2. **Dependency Graph View**
   - ASCII-art DAG visualization
   - Could use `termgraph` or custom rendering
   - Show critical path highlighted
   - Zoom/pan through large graphs

3. **Task Details Panel**
   - Full task information when selected
   - Blocked-by chain
   - Impact (what it unblocks)
   - Metadata, timestamps

4. **Action Bar**
   - Quick commands: claim, done, add, unclaim
   - Keyboard shortcuts displayed

5. **Status/Log Area**
   - Recent events (task completed, new task added)
   - Command output

**Keyboard Navigation:**

| Key | Action |
|-----|--------|
| `j`/`k` or arrows | Navigate task list |
| `Enter` | View task details |
| `c` | Claim selected task |
| `d` | Mark done |
| `a` | Add new task (opens prompt) |
| `f` | Filter tasks |
| `g` | Toggle graph view |
| `r` | Refresh |
| `q` | Quit |
| `?` | Help |

### 1.3 Pros and Cons of TUI

**Pros:**
- Stays in terminal workflow (no context switching)
- Fast startup, low latency
- Works over SSH (critical for remote servers)
- Works in tmux/screen sessions
- No additional dependencies (no browser, no GUI)
- Keyboard-driven (efficient for power users)
- Can run alongside agent processes

**Cons:**
- Graph visualization limited to ASCII art
- Learning curve for users unfamiliar with TUI conventions
- Limited real estate for complex information
- No mouse support in some terminals (though ratatui supports it)

---

## 2. Web Interface

### 2.1 Rust Web Frameworks

#### axum (Recommended)

**Crate**: [axum](https://crates.io/crates/axum) | [Documentation](https://docs.rs/axum)

Modern, ergonomic web framework built on tokio and tower, from the tokio team.

**Key Features:**
- Async-native with tokio
- Type-safe routing and extractors
- Tower middleware ecosystem
- WebSocket support
- Good documentation

**Basic server for workgraph:**

```rust
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Json, Router,
};
use std::sync::Arc;
use tokio::sync::RwLock;

type AppState = Arc<RwLock<Workgraph>>;

async fn list_tasks(State(state): State<AppState>) -> Json<Vec<Task>> {
    let wg = state.read().await;
    Json(wg.list_tasks())
}

async fn get_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
) -> Json<Option<Task>> {
    let wg = state.read().await;
    Json(wg.get_task(&id))
}

async fn ready_tasks(State(state): State<AppState>) -> Json<Vec<Task>> {
    let wg = state.read().await;
    Json(wg.ready())
}

async fn claim_task(
    State(state): State<AppState>,
    Path(id): Path<String>,
    Json(payload): Json<ClaimRequest>,
) -> Json<Result<Task, String>> {
    let mut wg = state.write().await;
    Json(wg.claim(&id, &payload.actor))
}

#[tokio::main]
async fn main() {
    let state: AppState = Arc::new(RwLock::new(Workgraph::load(".")?));

    let app = Router::new()
        .route("/api/tasks", get(list_tasks))
        .route("/api/tasks/:id", get(get_task))
        .route("/api/ready", get(ready_tasks))
        .route("/api/tasks/:id/claim", post(claim_task))
        .route("/api/tasks/:id/done", post(done_task))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind("127.0.0.1:3000").await.unwrap();
    axum::serve(listener, app).await.unwrap();
}
```

**Cargo.toml:**

```toml
[dependencies]
axum = "0.7"
tokio = { version = "1", features = ["full"] }
tower = "0.5"
tower-http = { version = "0.6", features = ["cors", "fs"] }
```

#### actix-web

Older, very performant framework. More complex API than axum. Good if you need raw performance, but axum is generally preferred for new projects.

### 2.2 Frontend Options

#### Option A: Minimal HTML/JS (Recommended to start)

Simple static HTML with vanilla JavaScript or Alpine.js. Served from the Rust backend.

**Pros:**
- No build step
- Fast to prototype
- Easy to understand

**Example structure:**

```
static/
  index.html
  style.css
  app.js
```

```html
<!-- index.html -->
<!DOCTYPE html>
<html>
<head>
    <title>Workgraph Dashboard</title>
    <link rel="stylesheet" href="/style.css">
    <script defer src="https://unpkg.com/alpinejs@3.x.x/dist/cdn.min.js"></script>
</head>
<body x-data="workgraph()">
    <div class="container">
        <h1>Workgraph</h1>

        <div class="stats">
            <span x-text="stats.ready + ' ready'"></span>
            <span x-text="stats.inProgress + ' in progress'"></span>
            <span x-text="stats.blocked + ' blocked'"></span>
        </div>

        <h2>Ready Tasks</h2>
        <ul>
            <template x-for="task in readyTasks">
                <li>
                    <span x-text="task.title"></span>
                    <button @click="claimTask(task.id)">Claim</button>
                </li>
            </template>
        </ul>

        <div id="graph"></div>
    </div>
    <script src="/app.js"></script>
</body>
</html>
```

#### Option B: htmx

Server-rendered HTML with htmx for interactivity. No JavaScript framework needed.

```html
<div hx-get="/api/tasks" hx-trigger="every 5s" hx-swap="innerHTML">
    <!-- Tasks rendered server-side -->
</div>

<button hx-post="/api/tasks/{{id}}/claim" hx-swap="outerHTML">
    Claim
</button>
```

**Pros:**
- Server-side rendering (Rust templates)
- Minimal JavaScript
- Good for CRUD operations

#### Option C: React/Vue/Svelte

Full SPA framework. Overkill for a simple dashboard, but provides best UX for complex interactions.

### 2.3 Graph Visualization Libraries

#### D3.js

**Website**: [d3js.org](https://d3js.org)

Most powerful and flexible, but steep learning curve.

```javascript
// Force-directed graph layout
const simulation = d3.forceSimulation(nodes)
    .force("link", d3.forceLink(links).id(d => d.id))
    .force("charge", d3.forceManyBody())
    .force("center", d3.forceCenter(width / 2, height / 2));
```

#### vis.js (vis-network)

**Website**: [visjs.org](https://visjs.org)

Easier to use than D3 for network graphs. Good defaults.

```javascript
const nodes = new vis.DataSet([
    { id: 1, label: "Task A" },
    { id: 2, label: "Task B" },
]);
const edges = new vis.DataSet([
    { from: 1, to: 2 }
]);
const network = new vis.Network(container, { nodes, edges }, options);
```

#### Mermaid

**Website**: [mermaid.js.org](https://mermaid.js.org)

Text-based graph definition. Easy to generate from backend.

```javascript
// Generate Mermaid syntax from workgraph
const mermaidDef = `
graph LR
    task-a[Task A] --> task-b[Task B]
    task-a --> task-c[Task C]
    task-b --> task-d[Task D]

    style task-a fill:#90EE90
    style task-b fill:#FFD700
`;
mermaid.render('graph', mermaidDef);
```

**Pros:**
- Simple text-based definition
- Can be generated server-side
- Works in GitHub markdown
- Supports multiple diagram types

**Recommended for workgraph:** Mermaid for simplicity, vis.js if you need interactivity.

### 2.4 Serving Static Files with axum

```rust
use tower_http::services::ServeDir;

let app = Router::new()
    .route("/api/tasks", get(list_tasks))
    // ... other API routes
    .fallback_service(ServeDir::new("static"));
```

### 2.5 Pros and Cons of Web Interface

**Pros:**
- Rich visualization (proper graph rendering)
- Accessible from any device with a browser
- Familiar UI paradigm for most users
- Can show multiple views simultaneously
- Mouse-friendly

**Cons:**
- Additional dependency (browser)
- Slightly more complexity to set up
- Needs a running server process
- More resource overhead
- Security considerations if exposed to network

---

## 3. IDE Integrations

### 3.1 VS Code Extension

**Development approach:** TypeScript extension that shells out to `wg` CLI.

**Features to implement:**

1. **Sidebar Panel**
   - Tree view of tasks organized by status
   - Ready tasks at top
   - Click to see details

2. **Status Bar Item**
   - Show count of ready tasks
   - Click to quick-claim

3. **Commands** (Command Palette)
   - `Workgraph: List Ready Tasks`
   - `Workgraph: Claim Task`
   - `Workgraph: Mark Done`
   - `Workgraph: Add Task`
   - `Workgraph: Show Graph`

4. **CodeLens** (optional)
   - Show task references in code comments
   - `// TODO(task-id): ...` -> clickable link

**Implementation sketch:**

```typescript
// extension.ts
import * as vscode from 'vscode';
import { exec } from 'child_process';

export function activate(context: vscode.ExtensionContext) {
    // Register tree view
    const taskProvider = new TaskTreeProvider();
    vscode.window.registerTreeDataProvider('workgraphTasks', taskProvider);

    // Register commands
    context.subscriptions.push(
        vscode.commands.registerCommand('workgraph.ready', async () => {
            const output = await execWg('ready --json');
            const tasks = JSON.parse(output);
            // Show quick pick or tree view
        }),

        vscode.commands.registerCommand('workgraph.claim', async (taskId: string) => {
            await execWg(`claim ${taskId}`);
            taskProvider.refresh();
            vscode.window.showInformationMessage(`Claimed: ${taskId}`);
        }),

        vscode.commands.registerCommand('workgraph.done', async (taskId: string) => {
            await execWg(`done ${taskId}`);
            taskProvider.refresh();
        })
    );

    // Status bar
    const statusBar = vscode.window.createStatusBarItem(
        vscode.StatusBarAlignment.Left
    );
    statusBar.command = 'workgraph.ready';
    updateStatusBar(statusBar);

    // File watcher for .workgraph changes
    const watcher = vscode.workspace.createFileSystemWatcher(
        '**/.workgraph/**'
    );
    watcher.onDidChange(() => {
        taskProvider.refresh();
        updateStatusBar(statusBar);
    });
}

function execWg(args: string): Promise<string> {
    return new Promise((resolve, reject) => {
        exec(`wg ${args}`, { cwd: vscode.workspace.rootPath },
            (err, stdout) => err ? reject(err) : resolve(stdout));
    });
}
```

**Effort:** Medium. Requires learning VS Code extension API. TypeScript knowledge needed.

### 3.2 Neovim Plugin (Lua)

**Development approach:** Lua plugin that calls `wg` CLI via `vim.fn.system()`.

**Features:**

1. **Telescope picker** for tasks
2. **Floating window** for task details
3. **Signs/virtual text** for task references in code
4. **Commands** (`:WgReady`, `:WgClaim`, etc.)

**Implementation sketch:**

```lua
-- lua/workgraph/init.lua
local M = {}

function M.ready()
    local output = vim.fn.system('wg ready --json')
    local tasks = vim.fn.json_decode(output)

    -- Use telescope for selection
    local pickers = require('telescope.pickers')
    local finders = require('telescope.finders')
    local actions = require('telescope.actions')

    pickers.new({}, {
        prompt_title = 'Ready Tasks',
        finder = finders.new_table({
            results = tasks,
            entry_maker = function(task)
                return {
                    value = task,
                    display = task.title,
                    ordinal = task.title,
                }
            end,
        }),
        attach_mappings = function(prompt_bufnr, map)
            actions.select_default:replace(function()
                local selection = actions.get_selected_entry()
                actions.close(prompt_bufnr)
                M.claim(selection.value.id)
            end)
            return true
        end,
    }):find()
end

function M.claim(task_id)
    vim.fn.system('wg claim ' .. task_id)
    vim.notify('Claimed: ' .. task_id)
end

function M.done(task_id)
    vim.fn.system('wg done ' .. task_id)
    vim.notify('Done: ' .. task_id)
end

function M.setup()
    vim.api.nvim_create_user_command('WgReady', M.ready, {})
    vim.api.nvim_create_user_command('WgClaim', function(opts)
        M.claim(opts.args)
    end, { nargs = 1 })
    vim.api.nvim_create_user_command('WgDone', function(opts)
        M.done(opts.args)
    end, { nargs = 1 })
end

return M
```

**Effort:** Low-medium. Neovim's Lua API is well-documented.

### 3.3 How Would IDE Integration Show Task Status?

1. **Sidebar/tree view**: Grouped by status (Ready, In Progress, Blocked, Done)
2. **Status bar**: Badge with ready count, click to expand
3. **Notifications**: When claimed task has new blockers, or when something you blocked is now unblocked
4. **CodeLens**: Inline annotations on task references in code

---

## 4. Other Integrations

### 4.1 GitHub Issues Sync

**Bidirectional sync possibilities:**

1. **GitHub -> Workgraph**
   - Import issues as tasks
   - Map labels to metadata
   - Issue dependencies (if using project boards) map to blocked-by

2. **Workgraph -> GitHub**
   - Create issues from tasks
   - Update issue status when task status changes
   - Add comments with progress updates

**Implementation approach:**

```rust
use octocrab::Octocrab;

async fn sync_from_github(wg: &mut Workgraph, repo: &str) -> Result<()> {
    let octocrab = Octocrab::builder()
        .personal_token(env::var("GITHUB_TOKEN")?)
        .build()?;

    let issues = octocrab
        .issues(owner, repo)
        .list()
        .state(octocrab::params::State::Open)
        .send()
        .await?;

    for issue in issues {
        let task_id = format!("gh-{}", issue.number);
        if !wg.has_task(&task_id) {
            wg.add_task(Task {
                id: task_id,
                title: issue.title,
                metadata: json!({
                    "github_url": issue.html_url,
                    "labels": issue.labels,
                }),
                ..Default::default()
            })?;
        }
    }
    Ok(())
}
```

**Challenges:**
- Two-way sync is complex (conflict resolution)
- GitHub API rate limits
- Mapping between task states and issue states

**Recommendation:** Start with one-way import (GitHub -> workgraph). Bidirectional sync adds significant complexity.

### 4.2 Slack/Discord Notifications

**Use cases:**
- Notify when a task you're blocked by is completed
- Daily digest of ready tasks
- Alert when tasks are aging

**Implementation with webhooks:**

```rust
async fn notify_slack(webhook_url: &str, message: &str) -> Result<()> {
    let client = reqwest::Client::new();
    client.post(webhook_url)
        .json(&json!({ "text": message }))
        .send()
        .await?;
    Ok(())
}

// Example usage
async fn on_task_completed(task: &Task, wg: &Workgraph) {
    let unblocked = wg.get_unblocked_by(task.id);
    if !unblocked.is_empty() {
        let msg = format!(
            "{} completed! Now ready: {}",
            task.title,
            unblocked.iter().map(|t| &t.title).join(", ")
        );
        notify_slack(&config.slack_webhook, &msg).await?;
    }
}
```

**Effort:** Low. Webhook-based notifications are straightforward.

### 4.3 CI/CD Integration

**Creating tasks from failing tests:**

```yaml
# .github/workflows/ci.yml
- name: Run tests
  run: cargo test --no-fail-fast 2>&1 | tee test-output.txt
  continue-on-error: true

- name: Create tasks from failures
  if: failure()
  run: |
    grep -E "^test .* FAILED" test-output.txt | while read line; do
      test_name=$(echo "$line" | sed 's/test \(.*\) \.\.\. FAILED/\1/')
      wg add "Fix failing test: $test_name" --tag ci-failure
    done
```

**Other CI/CD integrations:**
- Block merge if critical tasks incomplete
- Auto-create release tasks when version bumped
- Track deployment tasks in workgraph

### 4.4 MCP (Model Context Protocol) Server

Since workgraph is designed for both humans and agents, an MCP server would be natural:

```rust
// Expose workgraph as MCP tools
#[mcp_tool]
async fn ready_tasks() -> Vec<Task> {
    wg.ready()
}

#[mcp_tool]
async fn claim_task(id: String, actor: String) -> Result<Task> {
    wg.claim(&id, &actor)
}

#[mcp_tool]
async fn add_task(title: String, blocked_by: Vec<String>) -> Task {
    wg.add(&title, &blocked_by)
}
```

This would allow any MCP-compatible agent to interact with workgraph directly.

---

## 5. Recommendations

### 5.1 Lowest Effort, Highest Value

**Ranking by effort/value ratio:**

| Option | Effort | Value | Notes |
|--------|--------|-------|-------|
| **Slack/Discord webhooks** | Very Low | Medium | Quick wins for team visibility |
| **Neovim plugin** | Low | High (for Neovim users) | Lua is easy, Telescope integration powerful |
| **Simple web dashboard** | Low-Medium | High | Mermaid for graphs, Alpine.js for interactivity |
| **TUI** | Medium | High | Best for terminal-centric users |
| **VS Code extension** | Medium | High (for VS Code users) | Larger user base than Neovim |
| **GitHub sync** | Medium-High | Medium | Complex to do well |
| **MCP server** | Medium | Very High (for agent use) | Natural fit for workgraph's mission |

### 5.2 What Serves Both Humans and Agents Well?

**Key insight:** The CLI with `--json` output is already agent-friendly. The question is what makes it more human-friendly.

**Best options for dual human/agent use:**

1. **MCP Server** (highest priority for agents)
   - Agents get first-class tool access
   - No need to parse CLI output
   - Can be used by Claude, GPT, and other MCP-compatible agents

2. **Web Dashboard** (high priority for humans)
   - Visual graph representation humans crave
   - Real-time updates via WebSocket
   - Works on any device

3. **TUI** (medium priority)
   - Power users who live in terminal
   - Works over SSH (important for remote work)
   - Agents can also drive TUI via PTY if needed

### 5.3 Recommended Implementation Order

**Phase 1: Foundation (1-2 weeks)**
1. Add `wg serve` command - simple web server with JSON API
2. Create minimal web dashboard with Mermaid graph
3. Add WebSocket for real-time updates

**Phase 2: Agent Integration (1 week)**
4. Implement MCP server (`wg mcp`)
5. Test with Claude and other agents

**Phase 3: Power User Tools (2-3 weeks)**
6. TUI with ratatui (`wg ui` or `wg tui`)
7. Neovim plugin (can be community-contributed)

**Phase 4: Team Features (optional)**
8. Slack/Discord notifications
9. GitHub sync (import only)
10. VS Code extension

### 5.4 Technical Recommendation Summary

| Component | Recommendation |
|-----------|----------------|
| Web framework | axum |
| Web frontend | Minimal HTML + Alpine.js + Mermaid |
| TUI framework | ratatui + crossterm |
| Graph visualization | Mermaid (simple), vis.js (interactive) |
| IDE plugin language | Lua (Neovim), TypeScript (VS Code) |
| Agent protocol | MCP (Model Context Protocol) |

### 5.5 Final Thoughts

The workgraph CLI already provides excellent machine-readable output via `--json`. The highest-value addition for humans is **visualization** - seeing the dependency graph, understanding bottlenecks visually, and getting real-time status updates.

For a project serving both humans and agents:
- **Agents** benefit most from an MCP server
- **Humans** benefit most from a web dashboard with graph visualization
- **Power users** (who are often the same people building agents) benefit from a TUI

Start with the web dashboard. It's the best effort/value tradeoff and provides the visualization that the CLI inherently cannot.

---

## References

- [ratatui GitHub](https://github.com/ratatui/ratatui)
- [ratatui Documentation](https://docs.rs/ratatui)
- [axum GitHub](https://github.com/tokio-rs/axum)
- [axum Documentation](https://docs.rs/axum)
- [Mermaid Documentation](https://mermaid.js.org/intro/)
- [vis.js Network](https://visjs.github.io/vis-network/docs/network/)
- [VS Code Extension API](https://code.visualstudio.com/api)
- [Neovim Lua Guide](https://neovim.io/doc/user/lua-guide.html)
- [MCP Specification](https://modelcontextprotocol.io/)
- [htmx](https://htmx.org/)
- [Alpine.js](https://alpinejs.dev/)
