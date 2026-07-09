# GPU Benchmarks

`micromeasure` can benchmark GPU work (e.g. cuBLASLt GEMM, CUDA streams) alongside CPU microbenchmarks. Three features work together to make GPU output less misleading than naive "wrap a kernel in `Instant::now`" timing:

1. **`MeasurementDomain`** — tags a group as `Cpu`, `Gpu`, or `Mixed` to control CPU-PMU diagnostics.
2. **`MeasurementBackend`** — pluggable measurement window around the bench closure. The built-in `CudaEventBackend` records CUDA events on the default stream and uses device elapsed time as the sample duration.
3. **Per-sample custom metrics** — `bench_sample(...)` lets the bench return a `BenchSampleResult` carrying `operations` plus named metrics (`cuda_event_ms`, `tflops`, ...). The runner aggregates them across samples and renders a `custom metrics:` table.

There is also a **diagnostic replay pass** for collecting invasive counters without contaminating timing. With the `gpu-counters` feature, that pass can use CUPTI/NVPerf range-profiler counters.

These features are independent: you can use `MeasurementDomain::Gpu` with the default backend, or a custom backend with `MeasurementDomain::Cpu`, etc. But they are designed to compose for GPU work.

> Before writing a GPU benchmark, read [GPU Benchmarking Sharp Edges](./gpu-sharp-edges.md). It documents the failure modes that motivated this design (host PMU counters don't describe the kernel, calibration assumes CPU-like chunk scaling, etc.).

## Measurement domain

```rust,ignore
pub enum MeasurementDomain {
    Cpu,    // default: full PMU counters + historic diagnostics
    Gpu,    // suppress CPU-PMU bottleneck diagnostics; relabel coverage line
    Mixed,  // emit CPU-PMU diagnostics with a [host] prefix
}
```

Set it on a group with `g.measurement_domain(MeasurementDomain::Gpu)`.

### What each domain does

- **`Cpu`** (default): unchanged historic behaviour. Full PMU counters and the existing bottleneck diagnostics apply.
- **`Gpu`**: CPU PMU data is treated as host-orchestration context, not as the primary bottleneck. CPU-PMU bottleneck diagnostics are **suppressed entirely**. The PMU coverage byline is relabelled to `host PMU (orchestration)`. Run-stability warnings (CV, outliers) are still emitted, because sample stability matters for any benchmark.
- **`Mixed`**: CPU-PMU diagnostics are emitted but prefixed with `[host]`, so the reader knows the signal is host-side context (e.g. a GPU benchmark that does meaningful host-side data layout between kernel launches). The PMU coverage byline reads `host PMU (mixed workload)`.

### Interaction with `emits_cpu_diagnostics`

A backend can also opt out of CPU-PMU diagnostics via `MeasurementBackend::emits_cpu_diagnostics() -> false`. The runner suppresses CPU-PMU diagnostics when **either**:

- `stats.measurement_domain == Gpu`, or
- `stats.emits_cpu_diagnostics == false`.

`Mixed` keeps diagnostics (with `[host]` prefix) **unless** the backend also returns `false` from `emits_cpu_diagnostics`. This lets a CUDA event backend opt out for `Mixed` workloads where some host work is expected but you still do not want host-PMU bottleneck messages.

The `gpu_domain` and `mixed_domain` examples demonstrate the observable difference — see [gpu_domain](./examples/gpu-domain.md) and [mixed_domain](./examples/mixed-domain.md).

## Measurement backend

`MeasurementBackend` is an object-safe trait that owns the per-sample measurement window:

```rust,ignore
pub trait MeasurementBackend {
    fn begin(&mut self);
    fn end(&mut self);
    fn collect(
        &mut self,
        host_elapsed: Duration,
        ops: u64,
        chunk_index: usize,
        results: &mut Results,
        metrics: &mut Vec<MetricValue>,
    );

    fn measurement_label(&self) -> &'static str { "" }
    fn emits_cpu_diagnostics(&self) -> bool { true }
}
```

The runner calls `begin` immediately before the bench closure, `end` immediately after, then `collect` to materialize the measurement into the shared `Results` and any backend-specific metrics.

### The duration contract

Backends decide what `results.duration` means:

- `LinuxPerfBackend` and `WallClockBackend` set `results.duration = host_elapsed` (host wall-clock around the closure).
- A GPU backend (e.g. `CudaEventBackend`) sets `results.duration` to the **device event elapsed time** and separately reports `host_overhead_ms` as a custom metric. Throughput/latency stats then describe the GPU kernel, not the host orchestration — which is usually what a GPU benchmark author wants.
- If a backend leaves `results.duration` at `Duration::ZERO`, the runner falls back to `host_elapsed` to avoid divide-by-zero in throughput computation.

Backends must write `ops` into `results.iterations` (so the existing throughput/latency paths keep working) and set `results.chunks_executed = 1` per sample.

### Platform default

| Platform | Default backend |
|---|---|
| Linux | `LinuxPerfBackend` (perf-event group + individual-counter fallback) |
| other | `WallClockBackend` (timing-only) |

Override per group with `g.backend(|| Box::new(MyBackend::new()))`. The factory creates a fresh backend per benchmark run.

### Built-in `CudaEventBackend`

Available behind the `cuda` feature (see [Cargo.toml](https://github.com/rdaum/micromeasure/blob/main/Cargo.toml) and `build.rs` for linking). It records `cudaEventRecord` in `begin`/`end`, synchronizes the stop event in `end`, computes elapsed in `collect`, and pushes:

- `cuda_event_ms` — device event elapsed time
- `host_overhead_ms` — host wall-clock minus device time
- `gpu_gib_s` — when `bytes_per_op > 0`
- `gpu_tflops` — when `flops_per_op > 0`

It uses device elapsed time as `results.duration`, and `emits_cpu_diagnostics() == false`.

```rust,ignore
CudaEventBackend::new(bytes_per_op, flops_per_op)  // CudaResult<Self>
```

On a CUDA error it records `cuda_error_code` as a metric instead of crashing the run.

See [cuda_event_backend](./examples/cuda-event-backend.md) for a runnable example using real `cudaMemsetAsync` on the default stream.

### Built-in GPU counter collector

Available behind the `gpu-counters` feature. `GpuCounterCollector` wraps NVIDIA CUPTI/NVPerf range profiling for diagnostic replay passes:

```rust,ignore
let mut collector =
    GpuCounterCollector::new(DEFAULT_NVIDIA_GPU_COUNTERS, "my_range")?;
loop {
    collector.begin()?;
    run_workload();
    if collector.end()? {
        break;
    }
}
for metric in collector.decode()? {
    result = result.push_metric(metric.to_metric_value());
}
```

The default metric set maps NVIDIA profiler metric names into stable micromeasure metric names:

| NVIDIA metric | micromeasure metric |
|---|---|
| `gpu__compute_memory_throughput.avg.pct_of_peak_sustained_elapsed` | `gpu_memory_peak_pct` |
| `lts__throughput.avg.pct_of_peak_sustained_elapsed` | `gpu_l2_peak_pct` |
| `sm__throughput.avg.pct_of_peak_sustained_elapsed` | `gpu_sm_peak_pct` |
| `sm__inst_executed_pipe_tensor.avg.pct_of_peak_sustained_active` | `gpu_tensor_active_pct` |

`GpuCounterError::into_diagnostic_result()` converts counter setup, permission, metric, and collection failures into integer diagnostic metrics so a locked-down system can still run the timing benchmark.

See [gpu_counters](./examples/gpu-counters.md) for a runnable example.

### Writing a custom backend

The trait is object-safe; a runner can hold `Box<dyn MeasurementBackend>`. See [custom_backend](./examples/custom-backend.md) for a simulated CUDA event backend that needs no CUDA dependency — it records a fake device elapsed time and pushes `cuda_event_ms` / `host_overhead_ms` from `collect`.

The shape of a GPU backend `collect` is:

```rust,ignore
fn collect(
    &mut self,
    host_elapsed: Duration,
    ops: u64,
    _chunk_index: usize,
    results: &mut Results,
    metrics: &mut Vec<MetricValue>,
) {
    // Leaving has_* flags false suppresses CPU PMU rows and diagnostics.
    results.duration = self.device_duration;  // device event time
    results.iterations = ops;
    results.chunks_executed = 1;

    metrics.push(MetricValue::duration_ms("cuda_event_ms", self.device_duration));
    metrics.push(MetricValue::new("host_overhead_ms",
        (host_elapsed - self.device_duration).max(Duration::ZERO).as_secs_f64() * 1000.0, "ms"));
}
```

Leave `has_cycles`/`has_instructions`/... false to suppress CPU PMU rows in the stats table, and return `false` from `emits_cpu_diagnostics()` to suppress CPU-PMU bottleneck messages.

## Per-sample custom metrics

`bench_sample(name, f)` registers a bench whose function returns `BenchSampleResult`:

```rust,ignore
fn my_gpu_bench(ctx: &mut GpuContext, chunk_size: usize, chunk_num: usize) -> BenchSampleResult {
    let device_s = ctx.run_kernel(chunk_size);
    BenchSampleResult::operations(chunk_size as u64)
        .push_metric(
            MetricValue::duration_ms("cuda_event_ms", Duration::from_secs_f64(device_s))
                .with_display_name("CUDA event time"),
        )
        .push_metric(
            MetricValue::throughput_tflops("tflops", flops, device_s)
                .with_display_name("TFLOP/s"),
        )
}
```

The runner:

1. Reads `operations` and routes it through the existing throughput/latency aggregation (`Results.iterations`).
2. Collects `metrics` per sample and aggregates them into `MetricSummary` (mean, median, p95, min, max, contributing sample count).
3. Persists the aggregated summaries in `BenchmarkStats.metrics` (JSON via the existing serde derive) and renders them in a `custom metrics:` table beneath the standard stats table.

Bench-function metrics and backend-pushed metrics are **merged into one table**. The aggregation key is `(section, name, unit)`, so metrics with different sections or units are treated as distinct.

### `MetricValue`

`MetricValue` carries `name`, `value`, `unit`, optional `section`, optional `display_name`, and a `format` hint (`Number` or `Integer`):

```rust,ignore
MetricValue::new("host_overhead_ms", overhead_ms, "ms")
MetricValue::duration_ms("cuda_event_ms", duration)       // helper
MetricValue::bandwidth_gib_s("gpu_gib_s", bytes, seconds) // helper
MetricValue::throughput_tflops("tflops", flops, seconds)   // helper
MetricValue::integer("selected_algo_id", algo_id, "id")   // integer format, no decimals
```

`with_display_name(...)` sets a human-readable label (e.g. "GPU bandwidth"); the aggregation key remains `name`. `with_section(...)` groups metrics in the table. `Integer` format rounds and never uses scientific notation — use it for algorithm IDs, device indices, workspace sizes.

See [custom_metrics](./examples/custom-metrics.md) for a complete runnable example with `duration_ms`, `throughput_tflops`, and `integer` formats.

## Diagnostic replay pass

Some counters are too invasive to collect in the timing loop (e.g. enabling CUPTI/NVPerf profiling changes kernel launch latency and may require replay passes). The diagnostic pass lets you collect them in a separate run that does not contaminate the timing statistics.

```rust,ignore
g.diagnostic_samples(3)
    .diagnostic_pass(my_gpu_counter_replay)
    .bench_sample("fp4_gemm", my_gpu_bench);
```

Behaviour:

- Runs **once after** the normal timing samples, using the calibrated chunk size.
- Metrics returned by the diagnostic pass are merged into the same `custom metrics:` table.
- `DiagnosticResult::new(section)` applies `section` as the default for metrics that do not set their own section.
- The diagnostic pass does **not** contribute to latency, throughput, CV, or outlier statistics, so it can collect invasive counters without contaminating normal timing.
- Use `g.diagnostic_samples(n)` to repeat noisy diagnostic counters (default 1).
- The diagnostic pass is **fallible**: return `Err(DiagnosticError)` and the failure is reported as a diagnostic metric rather than a timing failure.

```rust,ignore
fn my_gpu_counter_replay(
    ctx: &mut GpuContext,
    chunk_size: usize,
    _chunk_num: usize,
) -> Result<DiagnosticResult, DiagnosticError> {
    ctx.run_kernel_under_profiler(chunk_size)
}
```

`DiagnosticResult` is a section plus a `Vec<MetricValue>`, built fluently:

```rust,ignore
DiagnosticResult::new("profiler")
    .push_metric(MetricValue::new("kernel_launches", 42.0, "count"))
```

Metrics with no explicit `section` inherit the result's `section`.

## Putting it together

A complete GPU benchmark group:

```rust,ignore
benchmark_main!(|runner| {
    runner.group::<GpuContext>("cuBLASLt FP4", |g| {
        g.throughput(Throughput::bytes(8))
            .measurement_domain(MeasurementDomain::Gpu)
            .backend(|| Box::new(CudaEventBackend::new(8, 16).unwrap()))
            .diagnostic_samples(3)
            .diagnostic_pass(my_gpu_counter_replay)
            .bench_sample("fp4_gemm", my_gpu_bench);
    });
});
```

The pieces compose:

- `MeasurementDomain::Gpu` suppresses CPU-PMU diagnostics and relabels the coverage line.
- `CudaEventBackend` provides device-side timing, sets `results.duration` to the device event time, and pushes `cuda_event_ms` / `host_overhead_ms` / `gpu_gib_s` / `gpu_tflops`.
- `bench_sample` lets the bench add its own metrics (e.g. `selected_algo_id`, bench-specific TFLOP/s).
- `diagnostic_pass` collects invasive counters in a separate run.
- `GpuCounterCollector` can be used inside that diagnostic pass to report CUPTI/NVPerf counters when the `gpu-counters` feature is enabled.

## Limitations

- The `MeasurementBackend` trait is wired into the **single-threaded** path only. The concurrent path does not yet use it. See [GPU Benchmarking Sharp Edges](./gpu-sharp-edges.md#measurementbackend-is-not-wired-into-the-concurrent-path).
- `CudaEventBackend` uses the **default stream** only. Multi-stream work would need a custom backend or an extension.
- `GpuCounterCollector` is NVIDIA-only and uses CUPTI/NVPerf. It may require driver counter permissions; permission failures should be treated as diagnostic availability failures, not benchmark timing failures.
