# custom_backend

**File:** `examples/custom_backend.rs`
**Run:**

```sh
cargo run --example custom_backend --release
```

## What it demonstrates

The `MeasurementBackend` trait wired into the runner with a simulated CUDA event backend — no CUDA dependency, but the shape matches what a real `CudaEventBackend` adapter would do. The backend records a fake "device elapsed" time around each sample, then in `collect()` pushes `cuda_event_ms` and `host_overhead_ms`. Because the bench function also returns a `BenchSampleResult` with its own `tflops` metric, the final `custom metrics:` table shows metrics from **both** the bench function and the backend.

## Key code

```rust,ignore
struct FakeCudaEventBackend {
    device_start: Option<Instant>,
    device_elapsed: Duration,
}

impl MeasurementBackend for FakeCudaEventBackend {
    fn begin(&mut self) {
        // A real backend calls `start_event.record(&stream)` here.
        self.device_start = Some(Instant::now());
    }

    fn end(&mut self) {
        // A real backend calls `stop_event.record(&stream)`,
        // `stop_event.synchronize()`, then
        // `device_elapsed = start_event.elapsed_ms(&stop_event)`.
        if let Some(start) = self.device_start.take() {
            let host = start.elapsed();
            self.device_elapsed = Duration::from_secs_f64(host.as_secs_f64() * 0.85);
        }
    }

    fn collect(
        &mut self,
        host_elapsed: Duration,
        ops: u64,
        _chunk_index: usize,
        results: &mut micromeasure::bench::Results,
        metrics: &mut Vec<MetricValue>,
    ) {
        // Leaving has_* flags false suppresses CPU PMU rows and
        // diagnostics — correct for a GPU benchmark.
        results.duration = host_elapsed;
        results.iterations = ops;
        results.chunks_executed = 1;

        let device_ms = self.device_elapsed.as_secs_f64() * 1000.0;
        let host_ms = host_elapsed.as_secs_f64() * 1000.0;
        let overhead_ms = (host_ms - device_ms).max(0.0);

        metrics.push(MetricValue::new("cuda_event_ms", device_ms, "ms"));
        metrics.push(MetricValue::new("host_overhead_ms", overhead_ms, "ms"));
    }

    fn measurement_label(&self) -> &'static str { "timing + CUDA events" }
    fn emits_cpu_diagnostics(&self) -> bool { false }
}
```

Registration:

```rust,ignore
runner.group::<FakeGpuBench>("GPU/backend", |g| {
    g.throughput(Throughput::bytes(8))
        .measurement_domain(MeasurementDomain::Gpu)
        .backend(|| Box::new(FakeCudaEventBackend::new()))
        .bench_sample("fp4_gemm", fake_gpu_kernel);
});
```

## What to look for

- The `Measurement` row reads `timing + CUDA events` (from `measurement_label()`), not the default `timing + PMU` or `timing only`.
- The `custom metrics:` table contains **three** metrics: `cuda_event_ms` and `host_overhead_ms` pushed by the backend, and `tflops` returned by the bench function's `BenchSampleResult`. Backend and bench metrics are merged into one table.
- `emits_cpu_diagnostics() == false` plus `MeasurementDomain::Gpu` together suppress CPU-PMU diagnostics and CPU-PMU stats rows. The backend leaves all `has_*` flags false, so no PMU rows appear.
- `results.duration = host_elapsed` here (the fake backend doesn't really change the duration). A real CUDA-event backend would set `results.duration` to the device event time so throughput/latency describe the kernel — see [cuda_event_backend](./cuda-event-backend.md) for that pattern.

## Why this example exists

It shows the full pluggable-backend contract without requiring a CUDA toolkit or GPU. Copy this shape for any device API (Vulkan, Metal, oneAPI,ROCm) — implement `begin`/`end`/`collect`, decide what `results.duration` means, push your metrics, set `emits_cpu_diagnostics()` appropriately.