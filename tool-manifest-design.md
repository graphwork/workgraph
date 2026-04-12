# Universal Tool Manifest Design for Workgraph Agents

## Overview

This design provides a unified system for declaring, discovering, and scoping tools across all workgraph executor types (claude, native, shell, amplifier). It builds on the existing bundle system while adding MCP integration and cross-executor compatibility.

## Design Principles

1. **Standards-based**: Use TOML for configuration, JSON Schema for validation, MCP for protocol compatibility
2. **Backwards compatible**: Existing projects without `tools.toml` keep current behavior
3. **Executor agnostic**: Same tools available across all executor types
4. **Simple format**: No custom DSL - just TOML and JSON Schema
5. **Extensible**: Support for custom tool sources via plugins

## Tool Manifest Format

### Primary Format: `tools.toml`

The tool manifest is a TOML file located at `.workgraph/tools.toml` in the project root.

```toml
[meta]
version = "1.0"
description = "Tool configuration for workgraph project"

# Tool source definitions
[sources]

[sources.native]
type = "builtin"
tools = ["file", "bash", "wg", "web_search", "bg"]
description = "Core workgraph tools"

[sources.mcp-claude-skills]
type = "mcp"
server_url = "stdio:claude-skills-server"
config_path = "~/.claude/mcp.json" 
description = "Claude Code skills via MCP"

[sources.web-tools]
type = "mcp" 
server_url = "http://localhost:3000/mcp"
tools = ["web_fetch", "web_scrape", "pdf_extract"]
description = "Extended web capabilities"

[sources.custom-project]
type = "directory"
path = "./tools"
description = "Project-specific tools"

# Bundle definitions - inherit and extend
[bundles]

[bundles.research]
extends = "builtin:research"
tools = [
  "file:read", "file:glob", "file:grep", 
  "bash",
  "wg:show", "wg:list", "wg:log",
  "mcp-claude-skills:*",
  "web-tools:web_fetch"
]
description = "Read-only research with web access"

[bundles.implementer] 
extends = "builtin:implementer"
tools = ["*"]
exclude = ["bash:rm", "bash:sudo"]
description = "Full implementation access with safety excludes"

[bundles.web-focused]
tools = [
  "file:read", "file:write",
  "web-tools:*",
  "mcp-claude-skills:web_search"
]
description = "Web scraping and analysis tasks"

# Per-task tool scoping
[task-scopes]
"research-*" = { bundle = "research" }
"implement-*" = { bundle = "implementer" } 
"web-*" = { bundle = "web-focused" }
"debug-*" = { tools = ["file:*", "bash", "wg:*"], exclude = ["file:write"] }

# Default tool sets per executor
[defaults]
claude = { bundle = "research" }
native = { bundle = "implementer" }
shell = { tools = ["bash", "wg:*"] }
amplifier = { bundle = "implementer" }
```

### Alternative Format: `mcp.json` (MCP-first projects)

For projects primarily using MCP servers:

```json
{
  "meta": {
    "version": "1.0",
    "format": "mcp-primary"
  },
  "mcpServers": {
    "claude-skills": {
      "command": "claude-skills-server",
      "args": []
    },
    "web-tools": {
      "url": "http://localhost:3000/mcp"
    }
  },
  "workgraph": {
    "bundles": {
      "default": {
        "mcpTools": ["claude-skills:*", "web-tools:*"],
        "nativeTools": ["wg:*"]
      }
    }
  }
}
```

## Tool Source Types

### 1. Builtin (`type = "builtin"`)

References workgraph's native tool registry:
- `file`: read_file, write_file, edit_file, glob, grep
- `bash`: shell command execution
- `wg`: workgraph operations (show, list, add, done, etc.)
- `web_search`: DuckDuckGo search
- `bg`: background job management

### 2. MCP Server (`type = "mcp"`)

Connects to Model Context Protocol servers:
- `server_url`: stdio command, HTTP endpoint, or Unix socket
- `config_path`: Optional MCP server configuration
- `tools`: Optional explicit tool list (discovers all if omitted)

### 3. Directory (`type = "directory"`)

Loads tools from a directory of implementations:
- `path`: Directory containing tool implementations
- File format: `tool_name.json` (JSON Schema) + `tool_name.{rs,py,js}` (implementation)

### 4. Bundle Reference (`type = "bundle"`)

References another tool bundle:
- `bundle`: Name of bundle to include
- `source`: Source project (for cross-project sharing)

## Tool Naming and References

Tools are referenced using `source:tool` syntax:
- `file:read` - read_file tool from native source
- `mcp-claude-skills:skill` - skill tool from MCP server
- `bash` - unqualified name uses first matching source
- `*` - all tools from source (with optional exclude list)

## Bundle System Integration

### Built-in Bundle Migration

Current bundles become source references:
- `builtin:bare` → native source with `["wg:*"]`
- `builtin:research` → native source with read-only tools
- `builtin:implementer` → native source with `["*"]`

### Bundle Inheritance

Bundles can extend built-ins or other bundles:
```toml
[bundles.enhanced-research]
extends = "builtin:research"
tools = ["+web-tools:*", "+custom:analyze"]  # + means add to base
exclude = ["bash"]
```

## Per-Task Tool Scoping

Tasks can specify tool requirements via:

### 1. Task-level scoping in `tools.toml`
```toml
[task-scopes]
"web-scraping-*" = { bundle = "web-focused" }
"secure-*" = { tools = ["file:read", "wg:*"], exclude = ["bash"] }
```

### 2. Bundle specification in `wg add`
```bash
wg add "Scrape news sites" --bundle web-focused
wg add "Research APIs" --tools "file:read,web-tools:*,mcp-claude-skills:search"
```

### 3. Tool requirements in task description
```bash
wg add "Debug network issues" -d "
## Description
Investigate connection timeouts.

## Required Tools
- web-tools:* (for testing endpoints)
- bash (for network commands)
- file:read (for log analysis)
"
```

## Registration and Discovery

### Startup Sequence

1. **Check for manifest**: Look for `.workgraph/tools.toml` or `mcp.json`
2. **Load sources**: Initialize all defined tool sources
3. **Build registry**: Create unified `ToolRegistry` with all available tools
4. **Apply scoping**: Filter tools based on executor type and task requirements

### MCP Discovery

MCP servers are discovered via:
- **Explicit configuration** in `tools.toml` 
- **Standard locations**: `~/.config/mcp/`, `~/.claude/mcp.json`
- **Environment variables**: `MCP_SERVERS_CONFIG`
- **Auto-discovery**: Scan for MCP servers on localhost ports

### Tool Resolution Priority

1. **Task-specific**: Tools explicitly required by task
2. **Bundle-defined**: Tools from assigned bundle
3. **Executor default**: Default bundle for executor type
4. **Global fallback**: Builtin implementer bundle

## Integration with Executor Types

### Native Executor

- **Current**: Uses `ToolRegistry::default_all()` 
- **New**: Uses `ToolRegistry::from_manifest(manifest)`
- **MCP Integration**: Wraps MCP tools as native `Tool` implementations
- **Backwards compatibility**: No manifest = current behavior

### Claude Executor

- **Current**: Uses Claude Code's built-in tool set
- **New**: Exports available tools as MCP server
- **Tool overlap**: Maps workgraph tools to Claude equivalents where possible
- **Enhancement**: Gets access to workgraph-specific tools (wg:*)

### Shell Executor

- **Current**: Limited to bash commands
- **New**: Can use any tool via CLI wrappers
- **Implementation**: Generate shell scripts that call tool APIs
- **Safety**: Restrict to read-only tools by default

### Amplifier Executor

- **Current**: Delegates to other executors
- **New**: Orchestrates tools across multiple agents
- **Bundle splitting**: Different tool sets for different agent types
- **Coordination**: Uses wg:* tools for inter-agent communication

## Migration Path

### Phase 1: Manifest Support (Backwards Compatible)

1. Add `ToolManifest` struct and TOML parser
2. Update `ToolRegistry` to support manifest loading
3. Maintain `default_all()` as fallback
4. No behavior change for existing projects

### Phase 2: MCP Integration

1. Implement MCP client library
2. Add `MCPToolSource` that wraps MCP servers as tools
3. Support `mcp://` URLs in tool sources
4. Auto-discover common MCP configurations

### Phase 3: Cross-Executor Tool Sharing

1. Export native tools as MCP server (`wg mcp serve`)
2. Update claude/amplifier executors to use MCP
3. Implement tool CLI wrappers for shell executor
4. Enable full cross-executor compatibility

### Phase 4: Enhanced Features

1. Tool versioning and compatibility checks
2. Dynamic tool loading/unloading
3. Tool performance metrics and optimization
4. Federated tool sharing across projects

## Example Use Cases

### Research Task with Web Access
```toml
[bundles.web-research]
tools = [
  "file:read", "file:grep", "file:glob",
  "bash:curl", "bash:jq", 
  "web-tools:fetch", "web-tools:scrape",
  "mcp-claude-skills:search", "mcp-claude-skills:analyze",
  "wg:show", "wg:log", "wg:artifact"
]
```

### Secure Implementation Task
```toml
[bundles.secure-dev]
tools = ["file:*", "wg:*"]
exclude = ["bash", "web-tools:*"]
description = "Code-only implementation without system access"
```

### Multi-Agent Coordination
```toml
[bundles.coordinator]
tools = ["wg:*", "mcp-claude-skills:plan", "mcp-claude-skills:analyze"]

[bundles.worker]
tools = ["file:*", "bash", "wg:show", "wg:log", "wg:done", "wg:fail"]
```

## Validation and Schema

Tool manifests are validated against JSON Schema for structure and MCP protocol compliance for tool definitions. The schema ensures:
- Required fields are present
- Tool source URLs are valid
- Bundle references exist
- No circular dependencies in bundle inheritance
- Tool naming follows `source:tool` convention