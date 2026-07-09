# concurrent_counters

**File:** `examples/concurrent_counters.rs`
**Run:**

```sh
cargo run --example concurrent_counters --release
```

## What it demonstrates

The same reader/writer contention scenario as [concurrent_scenario](./concurrent-scenario.md), but the reader also reports a bench-specific event counter (`read_misses`) via `ConcurrentWorkerResult::with_counter`.

## Key code

```rust,ignore
fn optimistic_reader(ctx: &CounterLatch, control: &ConcurrentBenchControl) -> ConcurrentWorkerResult {
    let mut operations = 0_u64;
    let mut read_misses = 0_u64;

    while !control.should_stop() {
        if let Ok(guard) = ctx.value.try_read() {
            black_box(*guard ^ control.thread_index() as u64 ^ control.role_thread_index() as u64);
            operations = operations.wrapping_add(1);
        } else {
            read_misses = read_misses.wrapping_add(1);
        }
    }

    ConcurrentWorkerResult::operations(operations).with_counter("read_misses", read_misses)
}
```

## What to look for

- Beneath the `optimistic_reader` stats table, a `bench event counters` table appears:

  ```text
  bench event counters:
    Event        Total      Per Op      Per Sec
    read_misses  123456     0.8421      24691.20
  ```

- `Per Op` is `counter / operations`; `Per Sec` is `counter / total_duration_sec`.
- The counter is a worker-local integer incremented in the hot loop and packaged once at the end — it does not contaminate the timing path. Keep event counters as plain integers, not locking structures.
- This is the pattern for retries, failed try-locks, dropped work, backoffs: count them locally, report them with `with_counter`, read the rates in the report.