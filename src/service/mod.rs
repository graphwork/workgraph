//! Agent service layer
//!
//! Provides agent registry and management for the workgraph agent service.
//!
//! This module includes:
//! - Executor plugins for spawning agents (Claude, shell, custom)
//! - Agent registry for tracking running agents
//! - Output routing and artifact management
//! - Health monitoring and heartbeat tracking

pub mod claude;
pub mod executor;
pub mod registry;

pub use claude::{ClaudeExecutor, ClaudeExecutorConfig, spawn_claude_agent, DEFAULT_CLAUDE_PROMPT};
pub use executor::{
    AgentHandle, DefaultExecutor, Executor, ExecutorConfig, ExecutorRegistry, ExecutorSettings,
    PromptTemplate, TemplateVars,
};
pub use registry::{AgentEntry, AgentRegistry, AgentStatus, LockedRegistry};
