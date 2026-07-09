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

// Demonstrates Phase 1 of the GPU benchmarking work
// (book/src/gpu-sharp-edges.md): declaring a benchmark group as
// `MeasurementDomain::Gpu` so the runner suppresses CPU-PMU bottleneck
// diagnostics and relabels the PMU coverage byline as
// "host PMU (orchestration)".
//
// The kernel here is a stand-in for one synchronized device operation
// (e.g. `cublasLtMatmul()` + `cudaDeviceSynchronize()`). On a real GPU
// benchmark the host thread is mostly idle, so CPU PMU counters describe
// launch/sync orchestration, not the device kernel. A previous run of
// this example with `MeasurementDomain::Cpu` produced:
//
//     possible bottlenecks:
//       - Likely data-side memory latency: backend stall is 89.99% with
//         cache pressure at 0.00% miss rate and 0.0000 misses/op
//
// which is misleading for GPU work. With `MeasurementDomain::Gpu`:
//
//   - the "possible bottlenecks:" section is suppressed entirely
//   - the PMU coverage byline reads:
//       host PMU (orchestration): coverage=100.0% ...
//
// Run with:
//   cargo run --example gpu_domain --release

use micromeasure::{MeasurementDomain, NoContext, Throughput, benchmark_main, black_box};

fn fake_gpu_kernel(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    // Stand-in for one synchronized device operation. In a real GPU
    // benchmark this would call cublasLtMatmul() + cudaDeviceSynchronize()
    // and the host thread would mostly wait. CPU PMU counters here
    // describe host orchestration, not the device kernel.
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
