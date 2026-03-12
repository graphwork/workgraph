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
pub mod bundle;
pub mod client;
pub mod openai_client;
pub mod provider;
pub mod tools;
