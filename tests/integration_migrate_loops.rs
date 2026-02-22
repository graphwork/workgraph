//! Integration tests for the loops_to -> structural cycles migration (Phase 3).
//!
//! NOTE: The LoopEdge struct, Task.loops_to field, and the migrate_loops
//! command have been removed. The migration is complete -- loops are now
//! handled entirely through structural cycles (after edges + CycleConfig).
//! All migration-specific tests have been removed.
