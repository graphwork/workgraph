# Migration Path: Universal Tool Manifest Implementation

This document outlines the step-by-step implementation plan for the universal tool manifest system, ensuring backwards compatibility and gradual rollout.

## Implementation Phases

### Phase 1: Foundation (Backwards Compatible)
**Objective**: Add manifest support without changing existing behavior

#### 1.1 Core Data Structures
```rust
// src/manifest/mod.rs - New module
pub struct ToolManifest {
    pub meta: ManifestMeta,
    pub sources: HashMap<String, ToolSource>,
    pub bundles: HashMap<String, ToolBundle>,
    pub task_scopes: HashMap<String, TaskScope>,
    pub defaults: HashMap<ExecutorType, ToolSelection>,
}

pub struct ToolSource {
    pub source_type: ToolSourceType,
    pub config: SourceConfig,
    pub description: Option<String>,
}

pub enum ToolSourceType {
    Builtin { tools: Vec<String> },
    Mcp { server_url: String, config_path: Option<String>, tools: Option<Vec<String>> },
    Directory { path: String, recursive: bool },
    Bundle { bundle: String, source: Option<String> },
}
```

**Files to create:**
- `src/manifest/mod.rs` - Core manifest types
- `src/manifest/parser.rs` - TOML parsing logic  
- `src/manifest/resolver.rs` - Tool resolution logic
- `schemas/tool-manifest.schema.json` - JSON Schema validation

#### 1.2 Manifest Loading
```rust
// src/manifest/loader.rs
impl ToolManifest {
    pub fn load_from_project(workgraph_dir: &Path) -> Result<Option<ToolManifest>> {
        let manifest_path = workgraph_dir.join("tools.toml");
        
        if manifest_path.exists() {
            let content = std::fs::read_to_string(&manifest_path)?;
            let manifest: ToolManifest = toml::from_str(&content)?;
            manifest.validate()?;  // Against JSON Schema
            Ok(Some(manifest))
        } else {
            Ok(None)  // No manifest = current behavior
        }
    }
}
```

#### 1.3 Update Native Executor
```rust
// src/executor/native/tools/mod.rs
pub fn default_all(working_dir: &Path, workgraph_dir: &Path) -> ToolRegistry {
    // Try manifest first, fall back to hardcoded
    if let Ok(Some(manifest)) = ToolManifest::load_from_project(workgraph_dir) {
        match ToolRegistry::from_manifest(&manifest, working_dir, workgraph_dir) {
            Ok(registry) => return registry,
            Err(e) => {
                eprintln!("Warning: Failed to load tools from manifest: {}", e);
                eprintln!("Falling back to default tool set");
            }
        }
    }
    
    // Existing hardcoded behavior unchanged
    let mut registry = ToolRegistry::new();
    file::register_file_tools(&mut registry);
    bash::register_bash_tool(&mut registry, working_dir);
    wg::register_wg_tools(&mut registry, workgraph_dir);
    web_search::register_web_search_tool(&mut registry);
    bg::register_bg_tool(&mut registry, workgraph_dir);
    registry
}
```

**Validation criteria:**
- Existing projects work unchanged (no `tools.toml` = current behavior)
- New projects can opt-in with `tools.toml`
- All tests pass with no manifest present
- Schema validation catches malformed manifests

---

### Phase 2: MCP Integration
**Objective**: Add MCP client support for tool discovery

#### 2.1 MCP Client Library
```rust
// src/mcp/client.rs - New module
pub struct MCPClient {
    transport: Box<dyn MCPTransport>,
    capabilities: MCPCapabilities,
}

pub trait MCPTransport: Send + Sync {
    async fn call(&self, method: &str, params: serde_json::Value) -> Result<serde_json::Value>;
    async fn connect(&mut self) -> Result<()>;
    fn is_connected(&self) -> bool;
}

// Implementations
pub struct StdioTransport { /* process management */ }
pub struct HttpTransport { /* HTTP client */ }  
pub struct UnixSocketTransport { /* unix socket */ }

impl MCPClient {
    pub async fn discover_tools(&self) -> Result<Vec<MCPToolDefinition>> {
        let response = self.transport.call("tools/list", json!({})).await?;
        serde_json::from_value(response)
    }
    
    pub async fn call_tool(&self, name: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        let request = json!({ "name": name, "arguments": params });
        self.transport.call("tools/call", request).await
    }
}
```

#### 2.2 MCP Tool Source Implementation
```rust
// src/manifest/sources/mcp.rs
pub struct MCPToolSource {
    client: MCPClient,
    tools: Vec<MCPTool>,
    config: MCPSourceConfig,
}

impl MCPToolSource {
    pub async fn initialize(config: MCPSourceConfig) -> Result<Self> {
        let transport = match config.server_url.as_str() {
            url if url.starts_with("stdio:") => {
                StdioTransport::new(&url[6..]).await?
            },
            url if url.starts_with("http") => {
                HttpTransport::new(url).await?
            },
            url if url.starts_with("unix://") => {
                UnixSocketTransport::new(&url[7..]).await?
            },
            _ => return Err(anyhow!("Unsupported MCP server URL: {}", config.server_url)),
        };
        
        let client = MCPClient::new(transport);
        let tools = client.discover_tools().await?
            .into_iter()
            .filter(|t| config.tools.is_none() || config.tools.as_ref().unwrap().contains(&t.name))
            .map(MCPTool::from_definition)
            .collect();
            
        Ok(Self { client, tools, config })
    }
}
```

#### 2.3 Registry Integration
```rust
// src/executor/native/tools/registry.rs
impl ToolRegistry {
    pub async fn from_manifest(
        manifest: &ToolManifest, 
        working_dir: &Path, 
        workgraph_dir: &Path
    ) -> Result<Self> {
        let mut registry = ToolRegistry::new();
        
        for (source_name, source_config) in &manifest.sources {
            match &source_config.source_type {
                ToolSourceType::Builtin { tools } => {
                    register_builtin_tools(&mut registry, tools, working_dir, workgraph_dir)?;
                }
                ToolSourceType::Mcp { server_url, config_path, tools } => {
                    let mcp_config = MCPSourceConfig {
                        server_url: server_url.clone(),
                        config_path: config_path.clone(),
                        tools: tools.clone(),
                    };
                    
                    match MCPToolSource::initialize(mcp_config).await {
                        Ok(source) => {
                            for tool in source.tools {
                                let qualified_name = format!("{}:{}", source_name, tool.name());
                                registry.register_mcp_tool(qualified_name, tool);
                            }
                        }
                        Err(e) => {
                            eprintln!("Warning: Failed to initialize MCP source {}: {}", source_name, e);
                            continue; // Skip unavailable sources
                        }
                    }
                }
                _ => { /* Other source types for later phases */ }
            }
        }
        
        Ok(registry)
    }
}
```

**Validation criteria:**
- Can connect to MCP servers (stdio, HTTP, Unix socket)
- Discovers tools from MCP servers correctly
- Gracefully handles MCP server failures
- MCP tools integrate with existing tool registry
- Tools can be called via both `source:tool` and unqualified names

---

### Phase 3: Cross-Executor Compatibility  
**Objective**: Enable tool sharing across all executor types

#### 3.1 MCP Server Export
```rust
// src/commands/mcp.rs - New command group
pub async fn serve_command(args: ServeMCPArgs) -> Result<()> {
    let manifest = ToolManifest::load_from_project(&args.workgraph_dir)?
        .unwrap_or_else(|| ToolManifest::default());
        
    let registry = ToolRegistry::from_manifest(&manifest, &args.working_dir, &args.workgraph_dir).await?;
    
    let server = MCPServer::new(registry);
    
    match args.transport {
        MCPTransport::Stdio => server.serve_stdio().await?,
        MCPTransport::Http { port } => server.serve_http(port).await?,
        MCPTransport::Unix { socket_path } => server.serve_unix(&socket_path).await?,
    }
    
    Ok(())
}

// Usage: wg mcp serve --stdio
//        wg mcp serve --http 8080
//        wg mcp serve --unix /tmp/wg-mcp.sock
```

#### 3.2 Claude Executor Enhancement
```rust
// Update claude executor configuration to use workgraph MCP server
pub async fn configure_claude_mcp() -> Result<()> {
    let mcp_config_path = dirs::home_dir().unwrap().join(".claude/mcp.json");
    
    let mut config: serde_json::Value = if mcp_config_path.exists() {
        serde_json::from_str(&std::fs::read_to_string(&mcp_config_path)?)?
    } else {
        json!({ "mcpServers": {} })
    };
    
    // Add workgraph as MCP server
    config["mcpServers"]["workgraph"] = json!({
        "command": "wg",
        "args": ["mcp", "serve", "--stdio"]
    });
    
    std::fs::write(&mcp_config_path, serde_json::to_string_pretty(&config)?)?;
    
    println!("Added workgraph MCP server to Claude configuration");
    Ok(())
}
```

#### 3.3 Shell Tool Wrappers
```rust
// src/executor/shell/tool_wrappers.rs
pub fn generate_shell_wrappers(manifest: &ToolManifest, output_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;
    
    let resolver = ToolResolver::new(manifest);
    let shell_tools = resolver.resolve_for_executor(ExecutorType::Shell)?;
    
    for tool in shell_tools {
        let wrapper_content = format!(
            r#"#!/bin/bash
# Auto-generated wrapper for {tool_name}
# Usage: {tool_name} [JSON_PARAMS]

if [ "$#" -eq 0 ]; then
    wg tool describe "{tool_name}"
    exit 0
fi

wg tool call "{tool_name}" "$1"
"#,
            tool_name = tool.qualified_name()
        );
        
        let wrapper_path = output_dir.join(format!("{}.sh", tool.name()));
        std::fs::write(&wrapper_path, wrapper_content)?;
        
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&wrapper_path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    
    Ok(())
}

// Command: wg tools generate-wrappers --output ~/.local/bin/wg-tools
```

#### 3.4 Amplifier Routing Enhancement
```rust
// src/executor/amplifier/router.rs
impl AmplifierExecutor {
    pub async fn route_by_tools(&self, required_tools: &[String]) -> Result<ExecutorType> {
        let tool_analysis = ToolAnalyzer::new(&self.manifest).analyze(required_tools)?;
        
        // Route based on tool requirements
        match tool_analysis {
            _ if tool_analysis.has_wg_tools() => Ok(ExecutorType::Native),
            _ if tool_analysis.is_web_heavy() => Ok(ExecutorType::Claude), 
            _ if tool_analysis.is_bash_only() => Ok(ExecutorType::Shell),
            _ if tool_analysis.requires_mcp() => Ok(ExecutorType::Native),
            _ => Ok(ExecutorType::Native), // Default
        }
    }
}
```

**Validation criteria:**
- Workgraph tools available in Claude via MCP
- Shell executor can use structured tools via wrappers
- Amplifier executor routes based on tool requirements
- Cross-executor tool sharing works seamlessly

---

### Phase 4: Enhanced Features
**Objective**: Advanced tool management capabilities

#### 4.1 Tool CLI Interface
```bash
# Tool discovery and information
wg tool list                    # List all available tools
wg tool list --source mcp-*     # List tools from MCP sources
wg tool describe file:read      # Show tool definition and usage
wg tool test file:read          # Test tool with sample input

# Tool execution
wg tool call file:read '{"file_path": "/path/to/file"}'
wg tool call --interactive web_search  # Interactive parameter input

# Tool management
wg tool sources                 # List configured tool sources
wg tool sources add mcp-example "http://localhost:3000/mcp"
wg tool sources remove mcp-example
wg tool sources test mcp-example  # Test source connectivity

# Bundle management
wg bundle list                  # List available bundles
wg bundle show research         # Show bundle contents
wg bundle create my-bundle --tools "file:*,web_search" --exclude "file:write"
wg bundle test my-bundle        # Validate bundle configuration
```

#### 4.2 Tool Performance Monitoring
```rust
// src/tools/monitor.rs
pub struct ToolUsageMonitor {
    metrics: HashMap<String, ToolMetrics>,
}

pub struct ToolMetrics {
    pub call_count: u64,
    pub total_duration: Duration,
    pub success_rate: f64,
    pub error_count: u64,
}

impl ToolUsageMonitor {
    pub async fn record_call(&mut self, tool_name: &str, duration: Duration, success: bool) {
        let metrics = self.metrics.entry(tool_name.to_string()).or_default();
        metrics.call_count += 1;
        metrics.total_duration += duration;
        if !success {
            metrics.error_count += 1;
        }
        metrics.success_rate = 1.0 - (metrics.error_count as f64 / metrics.call_count as f64);
    }
    
    pub fn generate_report(&self) -> ToolUsageReport {
        // Generate performance report for optimization
    }
}
```

#### 4.3 Tool Versioning
```toml
# Enhanced tools.toml with versioning
[sources.web-tools]
type = "mcp"
server_url = "http://localhost:3000/mcp"
version_constraints = "^1.2.0"
compatibility_check = true

[tools.compatibility]
"file:read" = { min_version = "1.0.0", deprecation_warning = "Use file:read_v2" }
"web_search" = { max_version = "2.0.0", replacement = "web-tools:search" }
```

#### 4.4 Dynamic Tool Loading
```rust
// Hot-reloading of tool sources without restart
impl ToolRegistry {
    pub async fn reload_source(&mut self, source_name: &str) -> Result<()> {
        // Gracefully unload existing tools from source
        self.unregister_source(source_name);
        
        // Reload from updated configuration
        let manifest = ToolManifest::load_from_project(&self.workgraph_dir)?
            .ok_or_else(|| anyhow!("No manifest found"))?;
            
        if let Some(source_config) = manifest.sources.get(source_name) {
            self.register_source(source_name, source_config).await?;
        }
        
        Ok(())
    }
}

// Command: wg tool reload mcp-webtools
```

**Validation criteria:**
- Comprehensive CLI tool interface
- Tool performance monitoring and reporting
- Version compatibility checking
- Hot-reloading without service restart

## Testing Strategy

### Unit Tests
- Manifest parsing and validation
- Tool resolution logic
- MCP client functionality
- Bundle inheritance and merging

### Integration Tests  
- End-to-end tool execution across executor types
- MCP server connectivity and tool discovery
- Cross-executor tool sharing
- Backwards compatibility with existing projects

### Performance Tests
- Tool registry loading time with large manifests
- MCP client connection pooling and reuse
- Tool resolution caching effectiveness
- Memory usage with multiple MCP sources

## Deployment Strategy

### Development Environment
1. Feature flags for each phase (`--experimental-tool-manifest`)
2. Separate test workspaces for each integration type
3. Docker containers for MCP server testing

### Production Rollout
1. **Opt-in Phase**: Projects manually add `tools.toml` to enable
2. **Default Phase**: New projects get example `tools.toml`
3. **Migration Phase**: Tool to convert existing bundles to manifest format
4. **Deprecation Phase**: Old bundle system marked deprecated
5. **Removal Phase**: Remove old system after migration period

### Rollback Plan
- Feature flags allow disabling new system
- Fallback to hardcoded tools always available  
- Migration tool can reverse manifest → bundle conversion
- No breaking changes to existing tool interfaces

## Risk Mitigation

### MCP Server Dependencies
- **Risk**: External MCP servers unavailable
- **Mitigation**: Graceful degradation, continue with available tools

### Performance Impact
- **Risk**: Tool resolution overhead
- **Mitigation**: Caching, lazy loading, profiling

### Configuration Complexity
- **Risk**: Complex manifests become unwieldy  
- **Mitigation**: Schema validation, examples, defaults

### Cross-Executor Compatibility
- **Risk**: Tool behavior differences between executors
- **Mitigation**: Standardized testing, compatibility matrix

## Success Metrics

### Functionality
- [ ] All current tools available via manifest system
- [ ] MCP integration with popular servers (Claude skills, web tools)
- [ ] Cross-executor tool sharing working
- [ ] 100% backwards compatibility maintained

### Performance
- [ ] Tool registry loading < 100ms for typical manifests
- [ ] MCP tool calls within 10% of native tool performance
- [ ] Memory usage increase < 20% from current baseline

### Usability
- [ ] Example manifests cover 80% of common use cases
- [ ] Migration from bundles → manifest automated
- [ ] Clear error messages for configuration issues
- [ ] Documentation covers all integration patterns