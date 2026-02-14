pub mod agency;
pub mod check;
pub mod config;
pub mod graph;
#[cfg(feature = "matrix")]
pub mod matrix;
pub mod matrix_commands;
#[cfg(feature = "matrix-lite")]
pub mod matrix_lite;
pub mod parser;
pub mod query;
pub mod service;
pub mod usage;

pub use config::MatrixConfig;
pub use graph::WorkGraph;
#[cfg(feature = "matrix")]
pub use matrix::commands::{MatrixCommand, help_text as matrix_help_text};
#[cfg(feature = "matrix")]
pub use matrix::listener::{ListenerConfig, MatrixListener, run_listener};
#[cfg(feature = "matrix")]
pub use matrix::{IncomingMessage, MatrixClient, VerificationEvent};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::commands::{
    MatrixCommand as MatrixCommandLite, help_text as matrix_lite_help_text,
};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::listener::{
    ListenerConfig as ListenerConfigLite, MatrixListener as MatrixListenerLite,
    run_listener as run_listener_lite,
};
#[cfg(feature = "matrix-lite")]
pub use matrix_lite::{
    IncomingMessage as IncomingMessageLite, MatrixClient as MatrixClientLite, send_notification,
    send_notification_to_room,
};
pub use parser::{load_graph, save_graph};
pub use service::{AgentEntry, AgentRegistry, AgentStatus};
