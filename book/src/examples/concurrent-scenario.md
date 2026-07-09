# concurrent_scenario

**File:** `examples/concurrent_scenario.rs`
**Run:**

```sh
cargo run --example concurrent_scenario --release
```

## What it demonstrates

A coordinated concurrent benchmark: 3 optimistic readers + 1 exclusive writer contending on a `RwLock<u64>`. Uses `concurrent_group`, `sample_duration`, and a custom `BenchmarkRuntimeOptions`.

## Key code

```rust,ignore
#[derive(Default)]
struct CounterLatch {
    value: RwLock<u64>,
}

impl ConcurrentBenchContext for CounterLatch {
    fn prepare(_num_threads: usize) -> Self { Self::default() }
}

fn optimistic_reader(ctx: &CounterLatch, control: &ConcurrentBenchControl) -> ConcurrentWorkerResult {
    let mut operations = 0_u64;
    while !control.should_stop() {
        if let Ok(guard) = ctx.value.try_read() {
            black_box(*guard ^ control.thread_index() as u64 ^ control.role_thread_index() as u64);
            operations = operations.wrapping_add(1);
        }
    }
    ConcurrentWorkerResult::operations(operations)
}

fn exclusive_writer(ctx: &CounterLatch, control: &ConcurrentBenchControl) -> ConcurrentWorkerResult {
    let mut operations = 0_u64;
    while !control.should_stop() {
        let mut guard = ctx.value.write().expect("rwlock poisoned");
        *guard = guard.wrapping_add(control.thread_index() as u64 + 1);
        black_box(*guard);
        operations = operations.wrapping_add(1);
    }
    ConcurrentWorkerResult::operations(operations)
}

benchmark_main!(|runner| {
    let workers = [
        ConcurrentWorker { name: "optimistic_reader", threads: 3, run: optimistic_reader },
        ConcurrentWorker { name: "exclusive_writer", threads: 1, run: exclusive_writer },
    ];

    runner
        .set_runtime(runtime)
        .concurrent_group::<CounterLatch>("Contention", |g| {
            g.sample_duration(Duration::from_millis(50))
                .bench("rwlock_readers_vs_writer", &workers);
        });
});
```

## What to look for

- Each sample runs all 4 threads (3 readers + 1 writer) for `sample_duration` (50 ms), then joins.
- Output is organized per worker role. `optimistic_reader (3 threads)` and `exclusive_writer (1 thread)` each get their own stats table with throughput, latency, and (on Linux) PMU counters.
- The `workers combined` section at the bottom aggregates the whole scenario — useful as the PMU view of the interacting workload.
- The reader uses `try_read()` (non-blocking) so under heavy writer contention it will mostly miss — that's the contention behaviour you are measuring. Compare reader throughput vs writer throughput to see the lock's fairness characteristics.