//! Native executor: Rust-native LLM client with tool-use loop.
//!
//! Supports multiple LLM providers through the `Provider` trait:
//! - Anthropic Messages API (`client.rs`)
//! - OpenAI-compatible APIs (`openai_client.rs`) — OpenRouter, OpenAI, Ollama, etc.
//!
//! Use `provider::create_provider()` to route a model string to the right backend.
//! Executes tools in-process. Eliminates external dependencies on
//! Claude CLI or Amplifier for agent execution.

pub mod agent;
pub mod background;
pub mod bundle;
pub mod cancel;
pub mod channel;
pub mod client;
pub mod inbox;
pub mod journal;
pub mod openai_client;
pub mod provider;
pub mod resume;
pub mod state_injection;
pub mod tools;
