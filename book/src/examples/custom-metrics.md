# custom_metrics

**File:** `examples/custom_metrics.rs`
**Run:**

```sh
cargo run --example custom_metrics --release
```

## What it demonstrates

Phase 2 of the GPU benchmarking work: per-sample custom metrics via `bench_sample(...)` returning a `BenchSampleResult`. The bench function is a stand-in for one synchronized GPU operation; in a real GPU benchmark it would record a CUDA event around the kernel and return `cuda_event_ms` and `host_overhead_ms` derived from host wall-clock minus device event time. Here the values are synthetic (derived from the chunk index) so the example runs without CUDA linked.

## Key code

```rust,ignore
impl BenchContext for FakeGpuBench {
    fn prepare(_chunk_size: usize) -> Self { FakeGpuBench }

    // Fixed chunk size: GPU work does not auto-scale the way CPU work does.
    fn chunk_size() -> Option<usize> { Some(4_000) }
}

fn fake_gpu_kernel_with_metrics(
    _ctx: &mut FakeGpuBench,
    chunk_size: usize,
    chunk_num: usize,
) -> BenchSampleResult {
    let host_start = Instant::now();
    /* ... stand-in kernel work ... */
    let host_elapsed_ms = host_start.elapsed().as_secs_f64() * 1_000.0;
    let device_ms = host_elapsed_ms * 0.85;
    let overhead_ms = (host_elapsed_ms - device_ms).max(0.0);
    let flops = (chunk_size as u64) * 1024;
    let device_seconds = device_ms / 1000.0;

    BenchSampleResult::operations(chunk_size as u64)
        .push_metric(
            MetricValue::duration_ms("cuda_event_ms", Duration::from_secs_f64(device_seconds))
                .with_display_name("CUDA event time"),
        )
        .push_metric(
            MetricValue::new("host_overhead_ms", overhead_ms, "ms")
                .with_display_name("Host overhead"),
        )
        .push_metric(
            MetricValue::throughput_tflops("tflops", flops, device_seconds)
                .with_display_name("TFLOP/s"),
        )
        .push_metric(
            MetricValue::integer("selected_algo_id", (chunk_num % 4) as i64, "id")
                .with_display_name("cuBLASLt algo"),
        )
}

benchmark_main!(|runner| {
    runner.group::<FakeGpuBench>("GPU/metrics", |g| {
        g.throughput(Throughput::bytes(8))
            .measurement_domain(MeasurementDomain::Gpu)
            .bench_sample("fp4_gemm", fake_gpu_kernel_with_metrics);
    });
});
```

## What to look for

- `bench_sample` instead of `bench`. The bench function returns `BenchSampleResult`; the runner reads `operations` for throughput/latency and collects `metrics` per sample.
- A `custom metrics:` table appears beneath the standard stats table, aggregating each `(section, name, unit)` across samples into mean, median, p95, min, max, and sample count.
- Four metric shapes are exercised:
  - `duration_ms` — helper for a `Duration`, formatted as `ms`.
  - `new` with a plain `f64` value and unit.
  - `throughput_tflops` — derived helper, computes `flops / (1e12 * seconds)`.
  - `integer` — `MetricFormat::Integer`, renders with no decimals and no scientific notation. Used here for `selected_algo_id`, which shifts with `chunk_num % 4` to show aggregation of a value that changes between samples.
- `MeasurementDomain::Gpu` suppresses CPU-PMU diagnostics (this is a GPU-domain bench).
- `chunk_size() -> Some(4_000)` bypasses calibration, as required for GPU work — see [Sharp Edges](../gpu-sharp-edges.md#calibration-assumes-cpu-like-chunk-scaling).
- `with_display_name(...)` sets a human-readable label ("CUDA event time", "Host overhead", "TFLOP/s", "cuBLASLt algo"); the aggregation key remains the machine-friendly `name`.

## What this does not do

This example does not use a `MeasurementBackend`. The `cuda_event_ms` value is a synthetic fraction of host wall-clock, not a real CUDA event. For real device-side timing, combine `bench_sample` with a backend — see [custom_backend](./custom-backend.md) and [cuda_event_backend](./cuda-event-backend.md).
