//! Integration tests for tool execution parallelism.
//!
//! Verifies that:
//! 1. Tools are correctly classified as read-only vs mutating
//! 2. Read-only tools execute in parallel via execute_batch
//! 3. Mutating tools execute serially
//! 4. Mixed batches: reads first (parallel), then writes (serial)
//! 5. Concurrency cap is enforced
//! 6. Results maintain original call order regardless of execution order

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde_json::json;
use tokio::sync::Barrier;

use workgraph::executor::native::client::ToolDefinition;
use workgraph::executor::native::tools::{
    Tool, ToolCall, ToolOutput, ToolRegistry, DEFAULT_MAX_CONCURRENT_TOOLS,
};

// ── Test tools ─────────────────────────────────────────────────────────

/// A read-only tool that sleeps for a configurable duration, tracking
/// execution order and concurrency.
struct SlowReadTool {
    name: String,
    delay: Duration,
    exec_counter: Arc<AtomicUsize>,
    max_concurrent: Arc<AtomicUsize>,
    current_concurrent: Arc<AtomicUsize>,
}

impl SlowReadTool {
    fn new(name: &str, delay: Duration, exec_counter: Arc<AtomicUsize>) -> Self {
        Self {
            name: name.to_string(),
            delay,
            exec_counter,
            max_concurrent: Arc::new(AtomicUsize::new(0)),
            current_concurrent: Arc::new(AtomicUsize::new(0)),
        }
    }

    fn with_concurrency_tracking(
        name: &str,
        delay: Duration,
        exec_counter: Arc<AtomicUsize>,
        max_concurrent: Arc<AtomicUsize>,
        current_concurrent: Arc<AtomicUsize>,
    ) -> Self {
        Self {
            name: name.to_string(),
            delay,
            exec_counter,
            max_concurrent,
            current_concurrent,
        }
    }
}

#[async_trait]
impl Tool for SlowReadTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "Test read-only tool".to_string(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _input: &serde_json::Value) -> ToolOutput {
        let order = self.exec_counter.fetch_add(1, Ordering::SeqCst);
        let current = self.current_concurrent.fetch_add(1, Ordering::SeqCst) + 1;
        // Update max observed concurrency
        self.max_concurrent.fetch_max(current, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        self.current_concurrent.fetch_sub(1, Ordering::SeqCst);
        ToolOutput::success(format!("read-{}-order-{}", self.name, order))
    }
}

/// A mutating tool that tracks execution order.
struct MutatingTool {
    name: String,
    delay: Duration,
    exec_counter: Arc<AtomicUsize>,
}

impl MutatingTool {
    fn new(name: &str, delay: Duration, exec_counter: Arc<AtomicUsize>) -> Self {
        Self {
            name: name.to_string(),
            delay,
            exec_counter,
        }
    }
}

#[async_trait]
impl Tool for MutatingTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "Test mutating tool".to_string(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn is_read_only(&self) -> bool {
        false
    }

    async fn execute(&self, _input: &serde_json::Value) -> ToolOutput {
        let order = self.exec_counter.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(self.delay).await;
        ToolOutput::success(format!("write-{}-order-{}", self.name, order))
    }
}

/// A read-only tool that uses a barrier to prove concurrent execution.
struct BarrierReadTool {
    name: String,
    barrier: Arc<Barrier>,
}

#[async_trait]
impl Tool for BarrierReadTool {
    fn name(&self) -> &str {
        &self.name
    }

    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name.clone(),
            description: "Barrier test tool".to_string(),
            input_schema: json!({"type": "object"}),
        }
    }

    fn is_read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _input: &serde_json::Value) -> ToolOutput {
        // This will deadlock if tools run serially — only completes
        // when all barrier participants arrive simultaneously.
        self.barrier.wait().await;
        ToolOutput::success(format!("barrier-{}", self.name))
    }
}

// ── Tests ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn test_tool_parallel_read_only_classification() {
    // Verify the built-in tools are classified correctly
    let tmp = tempfile::TempDir::new().unwrap();
    let registry = ToolRegistry::default_all(tmp.path(), tmp.path());

    // Read-only tools
    assert!(registry.is_read_only("read_file"), "read_file should be read-only");
    assert!(registry.is_read_only("glob"), "glob should be read-only");
    assert!(registry.is_read_only("grep"), "grep should be read-only");
    assert!(registry.is_read_only("wg_show"), "wg_show should be read-only");
    assert!(registry.is_read_only("wg_list"), "wg_list should be read-only");

    // Mutating tools
    assert!(!registry.is_read_only("write_file"), "write_file should be mutating");
    assert!(!registry.is_read_only("edit_file"), "edit_file should be mutating");
    assert!(!registry.is_read_only("bash"), "bash should be mutating");
    assert!(!registry.is_read_only("wg_add"), "wg_add should be mutating");
    assert!(!registry.is_read_only("wg_done"), "wg_done should be mutating");
    assert!(!registry.is_read_only("wg_fail"), "wg_fail should be mutating");
    assert!(!registry.is_read_only("wg_log"), "wg_log should be mutating");
    assert!(!registry.is_read_only("wg_artifact"), "wg_artifact should be mutating");

    // Unknown tools default to mutating (conservative)
    assert!(!registry.is_read_only("nonexistent_tool"), "unknown tools should be mutating");
}

#[tokio::test]
async fn test_tool_parallel_reads_execute_concurrently() {
    // 3 read-only tools each sleeping 50ms should complete in ~50ms (parallel),
    // not ~150ms (serial).
    let counter = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let current_concurrent = Arc::new(AtomicUsize::new(0));

    let mut registry = ToolRegistry::new();
    for i in 0..3 {
        registry.register(Box::new(SlowReadTool::with_concurrency_tracking(
            &format!("read_{}", i),
            Duration::from_millis(50),
            counter.clone(),
            max_concurrent.clone(),
            current_concurrent.clone(),
        )));
    }

    let calls: Vec<ToolCall> = (0..3)
        .map(|i| ToolCall {
            name: format!("read_{}", i),
            input: json!({}),
        })
        .collect();

    let start = Instant::now();
    let results = registry.execute_batch(&calls, 10).await;
    let elapsed = start.elapsed();

    // All 3 should complete
    assert_eq!(results.len(), 3);
    for r in &results {
        assert!(!r.output.is_error, "tool {} failed: {}", r.name, r.output.content);
    }

    // Should have run concurrently: elapsed < 100ms (not 150ms serial)
    assert!(
        elapsed < Duration::from_millis(120),
        "Parallel reads took {:?}, expected < 120ms (3x50ms serial would be 150ms)",
        elapsed
    );

    // Peak concurrency should be > 1
    assert!(
        max_concurrent.load(Ordering::SeqCst) > 1,
        "Expected concurrent execution, but max concurrency was {}",
        max_concurrent.load(Ordering::SeqCst)
    );
}

#[tokio::test]
async fn test_tool_parallel_barrier_proves_concurrency() {
    // Use a barrier that requires all 3 tools to arrive simultaneously.
    // If tools ran serially, this would deadlock (timeout).
    let barrier = Arc::new(Barrier::new(3));

    let mut registry = ToolRegistry::new();
    for i in 0..3 {
        registry.register(Box::new(BarrierReadTool {
            name: format!("barrier_{}", i),
            barrier: barrier.clone(),
        }));
    }

    let calls: Vec<ToolCall> = (0..3)
        .map(|i| ToolCall {
            name: format!("barrier_{}", i),
            input: json!({}),
        })
        .collect();

    // This will deadlock (and timeout) if execution is serial.
    let results = tokio::time::timeout(
        Duration::from_secs(5),
        registry.execute_batch(&calls, 10),
    )
    .await
    .expect("Barrier test timed out — tools are not executing concurrently");

    assert_eq!(results.len(), 3);
    for r in &results {
        assert!(!r.output.is_error);
    }
}

#[tokio::test]
async fn test_tool_parallel_mutating_serial() {
    // Mutating tools must execute serially: execution order == call order.
    let counter = Arc::new(AtomicUsize::new(0));

    let mut registry = ToolRegistry::new();
    for i in 0..3 {
        registry.register(Box::new(MutatingTool::new(
            &format!("write_{}", i),
            Duration::from_millis(10),
            counter.clone(),
        )));
    }

    let calls: Vec<ToolCall> = (0..3)
        .map(|i| ToolCall {
            name: format!("write_{}", i),
            input: json!({}),
        })
        .collect();

    let results = registry.execute_batch(&calls, 10).await;

    assert_eq!(results.len(), 3);
    // Verify serial execution: order-0, order-1, order-2
    assert_eq!(results[0].output.content, "write-write_0-order-0");
    assert_eq!(results[1].output.content, "write-write_1-order-1");
    assert_eq!(results[2].output.content, "write-write_2-order-2");
}

#[tokio::test]
async fn test_tool_parallel_mixed_batch() {
    // Mixed batch: reads should execute first (in parallel), then writes (serially).
    let counter = Arc::new(AtomicUsize::new(0));

    let mut registry = ToolRegistry::new();
    // 2 read-only tools
    for i in 0..2 {
        registry.register(Box::new(SlowReadTool::new(
            &format!("read_{}", i),
            Duration::from_millis(10),
            counter.clone(),
        )));
    }
    // 2 mutating tools
    for i in 0..2 {
        registry.register(Box::new(MutatingTool::new(
            &format!("write_{}", i),
            Duration::from_millis(10),
            counter.clone(),
        )));
    }

    // Interleave read and write calls
    let calls = vec![
        ToolCall { name: "write_0".to_string(), input: json!({}) },
        ToolCall { name: "read_0".to_string(), input: json!({}) },
        ToolCall { name: "write_1".to_string(), input: json!({}) },
        ToolCall { name: "read_1".to_string(), input: json!({}) },
    ];

    let results = registry.execute_batch(&calls, 10).await;

    assert_eq!(results.len(), 4);

    // Results should be in original call order
    assert!(results[0].name == "write_0");
    assert!(results[1].name == "read_0");
    assert!(results[2].name == "write_1");
    assert!(results[3].name == "read_1");

    // Reads should have executed first (lower order numbers).
    // Extract the order numbers from outputs.
    let read_orders: Vec<usize> = results
        .iter()
        .filter(|r| r.name.starts_with("read"))
        .map(|r| {
            r.output.content
                .rsplit("order-")
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap()
        })
        .collect();
    let write_orders: Vec<usize> = results
        .iter()
        .filter(|r| r.name.starts_with("write"))
        .map(|r| {
            r.output.content
                .rsplit("order-")
                .next()
                .unwrap()
                .parse::<usize>()
                .unwrap()
        })
        .collect();

    // All read orders should be less than all write orders
    let max_read_order = *read_orders.iter().max().unwrap();
    let min_write_order = *write_orders.iter().min().unwrap();
    assert!(
        max_read_order < min_write_order,
        "Reads (orders {:?}) should execute before writes (orders {:?})",
        read_orders,
        write_orders
    );
}

#[tokio::test]
async fn test_tool_parallel_concurrency_cap() {
    // With concurrency cap of 2, only 2 tools should run simultaneously
    // even when 5 read-only tools are submitted.
    let counter = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let current_concurrent = Arc::new(AtomicUsize::new(0));

    let mut registry = ToolRegistry::new();
    for i in 0..5 {
        registry.register(Box::new(SlowReadTool::with_concurrency_tracking(
            &format!("read_{}", i),
            Duration::from_millis(50),
            counter.clone(),
            max_concurrent.clone(),
            current_concurrent.clone(),
        )));
    }

    let calls: Vec<ToolCall> = (0..5)
        .map(|i| ToolCall {
            name: format!("read_{}", i),
            input: json!({}),
        })
        .collect();

    let results = registry.execute_batch(&calls, 2).await;

    assert_eq!(results.len(), 5);

    // Max concurrency should be capped at 2
    let observed_max = max_concurrent.load(Ordering::SeqCst);
    assert!(
        observed_max <= 2,
        "Expected max concurrency <= 2, but observed {}",
        observed_max
    );
}

#[tokio::test]
async fn test_tool_parallel_result_order_preserved() {
    // Even though tools execute in parallel (potentially completing out of order),
    // results must be returned in the original call order.
    let counter = Arc::new(AtomicUsize::new(0));

    let mut registry = ToolRegistry::new();
    // Tool 0: slowest (completes last)
    registry.register(Box::new(SlowReadTool::new("slow", Duration::from_millis(80), counter.clone())));
    // Tool 1: fastest (completes first)
    registry.register(Box::new(SlowReadTool::new("fast", Duration::from_millis(10), counter.clone())));
    // Tool 2: medium
    registry.register(Box::new(SlowReadTool::new("medium", Duration::from_millis(40), counter.clone())));

    let calls = vec![
        ToolCall { name: "slow".to_string(), input: json!({}) },
        ToolCall { name: "fast".to_string(), input: json!({}) },
        ToolCall { name: "medium".to_string(), input: json!({}) },
    ];

    let results = registry.execute_batch(&calls, 10).await;

    // Results must match call order, NOT completion order
    assert_eq!(results[0].name, "slow");
    assert_eq!(results[1].name, "fast");
    assert_eq!(results[2].name, "medium");
}

#[tokio::test]
async fn test_tool_parallel_single_call_no_overhead() {
    // A single tool call should work identically to direct execute.
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(SlowReadTool::new("solo", Duration::from_millis(5), counter)));

    let calls = vec![ToolCall { name: "solo".to_string(), input: json!({}) }];
    let results = registry.execute_batch(&calls, 10).await;

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].name, "solo");
    assert!(!results[0].output.is_error);
}

#[tokio::test]
async fn test_tool_parallel_unknown_tool() {
    // Unknown tools should return errors gracefully.
    let registry = ToolRegistry::new();

    let calls = vec![ToolCall {
        name: "nonexistent".to_string(),
        input: json!({}),
    }];
    let results = registry.execute_batch(&calls, 10).await;

    assert_eq!(results.len(), 1);
    assert!(results[0].output.is_error);
    assert!(results[0].output.content.contains("Unknown tool"));
}

#[tokio::test]
async fn test_tool_parallel_empty_batch() {
    let registry = ToolRegistry::new();
    let results = registry.execute_batch(&[], 10).await;
    assert!(results.is_empty());
}

#[tokio::test]
async fn test_tool_parallel_all_reads_no_writes() {
    // Batch with only read-only tools should all run in parallel.
    let counter = Arc::new(AtomicUsize::new(0));
    let max_concurrent = Arc::new(AtomicUsize::new(0));
    let current_concurrent = Arc::new(AtomicUsize::new(0));

    let mut registry = ToolRegistry::new();
    for i in 0..4 {
        registry.register(Box::new(SlowReadTool::with_concurrency_tracking(
            &format!("r{}", i),
            Duration::from_millis(30),
            counter.clone(),
            max_concurrent.clone(),
            current_concurrent.clone(),
        )));
    }

    let calls: Vec<ToolCall> = (0..4)
        .map(|i| ToolCall { name: format!("r{}", i), input: json!({}) })
        .collect();

    let start = Instant::now();
    let results = registry.execute_batch(&calls, 10).await;
    let elapsed = start.elapsed();

    assert_eq!(results.len(), 4);
    // Should be ~30ms (parallel), not ~120ms (serial)
    assert!(
        elapsed < Duration::from_millis(80),
        "All-reads batch took {:?}, expected < 80ms",
        elapsed
    );
    assert!(max_concurrent.load(Ordering::SeqCst) > 1);
}

#[tokio::test]
async fn test_tool_parallel_all_writes_no_reads() {
    // Batch with only mutating tools should execute serially.
    let counter = Arc::new(AtomicUsize::new(0));

    let mut registry = ToolRegistry::new();
    for i in 0..3 {
        registry.register(Box::new(MutatingTool::new(
            &format!("w{}", i),
            Duration::from_millis(10),
            counter.clone(),
        )));
    }

    let calls: Vec<ToolCall> = (0..3)
        .map(|i| ToolCall { name: format!("w{}", i), input: json!({}) })
        .collect();

    let results = registry.execute_batch(&calls, 10).await;

    assert_eq!(results.len(), 3);
    // Verify strictly sequential execution
    assert_eq!(results[0].output.content, "write-w0-order-0");
    assert_eq!(results[1].output.content, "write-w1-order-1");
    assert_eq!(results[2].output.content, "write-w2-order-2");
}

#[tokio::test]
async fn test_tool_parallel_duration_tracking() {
    let counter = Arc::new(AtomicUsize::new(0));
    let mut registry = ToolRegistry::new();
    registry.register(Box::new(SlowReadTool::new("timed", Duration::from_millis(50), counter)));

    let calls = vec![ToolCall { name: "timed".to_string(), input: json!({}) }];
    let results = registry.execute_batch(&calls, 10).await;

    assert_eq!(results.len(), 1);
    // Duration should be >= 50ms (the sleep) and < 200ms (generous upper bound)
    assert!(
        results[0].duration_ms >= 40 && results[0].duration_ms < 200,
        "Expected duration ~50ms, got {}ms",
        results[0].duration_ms
    );
}

#[tokio::test]
async fn test_tool_parallel_default_concurrency_constant() {
    assert_eq!(DEFAULT_MAX_CONCURRENT_TOOLS, 10);
}

#[tokio::test]
async fn test_tool_parallel_speedup_benchmark() {
    // Benchmark: parallel should be measurably faster than serial for 5 reads.
    let counter = Arc::new(AtomicUsize::new(0));

    let mut registry = ToolRegistry::new();
    for i in 0..5 {
        registry.register(Box::new(SlowReadTool::new(
            &format!("bench_{}", i),
            Duration::from_millis(30),
            counter.clone(),
        )));
    }

    let calls: Vec<ToolCall> = (0..5)
        .map(|i| ToolCall { name: format!("bench_{}", i), input: json!({}) })
        .collect();

    // Parallel execution
    let start = Instant::now();
    let _results = registry.execute_batch(&calls, 10).await;
    let parallel_elapsed = start.elapsed();

    // Serial would be 5 * 30ms = 150ms minimum
    let serial_estimate = Duration::from_millis(150);

    assert!(
        parallel_elapsed < serial_estimate,
        "Parallel ({:?}) should be faster than serial estimate ({:?})",
        parallel_elapsed,
        serial_estimate
    );

    // Speedup should be at least 2x
    let speedup = serial_estimate.as_millis() as f64 / parallel_elapsed.as_millis().max(1) as f64;
    assert!(
        speedup > 2.0,
        "Expected speedup > 2x, got {:.1}x (parallel: {:?}, serial estimate: {:?})",
        speedup,
        parallel_elapsed,
        serial_estimate
    );
}
