# gpu_counters

**File:** `examples/gpu_counters.rs`
**Run (requires CUDA toolkit, CUPTI, and an NVIDIA GPU):**

```sh
cargo run --example gpu_counters --features gpu-counters --release
```

## What it demonstrates

This is the most feature-complete GPU example in the crate. It combines three things in one benchmark group:

1. **`CudaEventBackend`** for timing - device event elapsed time drives the latency/throughput columns, same as [cuda_event_backend](./cuda-event-backend.md).
2. **`bench_sample`** for the timing path - the bench function returns a `BenchSampleResult` so the framework knows the operation count.
3. **`diagnostic_pass` with `GpuCounterCollector`** - after the timing samples finish, the same `cudaMemsetAsync` workload is replayed under the NVIDIA CUPTI/NVPerf range profiler, and the resulting GPU counters (memory throughput, L2 throughput, SM throughput, tensor-pipe activity) are reported as custom metrics.

The reason for the split is that CUPTI/NVPerf profiling is invasive: it can require multiple replay passes, it needs driver counter permissions, and enabling it changes kernel launch latency. Collecting those counters inside the timing loop would contaminate the latency/throughput numbers. The `diagnostic_pass` runs in a separate phase that does not feed `Results` into the stats aggregation, so you get clean timing *and* the counter view in one report. See [GPU Benchmarking Sharp Edges](../gpu-sharp-edges.md#invasive-counters-contaminate-timing) for the rationale.

## Prerequisites

This example needs more than the `cuda` feature alone:

- The `gpu-counters` feature, which implies `cuda` and adds CUPTI/NVPerf linking. `build.rs` compiles `native/gpu_counters.cpp` with `g++` and links `cupti`, `cuda`, and `stdc++`.
- A CUDA toolkit installation discoverable via `CUDA_HOME` or `CUDA_PATH` (the build script searches the standard `/usr/local/cuda*` paths as a fallback). The toolkit must include the CUPTI headers (`cupti_profiler_host.h`, `cupti_range_profiler.h`, `cupti_target.h`).
- An NVIDIA GPU and driver. Metric availability varies by architecture and driver; some metrics in `DEFAULT_NVIDIA_GPU_COUNTERS` may be unavailable on older or non-datacenter GPUs.
- **Counter permissions.** On many driver installations, performance counters are administrator-controlled. See [Permissions](#permissions) below.

If any of these are missing, the timing benchmark still runs - only the diagnostic counters are reported as unavailable. That fallback is deliberate and is the point of routing counters through `diagnostic_pass` rather than the timing path.

## Key code

### The timing path

Same shape as [cuda_event_backend](./cuda-event-backend.md): a fixed `chunk_size()` (GPU work does not auto-calibrate), a `CudaMemsetBench` context that allocates a device buffer in `prepare` and frees it in `Drop`, and a `bench_sample` function that enqueues `cudaMemsetAsync` on the default stream and returns the operation count.

```rust,ignore
impl BenchContext for CudaMemsetBench {
    fn prepare(_num_chunks: usize) -> Self {
        let mut device_buffer = null_mut();
        unsafe { check_cuda("cudaMalloc", cudaMalloc(&mut device_buffer, BYTES_PER_OP as usize)); }
        Self { device_buffer }
    }

    fn chunk_size() -> Option<usize> { Some(MEMSET_OPS_PER_SAMPLE) }
}
```

### The diagnostic pass

This is the new part. The diagnostic function replays the same workload under the range profiler. CUPTI range profiling may need more than one replay pass: `collector.end()` returns `Ok(true)` when all passes are complete, so the pass loops until done or until `MAX_COUNTER_REPLAY_PASSES` (8) is hit.

```rust,ignore
fn memset_gpu_counters(
    ctx: &mut CudaMemsetBench,
    chunk_size: usize,
    _chunk_num: usize,
) -> Result<DiagnosticResult, DiagnosticError> {
    let mut collector =
        match GpuCounterCollector::new(DEFAULT_NVIDIA_GPU_COUNTERS, "micromeasure_memset") {
            Ok(collector) => collector,
            Err(error) => return Ok(error.into_diagnostic_result()),
        };

    let mut passes = 0;
    loop {
        passes += 1;
        if let Err(error) = collector.begin() {
            return Ok(error.into_diagnostic_result());
        }
        let _ = memset_default_stream(ctx, chunk_size, passes as usize);
        let done = match collector.end() {
            Ok(done) => done,
            Err(error) => return Ok(error.into_diagnostic_result()),
        };
        if done || passes >= MAX_COUNTER_REPLAY_PASSES {
            break;
        }
    }

    let mut result = DiagnosticResult::new("gpu counters").push_metric(
        MetricValue::integer("gpu_counter_replay_passes", passes, "passes")
            .with_display_name("Replay passes"),
    );
    for metric in collector.decode()? {
        result = result.push_metric(metric.to_metric_value());
    }
    Ok(result)
}
```

A few things worth noticing:

- **Every error path returns `Ok(error.into_diagnostic_result())`, not `Err(...)`.** `GpuCounterError::into_diagnostic_result()` converts a counter failure into a single integer metric (e.g. `gpu_counter_permission_error`) in the `gpu counters` section. This is why a locked-down machine still produces a valid timing report - the diagnostic pass reports "counters unavailable" as a metric rather than failing the benchmark.
- **`Replay passes` is reported as a metric.** CUPTI range profiling can require several passes to collect all counters; the number varies by metric set and GPU. Reporting it lets you see whether the counter values came from one pass or several, which matters for interpreting them (more passes = more chance of perturbation).
- **The diagnostic pass calls `memset_default_stream` directly**, reusing the same bench function as the timing path. The work is identical; only the measurement wrapper differs.

### Registration

```rust,ignore
benchmark_main!(|runner| {
    runner.group::<CudaMemsetBench>("CUDA/counters", |g| {
        g.throughput(Throughput::bytes(BYTES_PER_OP))
            .measurement_domain(MeasurementDomain::Gpu)
            .backend(|| Box::new(CudaEventBackend::new(BYTES_PER_OP, FLOPS_PER_OP).unwrap()))
            .diagnostic_pass(memset_gpu_counters)
            .bench_sample("memset_async_default_stream", memset_default_stream);
    });
});
```

`measurement_domain(Gpu)` + `CudaEventBackend` suppress CPU-PMU diagnostics and provide device timing; `diagnostic_pass` adds the counter replay; `bench_sample` is the timing entry point. The three compose without interacting.

## What to look for

The output has two distinct sections:

**Standard stats table** (`results:`) - unchanged from [cuda_event_backend](./cuda-event-backend.md). The `Measurement` row reads `timing + CUDA events`, latency/throughput come from the CUDA event elapsed time, and `cuda_event_ms` / `host_overhead_ms` / `gpu_gib_s` appear as custom metrics. The CUPTI/NVPerf counters do **not** appear here - they are diagnostic-only and never define latency.

**`custom metrics:` table** - now contains the counter replay output. Expect to see:

- `Replay passes` (integer) - how many CUPTI replay passes were needed.
- `GPU memory peak` (`gpu_memory_peak_pct`, %) - `gpu__compute_memory_throughput.avg.pct_of_peak_sustained_elapsed`.
- `GPU L2 peak` (`gpu_l2_peak_pct`, %) - `lts__throughput.avg.pct_of_peak_sustained_elapsed`.
- `GPU SM peak` (`gpu_sm_peak_pct`, %) - `sm__throughput.avg.pct_of_peak_sustained_elapsed`.
- `GPU tensor active` (`gpu_tensor_active_pct`, %) - `sm__inst_executed_pipe_tensor.avg.pct_of_peak_sustained_active`.

`GpuCounterMetric::to_metric_value()` maps the long NVIDIA profiler names into the stable micromeasure names above. Unknown metrics are preserved under a generic `gpu_counter` name in the `gpu counters` section, so adding your own metric names to the collector does not lose data.

If counter collection failed, expect to see an integer error metric instead of the four percentage metrics:

| Error metric | `GpuCounterError` variant | Cause |
|---|---|---|
| `gpu_counter_permission_error` | `InsufficientPrivileges` | `ERR_NVGPUCTRPERM` / `CUPTI_ERROR_INSUFFICIENT_PRIVILEGES` - driver counter access denied |
| `gpu_counter_metric_error` | `MetricUnavailable` | a requested metric is not supported on this GPU/driver |
| `gpu_counter_profiler_error` | `ProfilerUnavailable` | CUPTI could not initialize or no CUDA context |
| `gpu_counter_collection_error` | `CollectionFailed` | begin/end/decode failed mid-collection |
| `gpu_counter_invalid_name` | `InvalidName` | a metric or range name contained an interior NUL byte |

The timing benchmark is valid in all of these cases; only the diagnostic counters were unavailable.

## Permissions

On many NVIDIA driver installations, performance counters require administrator-controlled permissions. The typical failure looks like `ERR_NVGPUCTRPERM` or `CUPTI_ERROR_INSUFFICIENT_PRIVILEGES`.

How to resolve it depends on your environment:

- **Bare metal / local dev box:** run under an account with counter access, or loosen the driver's counter permission policy (see NVIDIA's `nvidia-smi -pm` / counter-permission documentation for your driver version).
- **CI / container:** the container runtime may need to expose counter access; this is often gated by the driver and the runtime's device permissions.
- **Shared/locked-down host:** you may simply not have counter access. The benchmark still runs and reports timing; the diagnostic table reports the permission error as a metric instead of the four counter values.

This behaviour is intentional: counter collection is useful when available, but it should not make the main benchmark unusable on systems where it is locked down. The split between `bench_sample` (timing, always works) and `diagnostic_pass` (counters, best-effort) is what makes that possible.

## How it fits together

```text
timing samples (bench_sample + CudaEventBackend)
    -> latency, throughput, cuda_event_ms, host_overhead_ms, gpu_gib_s
diagnostic pass (GpuCounterCollector, after timing)
    -> Replay passes, gpu_memory_peak_pct, gpu_l2_peak_pct,
       gpu_sm_peak_pct, gpu_tensor_active_pct
    (or an integer error metric if counters are unavailable)
```

Both feed the same `custom metrics:` table, aggregated across the configured `diagnostic_samples` (default 1; raise it with `g.diagnostic_samples(n)` if the counters are noisy). The diagnostic pass does not contribute to latency, throughput, CV, or outlier statistics.