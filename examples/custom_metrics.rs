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

// Demonstrates Phase 2 of the GPU benchmarking work
// (book/src/gpu-sharp-edges.md): per-sample custom metrics via
// `bench_sample(...)` returning a `BenchSampleResult`.
//
// The bench function here is a stand-in for one synchronized GPU operation.
// In a real GPU benchmark it would record a CUDA event around the kernel,
// then return a `BenchSampleResult` carrying `cuda_event_ms` and
// `host_overhead_ms` derived from the host wall-clock span minus the
// device event elapsed time. Here the values are synthetic (derived from
// the chunk index) so the example runs without CUDA linked.
//
// The runner:
//   - routes `operations` through the existing throughput/latency path
//   - aggregates `cuda_event_ms` / `host_overhead_ms` / `tflops` across
//     samples (mean, median, p95, min, max, sample count)
//   - renders a `custom metrics:` table beneath the standard stats table
//   - persists the summaries into BenchmarkStats.metrics (JSON output)
//
// Run with:
//   cargo run --example custom_metrics --release

use micromeasure::{
    BenchContext, BenchSampleResult, MeasurementDomain, MetricValue, Throughput, benchmark_main,
    black_box,
};
use std::time::{Duration, Instant};

struct FakeGpuBench;

impl BenchContext for FakeGpuBench {
    fn prepare(_num_chunks: usize) -> Self {
        FakeGpuBench
    }

    // Fixed chunk size: GPU work does not auto-scale the way CPU work
    // does (launch overhead dominates small chunks, algorithm selection
    // can change by shape). See "Calibration Assumes CPU-Like Chunk
    // Scaling" in the sharp-edges doc.
    fn chunk_size() -> Option<usize> {
        Some(4_000)
    }
}

fn fake_gpu_kernel_with_metrics(
    _ctx: &mut FakeGpuBench,
    chunk_size: usize,
    chunk_num: usize,
) -> BenchSampleResult {
    let host_start = Instant::now();

    // Stand-in for one synchronized cuBLASLt FP4 GEMM per iteration. On
    // a real GPU this is dominated by `cudaDeviceSynchronize()` waiting
    // for the kernel to complete.
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        acc = acc.wrapping_add(black_box(i));
    }
    black_box(acc);

    let host_elapsed_ms = host_start.elapsed().as_secs_f64() * 1_000.0;

    // Pretend the device work took a fixed fraction of host wall-clock
    // time. In a real backend this would come from a CUDA event pair:
    //   start.record(); kernel.launch(); stop.record();
    //   stop.synchronize(); device_ms = start.elapsed_ms(&stop);
    let device_ms = host_elapsed_ms * 0.85;
    let overhead_ms = (host_elapsed_ms - device_ms).max(0.0);

    // Pretend the GEMM did M*N*K FP4 FMA ops, here scaled to chunk_size so
    // the number is illustrative.
    let flops = (chunk_size as u64) * 1024;
    let device_seconds = device_ms / 1000.0;

    BenchSampleResult::operations(chunk_size as u64)
        // Derived helper: duration_ms(name, Duration)
        .push_metric(
            MetricValue::duration_ms("cuda_event_ms", Duration::from_secs_f64(device_seconds))
                .with_display_name("CUDA event time"),
        )
        // Plain constructor with display_name
        .push_metric(
            MetricValue::new("host_overhead_ms", overhead_ms, "ms")
                .with_display_name("Host overhead"),
        )
        // Derived helper: throughput_tflops(name, flops, seconds)
        .push_metric(
            MetricValue::throughput_tflops("tflops", flops, device_seconds)
                .with_display_name("TFLOP/s"),
        )
        // Integer-formatted metric: no decimal places, no scientific
        // notation. The selected_algo_id shifts with the chunk number,
        // exercising aggregation of a value that changes between samples.
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
