#![recursion_limit = "256"]

pub mod graph;
pub mod parser;
pub mod query;
pub mod check;
pub mod config;
pub mod executors;
pub mod matrix;
pub mod service;

pub use graph::{WorkGraph, Node, NodeKind, Task, Actor, ActorType, Resource, Estimate, ResponseTime};
pub use parser::{load_graph, save_graph};
pub use query::{ready_tasks, blocked_by, cost_of};
pub use check::{check_cycles, check_orphans, CheckResult};
pub use config::{Config, MatrixConfig};
pub use service::{
    AgentHandle, AgentEntry, AgentRegistry, AgentStatus, ClaudeExecutor, ClaudeExecutorConfig,
    DefaultExecutor, Executor, ExecutorConfig, ExecutorRegistry, ExecutorSettings, LockedRegistry,
    PromptTemplate, ShellExecutor, ShellExecutorConfig, TemplateVars, spawn_claude_agent,
    spawn_shell_agent, DEFAULT_CLAUDE_PROMPT,
};
pub use matrix::{MatrixClient, IncomingMessage, VerificationEvent};
pub use matrix::commands::{MatrixCommand, help_text as matrix_help_text};
pub use matrix::listener::{MatrixListener, ListenerConfig, run_listener};
