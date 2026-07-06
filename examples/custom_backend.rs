// Copyright 2026 Ryan Daum
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Demonstrates the MeasurementBackend trait wired into the runner. A
// custom backend replaces the default Linux perf / wall-clock path with
// domain-specific measurement. This example uses a simulated CUDA event
// backend — no CUDA dependency, but the shape matches what a real
// `CudaEventBackend` adapter would do.
//
// The backend records a fake "device elapsed" time around each sample,
// then in `collect()` it pushes two custom metrics:
//   - `cuda_event_ms`: the (simulated) device-side elapsed time
//   - `host_overhead_ms`: host wall-clock minus device time
//
// Because the bench function also returns a `BenchSampleResult` with its
// own `tflops` metric, the final `custom metrics:` table shows metrics
// from BOTH the bench function and the backend — the backend appends to
// the same Vec.
//
// Run with:
//   cargo run --example custom_backend --release

use micromeasure::{
    BenchContext, BenchSampleResult, MeasurementBackend, MeasurementDomain, MetricValue,
    Throughput, benchmark_main, black_box,
};
use std::time::{Duration, Instant};

struct FakeGpuBench;

impl BenchContext for FakeGpuBench {
    fn prepare(_num_chunks: usize) -> Self {
        FakeGpuBench
    }

    fn chunk_size() -> Option<usize> {
        Some(4_000)
    }
}

/// A simulated CUDA event backend. A real implementation would hold
/// `cuda::Event` / `cuda::Stream` handles and call `cudaEventRecord` /
/// `cudaEventSynchronize` / `cudaEventElapsedTime` in begin/end/collect.
struct FakeCudaEventBackend {
    device_start: Option<Instant>,
    device_elapsed: Duration,
}

impl FakeCudaEventBackend {
    fn new() -> Self {
        Self {
            device_start: None,
            device_elapsed: Duration::ZERO,
        }
    }
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
            // Simulate the device taking 85% of the host wall-clock time.
            let host = start.elapsed();
            self.device_elapsed = Duration::from_secs_f64(host.as_secs_f64() * 0.85);
        }
    }

    fn collect(
        &mut self,
        host_elapsed: Duration,
        ops: u64,
        chunk_index: usize,
        results: &mut micromeasure::bench::Results,
        metrics: &mut Vec<MetricValue>,
    ) {
        // The backend owns timing + iteration count, same as the default
        // backends. Leaving has_* flags false suppresses CPU PMU rows and
        // diagnostics — correct for a GPU benchmark.
        results.duration = host_elapsed;
        results.iterations = ops;
        results.chunks_executed = 1;

        let device_ms = self.device_elapsed.as_secs_f64() * 1000.0;
        let host_ms = host_elapsed.as_secs_f64() * 1000.0;
        let overhead_ms = (host_ms - device_ms).max(0.0);

        metrics.push(MetricValue::new("cuda_event_ms", device_ms, "ms"));
        metrics.push(MetricValue::new("host_overhead_ms", overhead_ms, "ms"));

        // chunk_index is available for per-sample correlation (e.g. skip
        // warmup, log algorithm switches). Here we just touch it.
        let _ = chunk_index;
    }

    fn measurement_label(&self) -> &'static str {
        "timing + CUDA events"
    }

    fn emits_cpu_diagnostics(&self) -> bool {
        false
    }
}

fn fake_gpu_kernel(
    _ctx: &mut FakeGpuBench,
    chunk_size: usize,
    _chunk_num: usize,
) -> BenchSampleResult {
    let start = Instant::now();

    // Stand-in for one synchronized cuBLASLt FP4 GEMM per iteration.
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        acc = acc.wrapping_add(black_box(i));
    }
    black_box(acc);

    let device_ms = start.elapsed().as_secs_f64() * 1000.0 * 0.85;
    let tflops = (chunk_size as f64 * 1024.0) / (device_ms * 1e9).max(1e-9);

    BenchSampleResult::operations(chunk_size as u64).with_metric("tflops", tflops, "TFLOP/s")
}

benchmark_main!(|runner| {
    runner.group::<FakeGpuBench>("GPU/backend", |g| {
        // The measurement_domain + backend work together: the domain
        // suppresses CPU-PMU diagnostics, the backend provides
        // device-side timing. The factory creates a fresh backend per
        // benchmark run.
        g.throughput(Throughput::bytes(8))
            .measurement_domain(MeasurementDomain::Gpu)
            .backend(|| Box::new(FakeCudaEventBackend::new()))
            .bench_sample("fp4_gemm", fake_gpu_kernel);
    });
});
