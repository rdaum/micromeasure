# gpu_domain

**File:** `examples/gpu_domain.rs`
**Run:**

```sh
cargo run --example gpu_domain --release
```

## What it demonstrates

The effect of `MeasurementDomain::Gpu` on a stand-in GPU workload. The "kernel" is a CPU loop, but the point is the diagnostics path: with `Cpu` (the default) the runner emits a misleading "data-side memory latency" bottleneck; with `Gpu` it suppresses that and relabels the coverage byline.

## Key code

```rust,ignore
fn fake_gpu_kernel(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        acc = acc.wrapping_add(black_box(i));
    }
    black_box(acc);
}

benchmark_main!(|runner| {
    runner.group::<NoContext>("GPU/domain", |g| {
        g.throughput(Throughput::bytes(8))
            .measurement_domain(MeasurementDomain::Gpu)
            .bench("fake_kernel", fake_gpu_kernel);
    });
});
```

## What to look for

With `MeasurementDomain::Gpu`:

- The `possible bottlenecks:` section is suppressed entirely (no "data-side memory latency" diagnostic, even though the host thread's PMU counters would otherwise trigger it).
- The PMU coverage byline reads `host PMU (orchestration): coverage=...` instead of the default `host PMU (perf event group): coverage=...`.
- Run-stability warnings (CV, outliers) are **still emitted** if the run is noisy — domain suppression is for CPU-PMU bottleneck diagnostics only.

The example's leading comment records the actual before/after output so you can see the difference without editing the file: with `Cpu` you get `Likely data-side memory latency: backend stall is 89.99% ...`; with `Gpu` that line disappears.

## When to use `Gpu` vs `Mixed`

- `Gpu`: the host thread is mostly idle waiting on the device (synchronized kernel calls). CPU PMU describes orchestration only.
- `Mixed`: the host thread does real work (data layout, tile reshaping) interleaved with device calls. You want the CPU-PMU diagnostics, but labelled as host context. See [mixed_domain](./mixed-domain.md).