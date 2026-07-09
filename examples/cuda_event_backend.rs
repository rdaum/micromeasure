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

// Demonstrates the feature-gated CUDA event backend with real default-stream
// GPU work.
//
// The benchmark body enqueues cudaMemsetAsync operations on the default
// stream. The backend records CUDA events around each sample and uses device
// elapsed time for latency/throughput, while also reporting host overhead.
//
// Run on a CUDA-capable machine with:
//   cargo run --example cuda_event_backend --features cuda --release

use micromeasure::{
    BenchContext, BenchSampleResult, CudaEventBackend, MeasurementDomain, Throughput,
    benchmark_main,
};
use std::ffi::c_void;
use std::ptr::null_mut;

type CudaErrorCode = i32;
type CudaStreamHandle = *mut c_void;

const CUDA_SUCCESS: CudaErrorCode = 0;
const BYTES_PER_OP: u64 = 16 * 1024 * 1024;
const FLOPS_PER_OP: u64 = 0;
const MEMSET_OPS_PER_SAMPLE: usize = 16;

#[link(name = "cudart")]
unsafe extern "C" {
    fn cudaMalloc(ptr: *mut *mut c_void, size: usize) -> CudaErrorCode;
    fn cudaFree(ptr: *mut c_void) -> CudaErrorCode;
    fn cudaMemsetAsync(
        ptr: *mut c_void,
        value: i32,
        count: usize,
        stream: CudaStreamHandle,
    ) -> CudaErrorCode;
}

fn check_cuda(call: &'static str, status: CudaErrorCode) {
    assert_eq!(
        status, CUDA_SUCCESS,
        "{call} failed with CUDA status {status}"
    );
}

struct CudaMemsetBench {
    device_buffer: *mut c_void,
}

impl BenchContext for CudaMemsetBench {
    fn prepare(_num_chunks: usize) -> Self {
        let mut device_buffer = null_mut();
        unsafe {
            check_cuda(
                "cudaMalloc",
                cudaMalloc(&mut device_buffer, BYTES_PER_OP as usize),
            );
        }
        Self { device_buffer }
    }

    fn chunk_size() -> Option<usize> {
        Some(MEMSET_OPS_PER_SAMPLE)
    }
}

impl Drop for CudaMemsetBench {
    fn drop(&mut self) {
        if !self.device_buffer.is_null() {
            unsafe {
                let _ = cudaFree(self.device_buffer);
            }
        }
    }
}

fn memset_default_stream(
    ctx: &mut CudaMemsetBench,
    chunk_size: usize,
    chunk_num: usize,
) -> BenchSampleResult {
    for op in 0..chunk_size {
        let pattern = ((chunk_num + op) & 0xff) as i32;
        unsafe {
            check_cuda(
                "cudaMemsetAsync",
                cudaMemsetAsync(
                    ctx.device_buffer,
                    pattern,
                    BYTES_PER_OP as usize,
                    null_mut(),
                ),
            );
        }
    }

    BenchSampleResult::operations(chunk_size as u64)
}

benchmark_main!(|runner| {
    runner.group::<CudaMemsetBench>("CUDA/events", |g| {
        g.throughput(Throughput::bytes(BYTES_PER_OP))
            .measurement_domain(MeasurementDomain::Gpu)
            .backend(|| Box::new(CudaEventBackend::new(BYTES_PER_OP, FLOPS_PER_OP).unwrap()))
            .bench_sample("memset_async_default_stream", memset_default_stream);
    });
});
