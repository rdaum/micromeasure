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

// Demonstrates CUPTI/NVPerf GPU counters collected through a diagnostic replay
// pass.
//
// The benchmark body enqueues cudaMemsetAsync operations on the default
// stream. The backend records CUDA events for timing, while the diagnostic pass
// replays the same work under the range profiler and reports GPU counters.
//
// Run on a CUDA-capable machine with:
//   cargo run --example gpu_counters --features gpu-counters --release

use micromeasure::{
    BenchContext, BenchSampleResult, CudaEventBackend, DEFAULT_NVIDIA_GPU_COUNTERS,
    DiagnosticError, DiagnosticResult, GpuCounterCollector, MeasurementDomain, MetricValue,
    Throughput, benchmark_main,
};
use std::ffi::c_void;
use std::ptr::null_mut;

type CudaErrorCode = i32;
type CudaStreamHandle = *mut c_void;

const CUDA_SUCCESS: CudaErrorCode = 0;
const BYTES_PER_OP: u64 = 16 * 1024 * 1024;
const FLOPS_PER_OP: u64 = 0;
const MEMSET_OPS_PER_SAMPLE: usize = 16;
const MAX_COUNTER_REPLAY_PASSES: i64 = 8;

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
    fn prepare(_chunk_size: usize) -> Self {
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
    let metrics = match collector.decode() {
        Ok(metrics) => metrics,
        Err(error) => return Ok(error.into_diagnostic_result()),
    };
    for metric in metrics {
        result = result.push_metric(metric.to_metric_value());
    }
    Ok(result)
}

benchmark_main!(|runner| {
    runner.group::<CudaMemsetBench>("CUDA/counters", |g| {
        g.throughput(Throughput::bytes(BYTES_PER_OP))
            .measurement_domain(MeasurementDomain::Gpu)
            .backend(|| Box::new(CudaEventBackend::new(BYTES_PER_OP, FLOPS_PER_OP).unwrap()))
            .diagnostic_pass(memset_gpu_counters)
            .bench_sample("memset_async_default_stream", memset_default_stream);
    });
});
