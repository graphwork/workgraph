pub mod graph;
pub mod parser;
pub mod query;
pub mod check;
pub mod config;
pub mod executors;
pub mod service;

pub use graph::{WorkGraph, Node, NodeKind, Task, Actor, Resource, Estimate};
pub use parser::{load_graph, save_graph};
pub use query::{ready_tasks, blocked_by, cost_of};
pub use check::{check_cycles, check_orphans, CheckResult};
pub use config::Config;
pub use service::{
    AgentHandle, AgentEntry, AgentRegistry, AgentStatus, ClaudeExecutor, ClaudeExecutorConfig,
    DefaultExecutor, Executor, ExecutorConfig, ExecutorRegistry, ExecutorSettings, LockedRegistry,
    PromptTemplate, TemplateVars, spawn_claude_agent, DEFAULT_CLAUDE_PROMPT,
};
