//! Agent inbox: incoming user input drained at every turn boundary.
//!
//! One abstraction over two delivery surfaces:
//!
//! - **TUI** (`wg nex` interactive session): the composing buffer at
//!   the bottom of the terminal feeds the inbox when the user hits
//!   Enter. Stage E wires this up.
//! - **Workgraph IPC** (headless dispatch): `wg send <agent-id>
//!   "message"` appends to a file that the agent tails. Stage F
//!   wires this up.
//!
//! Stage B — this stage — introduces the trait and an in-memory
//! implementation, and wires the `drain()` call into the run loop's
//! turn boundary. No delivery surface is producing yet, so the
//! drain is effectively a no-op at runtime. The turn-boundary
//! placement is what matters: by the time Stage E/F plug in real
//! producers, the consumer side is already integrated.
//!
//! Two levels of input:
//!
//! - **Note**: appended to the next user turn. Does not cancel
//!   in-flight work. Typical path for "here's extra context" or
//!   "one more thing" messages typed during agent work.
//! - **Interrupt**: same as Note, but also trips the cooperative
//!   cancel. The in-flight tool/LLM call is aborted at the next
//!   boundary, and the message becomes the next user turn. Typical
//!   path for "stop doing that, try X instead" from either a human
//!   or a workgraph coordinator.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;

use super::cancel::CancelToken;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UserInput {
    /// Non-interrupting message. Delivered to the agent at the next
    /// turn boundary; the current LLM call and in-flight tool finish
    /// cleanly first.
    Note(String),
    /// Interrupting message. Sets the cooperative cancel flag on
    /// delivery so the current work aborts at its next select!
    /// checkpoint; the message then becomes the next user turn.
    Interrupt(String),
}

impl UserInput {
    pub fn text(&self) -> &str {
        match self {
            UserInput::Note(s) | UserInput::Interrupt(s) => s.as_str(),
        }
    }

    pub fn is_interrupt(&self) -> bool {
        matches!(self, UserInput::Interrupt(_))
    }
}

#[async_trait]
pub trait AgentInbox: Send + Sync {
    /// Non-blocking drain of any accumulated user inputs. Called at
    /// every turn boundary. Returning an empty vec is the common case
    /// and must be cheap.
    async fn drain(&mut self) -> Vec<UserInput>;
}

/// In-memory inbox backed by a Mutex-guarded VecDeque. Used by the
/// TUI in Stage E and by tests everywhere. Cheap to construct; the
/// `handle()` method returns a clone-able producer-side handle so
/// other threads/tasks can push without holding the inbox itself.
#[derive(Default, Clone)]
pub struct InMemoryInbox {
    queue: Arc<Mutex<VecDeque<UserInput>>>,
}

impl InMemoryInbox {
    pub fn new() -> Self {
        Self::default()
    }

    /// Push a message into the inbox. If it's an `Interrupt`, also
    /// flips the supplied cancel token's cooperative flag so the
    /// in-flight work aborts at its next boundary.
    pub fn push(&self, input: UserInput, cancel: Option<&CancelToken>) {
        let is_interrupt = input.is_interrupt();
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(input);
        }
        if is_interrupt
            && let Some(token) = cancel
        {
            token.request_cooperative();
        }
    }

    /// Snapshot length — for tests + diagnostics.
    pub fn len(&self) -> usize {
        self.queue.lock().map(|q| q.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

#[async_trait]
impl AgentInbox for InMemoryInbox {
    async fn drain(&mut self) -> Vec<UserInput> {
        match self.queue.lock() {
            Ok(mut q) => q.drain(..).collect(),
            Err(_) => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn drain_returns_in_fifo_order() {
        let mut inbox = InMemoryInbox::new();
        inbox.push(UserInput::Note("first".into()), None);
        inbox.push(UserInput::Note("second".into()), None);
        inbox.push(UserInput::Note("third".into()), None);
        let drained = inbox.drain().await;
        assert_eq!(drained.len(), 3);
        assert_eq!(drained[0].text(), "first");
        assert_eq!(drained[2].text(), "third");
    }

    #[tokio::test]
    async fn drain_clears_the_queue() {
        let mut inbox = InMemoryInbox::new();
        inbox.push(UserInput::Note("x".into()), None);
        assert_eq!(inbox.len(), 1);
        let _ = inbox.drain().await;
        assert!(inbox.is_empty());
    }

    #[tokio::test]
    async fn drain_empty_is_ok() {
        let mut inbox = InMemoryInbox::new();
        let drained = inbox.drain().await;
        assert!(drained.is_empty());
    }

    #[tokio::test]
    async fn interrupt_push_flips_cancel() {
        let inbox = InMemoryInbox::new();
        let cancel = CancelToken::new();
        assert!(!cancel.is_cooperative());
        inbox.push(
            UserInput::Interrupt("stop that".into()),
            Some(&cancel),
        );
        assert!(cancel.is_cooperative());
    }

    #[tokio::test]
    async fn note_push_does_not_flip_cancel() {
        let inbox = InMemoryInbox::new();
        let cancel = CancelToken::new();
        inbox.push(UserInput::Note("fyi".into()), Some(&cancel));
        assert!(!cancel.is_cooperative());
    }

    #[tokio::test]
    async fn is_interrupt_discriminates() {
        assert!(!UserInput::Note("n".into()).is_interrupt());
        assert!(UserInput::Interrupt("i".into()).is_interrupt());
    }
}
