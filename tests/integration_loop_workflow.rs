//! End-to-end integration tests for the full loop edge workflow via wg CLI.
//!
//! NOTE: The LoopEdge struct, Task.loops_to field, and related CLI commands
//! (--loops-to, --loop-max, wg loops) have been removed. Loop functionality
//! is now handled through structural cycles (after edges + CycleConfig).
//! All loop-edge-specific workflow tests have been removed.
