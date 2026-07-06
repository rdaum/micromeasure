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

// Demonstrates Phase 1 behavior for a `MeasurementDomain::Mixed` benchmark:
// one where CPU work and device work are interleaved on the host thread
// (e.g. a GPU benchmark that does meaningful host-side data layout or
// scale-tile reshaping between kernel launches).
//
// Unlike the `gpu_domain` example, Mixed does NOT suppress CPU-PMU
// bottleneck diagnostics outright — the CPU work is real work, after all.
// Instead it prefixes them with `[host]` so the reader knows the signal
// is host-side context, not a description of the device portion:
//
//     possible bottlenecks:
//       - [host] Likely data-side memory latency: backend stall is 50.0% ...
//
// The PMU coverage byline also reads "host PMU (mixed workload)".
//
// Run with:
//   cargo run --example mixed_domain --release

use micromeasure::{MeasurementDomain, NoContext, Throughput, benchmark_main, black_box};

// A workload that interleaves CPU data reshaping with a stand-in for a
// synchronized device call. The intent is realistic: real GPU workflows
// often have host-side work in the measured loop.
fn mixed_cpu_gpu_work(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        // Stand-in for layout/scale reshaping that touches host cache.
        acc = acc.wrapping_add(black_box(i));
        // Stand-in for a kernel launch + synchronize. The real version
        // would call into cuBLASLt or CUDA and block on the device.
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
