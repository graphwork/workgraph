#![recursion_limit = "256"]

pub mod graph;
pub mod parser;
pub mod query;
pub mod check;
pub mod config;
pub mod executors;
#[cfg(feature = "matrix")]
pub mod matrix;
#[cfg(feature = "matrix-lite")]
pub mod matrix_lite;
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
#[cfg(feature = "matrix")]
pub use matrix::{MatrixClient, IncomingMessage, VerificationEvent};
#[cfg(feature = "matrix")]
pub use matrix::commands::{MatrixCommand, help_text as matrix_help_text};
#[cfg(feature = "matrix")]
pub use matrix::listener::{MatrixListener, ListenerConfig, run_listener};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::{MatrixClient as MatrixClientLite, IncomingMessage as IncomingMessageLite, send_notification, send_notification_to_room};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::commands::{MatrixCommand as MatrixCommandLite, help_text as matrix_lite_help_text};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::listener::{MatrixListener as MatrixListenerLite, ListenerConfig as ListenerConfigLite, run_listener as run_listener_lite};
