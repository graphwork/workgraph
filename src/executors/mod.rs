//! Executor implementations for workgraph.
//!
//! This module provides convenient re-exports of executors from the service layer.
//! The actual implementations live in `src/service/` for better organization.

pub mod claude;
pub mod shell;
