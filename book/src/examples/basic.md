# basic

**File:** `examples/basic.rs`
**Run:**

```sh
cargo run --example basic --release
```

## What it demonstrates

The minimal single-threaded benchmark: a tight integer-add loop measured with PMU counters (on Linux) or timing-only (elsewhere). It also shows how to override `BenchmarkRuntimeOptions` for the whole session.

## Key code

```rust,ignore
fn add_loop(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        acc = acc.wrapping_add(black_box(i));
    }
    black_box(acc);
}

benchmark_main!(|runner| {
    let runtime = BenchmarkRuntimeOptions {
        warm_up_duration: Duration::from_millis(500),
        benchmark_duration: Duration::from_secs(2),
        ..BenchmarkRuntimeOptions::default()
    };

    runner
        .set_runtime(runtime)
        .group::<NoContext>("Arithmetic", |g| {
            g.throughput(Throughput::per_operation(8, "bytes"))
                .bench("add_loop", add_loop);
        });
});
```

## What to look for

- `Throughput::per_operation(8, "bytes")` — one loop iteration represents 8 bytes (a `u64`), so the throughput column reads `bytes/s`.
- `black_box` on `0`, `limit`, each `i`, and the final `acc`. Without `black_box(i)` the optimizer can hoist or fold the loop.
- The custom runtime shortens warm-up to 500 ms and benchmark duration to 2 s, so the example finishes faster than the default 1s/5s.
- On Linux, the stats table has PMU rows (`instructions/op`, `branches/op`, `cache misses/op`, IPC). Off Linux, those rows are absent and the `Measurement` row reads `timing only`.