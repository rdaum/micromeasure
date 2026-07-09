# mixed_domain

**File:** `examples/mixed_domain.rs`
**Run:**

```sh
cargo run --example mixed_domain --release
```

## What it demonstrates

`MeasurementDomain::Mixed` for a workload that interleaves CPU data reshaping with a stand-in synchronized device call. Unlike `Gpu`, `Mixed` does **not** suppress CPU-PMU bottleneck diagnostics — the CPU work is real work. Instead it prefixes them with `[host]` so the reader knows the signal is host-side context, not a description of the device portion.

## Key code

```rust,ignore
fn mixed_cpu_gpu_work(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        // Stand-in for layout/scale reshaping that touches host cache.
        acc = acc.wrapping_add(black_box(i));
        // Stand-in for a kernel launch + synchronize.
        if i % 64 == 0 {
            black_box(acc);
        }
    }
    black_box(acc);
}

benchmark_main!(|runner| {
    runner.group::<NoContext>("GPU/domain", |g| {
        g.throughput(Throughput::bytes(8))
            .measurement_domain(MeasurementDomain::Mixed)
            .bench("mixed_cpu_gpu_work", mixed_cpu_gpu_work);
    });
});
```

## What to look for

- The PMU coverage byline reads `host PMU (mixed workload): coverage=...`.
- If a bottleneck diagnostic fires, it appears with a `[host] ` prefix:

  ```text
  possible bottlenecks:
    - [host] Likely data-side memory latency: backend stall is 50.0% ...
  ```

- Run-stability warnings are emitted without a prefix (they are not CPU-PMU-derived).

## Interaction with `emits_cpu_diagnostics`

`Mixed` keeps CPU-PMU diagnostics **unless** the backend also returns `false` from `emits_cpu_diagnostics()`. So a CUDA-event backend on a `Mixed` workload can still opt out of host-PMU bottleneck messages if you decide the host work is not the signal you want reported. See [GPU Benchmarks](../gpu.md#interaction-with-emits_cpu_diagnostics).