//! Agent service layer
//!
//! Provides agent registry and management for the workgraph agent service.
//!
//! This module includes:
//! - Executor configuration for spawning agents
//! - Agent registry for tracking running agents

pub mod executor;
pub mod registry;

pub use executor::{
    ExecutorConfig, ExecutorRegistry, ExecutorSettings,
    PromptTemplate, TemplateVars,
};
pub use registry::{AgentEntry, AgentRegistry, AgentStatus, LockedRegistry};
