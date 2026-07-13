# GPU Benchmarking Sharp Edges

This document catalogs the failure modes that motivated `micromeasure`'s GPU benchmarking design. Each section names the sharp edge, explains why it bites naive GPU benchmarks, and points at the `micromeasure` feature that addresses it.

It was written from the source: every claim below is grounded in the actual code paths in `src/bench.rs`, `src/bench/backend.rs`, `src/bench/cuda.rs`, and `src/bench/perf.rs`. When a feature exists to mitigate an edge, the relevant types are named so you can read the source for the authoritative behaviour.

## CPU PMU diagnostics are misleading for GPU kernels

### The problem

The default measurement path on Linux is `LinuxPerfBackend`, which opens a perf-event group and fills `Results` with CPU PMU counters: cycles, instructions, branches, branch misses, cache references, cache misses, frontend/backend stalls.

For a CPU microbenchmark these are the whole point. For a GPU benchmark they describe the **host thread**, which during a synchronized device call is mostly idle, waiting on `cudaDeviceSynchronize()`. The host thread's counters therefore describe launch/sync orchestration, not the device kernel.

Concretely, a GPU benchmark run with `MeasurementDomain::Cpu` (the default) can produce:

```text
possible bottlenecks:
  - Likely data-side memory latency: backend stall is 89.99% with
    cache pressure at 0.00% miss rate and 0.0000 misses/op
```

That reads like a memory-latency problem in the measured work. It isn't — the host thread is stalled waiting for the GPU. The "cache pressure at 0.00% miss rate" is the giveaway, but the diagnostic is still emitted because the heuristics trigger on backend-stall percentage alone.

### The mitigation

`MeasurementDomain` (`src/bench/backend.rs`) tags a group as `Cpu`, `Gpu`, `Io`, or `Mixed`. The diagnostics path (`diagnose_stats` in `src/bench.rs`) consults it:

- `Gpu`: CPU-PMU bottleneck diagnostics are suppressed entirely. The PMU coverage byline is relabelled to `host PMU (orchestration): coverage=...`.
- `Mixed`: diagnostics are emitted but prefixed with `[host] `, so the reader knows the signal is host-side context. The byline reads `host PMU (mixed workload)`.
- `Cpu`: unchanged historic behaviour.

Run-stability warnings (CV, outliers) are **kept in all domains** — sample stability matters for any benchmark, including GPU.

A backend can also opt out via `MeasurementBackend::emits_cpu_diagnostics() -> false`. The runner suppresses CPU-PMU diagnostics when **either** `measurement_domain == Gpu` **or** `emits_cpu_diagnostics == false`. `Mixed` keeps diagnostics unless the backend also opts out.

See `src/bench.rs` around `diagnose_stats` for the exact thresholds and the `[host]` prefix logic. See [gpu_domain](./examples/gpu-domain.md) and [mixed_domain](./examples/mixed-domain.md) for the observable difference.

## Calibration assumes CPU-like chunk scaling

### The problem

The single-threaded calibration path (`calibrate_engine` in `src/bench.rs`) finds a `chunk_size` such that one chunk takes roughly 50 ms (`TARGET_CHUNK_DURATION`). It does up to 15 passes, starting with one operation and scaling by the ratio of target-to-observed duration, clamped to `1..=50,000,000` operations.

This model assumes the work scales roughly linearly with `chunk_size` and that 50 ms is a meaningful sample window. Both assumptions break for GPU work:

- **Launch overhead dominates small chunks.** A `cudaMemsetAsync` of 16 MiB takes a few hundred microseconds of device time, but the launch + sync path is largely fixed cost. Doubling the chunk size does not double the wall-clock time until you are past the launch-overhead regime.
- **Algorithm selection can change with shape.** cuBLASLt picks a GEMM algorithm from a heuristic that depends on M/N/K, dtype, and tile shape. A chunk size that lands on a different algorithm than your real workload selects a different kernel and the numbers are meaningless.
- **50 ms of GPU work is a lot.** A GEMM at the sizes people actually benchmark may already take tens of milliseconds per call. Running 100 of them to fill the sample budget is not what you want.

Letting `calibrate_engine` pick the chunk size for a GPU benchmark produces a number that is a mix of launch overhead, the wrong algorithm, and a sample window far larger than intended.

### The mitigation

`BenchContext::chunk_size() -> Option<usize>` lets a benchmark **bypass chunk-size calibration** and declare a fixed chunk size. When it returns `Some(n)`, `calibrate_engine` warms up at that size, uses the active measurement backend's observed duration to select a sample count, and then runs the fixed-size samples.

Every GPU example in this crate uses a fixed `chunk_size()`:

```rust,ignore
impl BenchContext for FakeGpuBench {
    fn prepare(_num_chunks: usize) -> Self { FakeGpuBench }

    fn chunk_size() -> Option<usize> {
        Some(4_000)
    }
}
```

When `chunk_size()` returns `Some`, the runner estimates chunk throughput and duration from the warm-up passes. It uses that duration to choose a sample count that targets `benchmark_duration`, clamped to `min_samples..=max_samples`, just as it does after automatic chunk calibration.

### What this implies

- You are responsible for picking a chunk size that matches the workload you care about. The framework will not second-guess you.
- `benchmark_duration` remains a target rather than a hard limit. The `min_samples` and `max_samples` bounds can still make the actual run shorter or longer.
- If your per-sample device time is large, consider lowering `min_samples` or the chunk size so the run completes in reasonable wall-clock time.

## Host wall-clock is not device time

### The problem

The runner measures the host wall-clock span around the bench closure with `Instant::now()`. For a GPU benchmark that synchronizes on the device, host wall-clock includes:

- kernel launch overhead
- queue scheduling
- `cudaDeviceSynchronize()` blocking time
- any host-side data marshalling

For a synchronized device call, host wall-clock and device elapsed time are usually **very close** (the host thread is just blocked), but they are not identical. Host wall-clock includes queue/scheduling jitter that device event time does not. Using host wall-clock as `results.duration` for a GPU benchmark makes the throughput/latency numbers reflect host orchestration noise, not the kernel.

### The mitigation

The `MeasurementBackend` **duration contract** (documented on `MeasurementBackend::collect` in `src/bench/backend.rs`) lets a backend decide what `results.duration` means:

- `LinuxPerfBackend` and `WallClockBackend` set `results.duration = host_elapsed`.
- A GPU backend sets `results.duration` to the **device event elapsed time** and separately reports `host_overhead_ms` as a custom metric. Throughput/latency stats then describe the GPU kernel.

`CudaEventBackend` (in `src/bench/cuda.rs`) does exactly this: in `begin` it records a start event on the default stream; in `end` it records and synchronizes a stop event; in `collect` it computes `cudaEventElapsedTime` and sets `results.duration = device_duration`. It also pushes `cuda_event_ms` and `host_overhead_ms` so you can see the split.

If a backend leaves `results.duration` at `Duration::ZERO`, the runner falls back to `host_elapsed` to avoid divide-by-zero in throughput computation. This is a safety net, not a default — backends should set it.

### `cudaEventElapsedTime` caveats

- It returns a `f32` with ~0.5 us resolution. For sub-microsecond kernels this is noise. The framework converts to `f64` but the resolution limit is a CUDA runtime property.
- It requires the two events to have both completed. `CudaEventBackend` synchronizes the stop event in `end()` before `collect()` reads the elapsed time.
- It is only meaningful for events on the same stream. `CudaEventBackend` uses the default stream throughout. Multi-stream work would need a backend that records on the relevant streams and reads the correct event pairs.

## No per-sample custom metrics (historically)

### The problem

The historic bench signature is `fn(&mut T, usize, usize) -> ()`. The only output is "did it run"; the framework derives operation count from `chunk_size` (or `operations_per_chunk()`). For a GPU benchmark this loses information only the bench knows:

- which cuBLASLt algorithm was selected for this sample
- CUDA event elapsed time (when not using a CUDA-event backend)
- TFLOP/s derived from a shape-dependent FLOP count
- workspace size, validation codes, per-sample device clock samples

You could log these to stderr, but you could not get them into the persisted report or the stats table.

### The mitigation

`BenchSampleResult` (`src/bench/backend.rs`) is the Phase 2 extension point. A bench function with the richer signature `fn(&mut T, usize, usize) -> BenchSampleResult` returns one per sample:

```rust,ignore
pub struct BenchSampleResult {
    pub operations: u64,
    pub metrics: Vec<MetricValue>,
}
```

The runner:

1. Reads `operations` and routes it through the existing throughput/latency aggregation (`Results.iterations`).
2. Collects `metrics` per sample and aggregates them into `MetricSummary` (mean, median, p95, min, max, contributing sample count).
3. Persists the aggregated summaries in `BenchmarkStats.metrics` (JSON via serde) and renders them in a `custom metrics:` table beneath the standard stats table.

Bench-function metrics and backend-pushed metrics are **merged into one table**. The aggregation key is `(section, name, unit)`, so metrics with different sections or units are distinct. `MetricValue` has a `format` hint (`Number` for adaptive scientific/decimal, `Integer` for IDs/counts) and an optional `display_name` (cosmetic; the aggregation key remains `name`).

Register a `bench_sample` with `g.bench_sample(name, f)` instead of `g.bench(name, f)`. See [custom_metrics](./examples/custom-metrics.md) and [custom_backend](./examples/custom-backend.md).

## Invasive counters contaminate timing

### The problem

Some GPU counters can only be collected by enabling profiling (CUPTI, NSight). Enabling profiling changes kernel launch latency — the act of measuring changes the thing being measured. If you collect those counters in the timing loop, your latency/throughput/CV numbers describe "kernel under profiler", not "kernel".

### The mitigation

The **diagnostic replay pass** runs once *after* the normal timing samples, using the calibrated chunk size. It does **not** contribute to latency, throughput, CV, or outlier statistics.

```rust,ignore
g.diagnostic_samples(3)
    .diagnostic_pass(my_gpu_counter_replay)
    .bench_sample("fp4_gemm", my_gpu_bench);
```

- `diagnostic_pass(f)` registers a function `fn(&mut T, usize, usize) -> Result<DiagnosticResult, DiagnosticError>`.
- `diagnostic_samples(n)` repeats the pass `n` times (default 1) for noisy counters.
- The pass is **fallible**: return `Err(DiagnosticError)` and the failure is reported as a diagnostic metric rather than a timing failure.
- Metrics returned by the pass are merged into the same `custom metrics:` table as bench-sample and backend metrics.
- `DiagnosticResult::new(section)` applies `section` as the default for metrics that do not set their own section.

See `src/bench.rs` around `execute_diagnostic_sample` for the exact wiring. The pass runs on the same measuring thread as the timing samples, with the same backend — the backend's `begin`/`end` are called around it, but the resulting `Results` are not fed into the stats aggregation.

When the `gpu-counters` feature is enabled, `GpuCounterCollector` provides a CUPTI/NVPerf range-profiler wrapper designed for this path. It may need multiple user replay passes:

```rust,ignore
loop {
    collector.begin()?;
    run_workload();
    if collector.end()? {
        break;
    }
}
```

The collector evaluates the configured range after all passes complete and maps the default NVIDIA metric set into stable micromeasure names such as `gpu_memory_peak_pct`, `gpu_l2_peak_pct`, `gpu_sm_peak_pct`, and `gpu_tensor_active_pct`.

## NVIDIA counter permissions are environment-dependent

### The problem

NVIDIA performance counters are often restricted by driver policy. On locked-down systems, CUPTI/NVPerf setup can fail with errors such as `ERR_NVGPUCTRPERM` or `CUPTI_ERROR_INSUFFICIENT_PRIVILEGES`. Metric availability can also vary by GPU architecture and driver.

This is different from a benchmark failure: the workload may be perfectly runnable, while the optional diagnostic counters are unavailable.

### The mitigation

`GpuCounterError` categorizes common failures, and `GpuCounterError::into_diagnostic_result()` converts them into integer diagnostic metrics:

- `gpu_counter_permission_error`
- `gpu_counter_metric_error`
- `gpu_counter_profiler_error`
- `gpu_counter_collection_error`
- `gpu_counter_invalid_name`

Use that conversion inside `diagnostic_pass` when you want timing to remain usable even if counters are unavailable. See [gpu_counters](./examples/gpu-counters.md).

## Concurrent backends use a scenario-wide window

Concurrent groups can opt into `backend(...)`. The coordinator calls `begin`
after every worker is ready, releases them together, joins them all, then calls
`end` and `collect`. Backend custom metrics are scenario-scoped. Worker PMU
summaries are retained separately, so a GPU backend can own combined device
timing without discarding host-thread context.

This does not make the built-in `CudaEventBackend` multi-stream aware: it still
records on the CUDA default stream. A concurrent GPU benchmark using other
streams needs a custom backend that records the correct device-wide or
multi-stream boundary.

## `CudaEventBackend` uses the default stream only

### The problem (current limitation)

`CudaEventBackend` (`src/bench/cuda.rs`) records events with `cudaEventRecord(event, null_mut())` — the default stream. It does not accept a stream handle, and `CudaEvent` only exposes `record_default_stream()`.

### What this means

- Work enqueued on non-default streams is not measured correctly. The start/stop events record on the default stream, so `cudaEventElapsedTime` measures the default-stream gap, not the time your work actually ran on its stream.
- Multi-stream overlap (the whole point of using multiple streams) is invisible to this backend.
- If you need multi-stream timing, you need a custom backend that owns stream handles and records events on the right stream. The `MeasurementBackend` trait supports this — `CudaEventBackend` just doesn't implement it.

## `MetricValue` is `f64`-only

`MetricValue.value` is `f64`. There is no integer-typed metric value; `MetricFormat::Integer` is a *rendering* hint that rounds and suppresses decimals, not a storage type.

For most metrics this is fine. For categorical or count-valued metrics (algorithm IDs, device indices, kernel counts) the `Integer` format hint avoids scientific notation and decimal places in the table. But the underlying value is still `f64`, so very large integer counts (> 2^53) lose precision. This has not been a problem in practice for the GPU metrics `micromeasure` targets, but be aware of it if you try to push raw byte counts as metrics instead of deriving `GiB/s`.

## Recap

| Sharp edge | Mitigation | Where |
|---|---|---|
| CPU PMU diagnostics misleading for GPU | `MeasurementDomain::{Gpu, Mixed}` + `emits_cpu_diagnostics()` | `src/bench.rs` `diagnose_stats` |
| Calibration assumes CPU-like scaling | `BenchContext::chunk_size() -> Some(n)` | `src/bench.rs` `calibrate_engine` |
| Host wall-clock != device time | `MeasurementBackend` duration contract; `CudaEventBackend` | `src/bench/backend.rs`, `src/bench/cuda.rs` |
| No per-sample custom metrics | `bench_sample` + `BenchSampleResult` + `MetricValue` | `src/bench/backend.rs` |
| Invasive counters contaminate timing | `diagnostic_pass` + `diagnostic_samples` | `src/bench.rs` |
| NVIDIA counter permissions vary | `GpuCounterError::into_diagnostic_result()` | `src/bench/gpu_counters.rs` |
| Concurrent scenario timing | concurrent group `backend(...)` | `src/bench.rs` |
| `CudaEventBackend` default stream only | (none yet — write a custom backend for multi-stream) | `src/bench/cuda.rs` |
| `MetricValue` is `f64`-only | `MetricFormat::Integer` for rendering; avoid > 2^53 raw counts | `src/bench/backend.rs` |
