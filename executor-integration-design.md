# Executor Integration Design: Universal Tool Manifest

This document details how the universal tool manifest system integrates with workgraph's current executor architecture.

## Current Architecture Review

### Native Executor (`src/executor/native/`)
- **Tool Registry**: `ToolRegistry` maps tool names to implementations
- **Tool Trait**: All tools implement `name()`, `definition()`, `execute()`, `is_read_only()`
- **Bundle System**: TOML files in `.workgraph/bundles/` filter available tools
- **Registration**: Hardcoded in `ToolRegistry::default_all()`

### Claude Executor 
- **Tool Set**: Uses Claude Code's built-in tools (Read, Write, Edit, Bash, etc.)
- **No Registry**: Tools are provided by Claude platform
- **Limited Customization**: Cannot add workgraph-specific tools like `wg:*`

### Shell Executor
- **Single Tool**: Only bash commands via shell execution
- **No Registry**: Direct command execution

### Amplifier Executor
- **Delegation**: Routes to other executors based on task type
- **Bundle Support**: Uses bundles to determine appropriate executor

## Integration Strategy

### 1. Native Executor Integration

#### Current Tool Registration
```rust
// src/executor/native/tools/mod.rs
pub fn default_all(working_dir: &Path, workgraph_dir: &Path) -> ToolRegistry {
    let mut registry = ToolRegistry::new();
    
    // Hardcoded registration
    file::register_file_tools(&mut registry);
    bash::register_bash_tool(&mut registry, working_dir);
    wg::register_wg_tools(&mut registry, workgraph_dir);
    web_search::register_web_search_tool(&mut registry);
    bg::register_bg_tool(&mut registry, workgraph_dir);
    
    registry
}
```

#### New Manifest-Based Registration
```rust
// src/executor/native/tools/mod.rs
pub fn from_manifest(manifest: &ToolManifest, context: &ExecutorContext) -> Result<ToolRegistry> {
    let mut registry = ToolRegistry::new();
    
    for (source_name, source_config) in &manifest.sources {
        match source_config.source_type {
            ToolSourceType::Builtin => {
                register_builtin_tools(&mut registry, source_config, context)?;
            }
            ToolSourceType::Mcp => {
                register_mcp_tools(&mut registry, source_config, context).await?;
            }
            ToolSourceType::Directory => {
                register_directory_tools(&mut registry, source_config, context)?;
            }
            ToolSourceType::Bundle => {
                register_bundle_tools(&mut registry, source_config, context)?;
            }
        }
    }
    
    Ok(registry)
}

// Backwards compatibility
pub fn default_all(working_dir: &Path, workgraph_dir: &Path) -> ToolRegistry {
    let manifest_path = workgraph_dir.join("tools.toml");
    
    if manifest_path.exists() {
        match ToolManifest::load(&manifest_path) {
            Ok(manifest) => {
                let context = ExecutorContext::new(working_dir, workgraph_dir);
                from_manifest(&manifest, &context).unwrap_or_else(|_| default_fallback())
            }
            Err(_) => default_fallback(),
        }
    } else {
        default_fallback()  // Current hardcoded behavior
    }
}
```

#### MCP Tool Wrapper
```rust
// src/executor/native/tools/mcp.rs
pub struct MCPTool {
    name: String,
    definition: ToolDefinition,
    client: MCPClient,
}

#[async_trait]
impl Tool for MCPTool {
    fn name(&self) -> &str { &self.name }
    
    fn definition(&self) -> &ToolDefinition { &self.definition }
    
    async fn execute(&self, input: &serde_json::Value) -> Result<ToolOutput> {
        let result = self.client.call_tool(&self.name, input).await?;
        Ok(ToolOutput::from_mcp_result(result))
    }
    
    fn is_read_only(&self) -> bool {
        // Infer from tool definition or MCP metadata
        self.definition.input_schema
            .get("properties")
            .and_then(|p| p.get("operation"))
            .and_then(|o| o.get("enum"))
            .map(|ops| !ops.as_array().unwrap().iter().any(|op| 
                op.as_str().unwrap_or("").contains("write")
            ))
            .unwrap_or(false)
    }
}
```

### 2. Claude Executor Integration

#### Current Limitation
The Claude executor uses Claude Code's built-in tools and cannot be extended with workgraph-specific tools.

#### Solution: MCP Export Server
```rust
// src/commands/mcp_serve.rs
pub async fn serve_mcp(port: u16, manifest: &ToolManifest) -> Result<()> {
    let registry = ToolRegistry::from_manifest(manifest, &context).await?;
    let server = MCPServer::new(registry);
    
    server.serve(format!("0.0.0.0:{}", port)).await?;
}
```

This allows Claude executors to access workgraph tools via MCP protocol.

#### Claude-Side Configuration
```json
// ~/.claude/mcp.json
{
  "mcpServers": {
    "workgraph": {
      "command": "wg",
      "args": ["mcp", "serve", "--port", "8080"]
    }
  }
}
```

### 3. Shell Executor Integration

#### Current Limitation
Shell executor only supports bash commands, no structured tool interface.

#### Solution: CLI Tool Wrappers
```rust
// src/executor/shell/mod.rs
pub fn generate_tool_wrappers(manifest: &ToolManifest, output_dir: &Path) -> Result<()> {
    for tool in manifest.resolve_tools("shell") {
        let wrapper_script = format!(
            r#"#!/bin/bash
# Generated wrapper for {tool_name}
wg tool call "{tool_name}" "$@"
"#,
            tool_name = tool.name
        );
        
        let script_path = output_dir.join(format!("{}.sh", tool.name));
        std::fs::write(&script_path, wrapper_script)?;
        std::fs::set_permissions(&script_path, std::os::unix::fs::PermissionsExt::from_mode(0o755))?;
    }
    Ok(())
}
```

#### Tool CLI Interface
```bash
# New wg subcommand for tool execution
wg tool call "file:read" '{"file_path": "/path/to/file"}'
wg tool list --source native
wg tool describe "web_search"
```

### 4. Amplifier Executor Integration

#### Current Bundle-Based Delegation
The Amplifier executor uses bundles to determine which sub-executor to use for tasks.

#### Enhanced Tool-Aware Delegation
```rust
// src/executor/amplifier/mod.rs
impl AmplifierExecutor {
    pub async fn route_task(&self, task: &Task) -> Result<Box<dyn Executor>> {
        let required_tools = self.analyze_task_tools(task).await?;
        let available_executors = self.get_compatible_executors(&required_tools).await?;
        
        // Prefer native for workgraph tools, claude for analysis, shell for simple commands
        let executor = match self.select_best_executor(&available_executors, &required_tools) {
            ExecutorType::Native if required_tools.has_wg_tools() => self.native_executor(),
            ExecutorType::Claude if required_tools.is_analysis_heavy() => self.claude_executor(),
            ExecutorType::Shell if required_tools.is_bash_only() => self.shell_executor(),
            _ => self.native_executor(), // Default fallback
        };
        
        Ok(executor)
    }
}
```

## Bundle System Evolution

### Current Bundle Format
```toml
# .workgraph/bundles/research.toml
name = "research"
description = "Read-only research tools"
tools = ["read_file", "glob", "grep", "web_search"]
context_scope = "task"
```

### Extended Bundle Format with Tool Sources
```toml
# .workgraph/bundles/enhanced-research.toml
name = "enhanced-research"
description = "Research with MCP integration"
extends = "builtin:research"

[tool-sources]
claude-skills = { type = "mcp", server = "claude-skills-server" }
web-tools = { type = "mcp", server = "http://localhost:3000/mcp" }

[tools]
include = [
    "file:*",
    "claude-skills:search", "claude-skills:analyze",
    "web-tools:fetch", "web-tools:scrape",
    "wg:show", "wg:log"
]
exclude = ["file:write", "bash"]

context_scope = "task"
```

### Migration Path
1. **Phase 1**: Existing bundles continue to work unchanged
2. **Phase 2**: Add `tool-sources` section support
3. **Phase 3**: Deprecate old format in favor of `tools.toml`

## Tool Discovery and Resolution

### Resolution Priority
1. **Task-specific requirements**: Tools specified in `wg add --tools`
2. **Task scope patterns**: Matching patterns in `[task-scopes]`
3. **Bundle assignment**: From `wg add --bundle` or bundle inheritance
4. **Executor defaults**: Default tools from `[defaults]` section
5. **Global fallback**: `builtin:implementer` bundle

### Resolution Algorithm
```rust
// src/executor/tool_resolver.rs
pub struct ToolResolver {
    manifest: ToolManifest,
    registry: ToolRegistry,
}

impl ToolResolver {
    pub fn resolve_for_task(&self, task: &Task, executor_type: ExecutorType) -> Result<ToolSet> {
        let mut tools = ToolSet::new();
        
        // 1. Task-specific tools
        if let Some(task_tools) = &task.required_tools {
            tools.extend(self.resolve_tool_references(task_tools)?);
        }
        
        // 2. Task scope patterns
        for (pattern, scope) in &self.manifest.task_scopes {
            if glob_match(pattern, &task.id) {
                if let Some(bundle) = &scope.bundle {
                    tools.extend(self.resolve_bundle(bundle)?);
                }
                if let Some(scope_tools) = &scope.tools {
                    tools.extend(self.resolve_tool_references(scope_tools)?);
                }
                if let Some(excludes) = &scope.exclude {
                    tools.exclude(&self.resolve_tool_references(excludes)?);
                }
            }
        }
        
        // 3. Executor defaults
        if tools.is_empty() {
            if let Some(default) = self.manifest.defaults.get(&executor_type) {
                tools.extend(self.resolve_tool_selection(default)?);
            }
        }
        
        // 4. Global fallback
        if tools.is_empty() {
            tools.extend(self.resolve_bundle("builtin:implementer")?);
        }
        
        Ok(tools)
    }
}
```

## Error Handling and Fallbacks

### MCP Server Failures
```rust
// src/executor/native/tools/mcp.rs
impl MCPToolSource {
    async fn initialize(&mut self) -> Result<()> {
        match self.connect().await {
            Ok(_) => Ok(()),
            Err(e) => {
                eprintln!("Warning: MCP server {} unavailable: {}", self.server_url, e);
                self.mark_unavailable();
                Ok(()) // Continue without this source
            }
        }
    }
}
```

### Tool Not Found Handling
```rust
pub fn resolve_tool_reference(&self, reference: &str) -> Result<Vec<Tool>> {
    match self.registry.get_by_reference(reference) {
        Some(tools) => Ok(tools),
        None => {
            eprintln!("Warning: Tool '{}' not found, skipping", reference);
            Ok(vec![]) // Skip missing tools rather than failing
        }
    }
}
```

### Backwards Compatibility
- **No manifest**: Use current hardcoded tool registration
- **Invalid manifest**: Log warning and fall back to defaults
- **Missing tool sources**: Skip unavailable sources, continue with others
- **Bundle inheritance**: Support both old `.toml` files and new manifest format

## Performance Considerations

### Tool Registration Caching
```rust
// Cache tool registries per manifest hash
pub struct ToolRegistryCache {
    cache: HashMap<u64, ToolRegistry>,
}

impl ToolRegistryCache {
    pub async fn get_or_create(&mut self, manifest: &ToolManifest) -> Result<&ToolRegistry> {
        let hash = manifest.hash();
        
        if !self.cache.contains_key(&hash) {
            let registry = ToolRegistry::from_manifest(manifest, &context).await?;
            self.cache.insert(hash, registry);
        }
        
        Ok(self.cache.get(&hash).unwrap())
    }
}
```

### Lazy MCP Connections
- Connect to MCP servers only when their tools are first used
- Maintain connection pools for frequently used servers
- Implement connection retry logic with backoff

### Bundle Resolution Caching
- Cache resolved tool sets per bundle configuration
- Invalidate cache when manifest changes
- Pre-compute common bundle combinations