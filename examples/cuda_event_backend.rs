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

// Demonstrates the feature-gated CUDA event backend.
//
// The benchmark body should enqueue CUDA work on the default stream. The
// backend records CUDA events around each sample and uses device elapsed time
// for latency/throughput, while also reporting host overhead.
//
// Run on a CUDA-capable machine with:
//   cargo run --example cuda_event_backend --features cuda --release

use micromeasure::{
    CudaEventBackend, MeasurementDomain, NoContext, Throughput, benchmark_main, black_box,
};

const BYTES_PER_OP: u64 = 16 * 1024;
const FLOPS_PER_OP: u64 = 2 * 1024;

fn enqueue_default_stream_work(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    // Replace this with a kernel launch or library call that enqueues work on
    // CUDA's default stream. The loop keeps this example self-contained; CUDA
    // events around an otherwise empty default stream will measure near-zero
    // device time.
    for i in 0..chunk_size {
        black_box(i);
    }
}

benchmark_main!(|runner| {
    runner.group::<NoContext>("CUDA/events", |g| {
        g.throughput(Throughput::bytes(BYTES_PER_OP))
            .measurement_domain(MeasurementDomain::Gpu)
            .backend(|| Box::new(CudaEventBackend::new(BYTES_PER_OP, FLOPS_PER_OP).unwrap()))
            .bench("default_stream_work", enqueue_default_stream_work);
    });
});
