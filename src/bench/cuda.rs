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

//! CUDA event timing backend.
//!
//! This module is available behind the `cuda` feature. It links against the
//! CUDA runtime (`cudart`) and provides a small default-stream event backend
//! for CUDA microbenchmarks.

use crate::bench::Results;
use crate::{MeasurementBackend, MetricValue};
use std::ffi::c_void;
use std::fmt;
use std::ptr::null_mut;
use std::time::{Duration, Instant};

type CudaErrorCode = i32;
type CudaEventHandle = *mut c_void;
type CudaStreamHandle = *mut c_void;

const CUDA_SUCCESS: CudaErrorCode = 0;

#[link(name = "cudart")]
unsafe extern "C" {
    fn cudaEventCreate(event: *mut CudaEventHandle) -> CudaErrorCode;
    fn cudaEventDestroy(event: CudaEventHandle) -> CudaErrorCode;
    fn cudaEventRecord(event: CudaEventHandle, stream: CudaStreamHandle) -> CudaErrorCode;
    fn cudaEventSynchronize(event: CudaEventHandle) -> CudaErrorCode;
    fn cudaEventElapsedTime(
        ms: *mut f32,
        start: CudaEventHandle,
        end: CudaEventHandle,
    ) -> CudaErrorCode;
}

/// Convenient result alias for CUDA backend operations.
pub type CudaResult<T> = Result<T, CudaError>;

/// Error returned when a CUDA runtime call fails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CudaError {
    call: &'static str,
    code: CudaErrorCode,
}

impl CudaError {
    fn new(call: &'static str, code: CudaErrorCode) -> Self {
        Self { call, code }
    }

    /// Name of the CUDA runtime call that failed.
    pub fn call(&self) -> &'static str {
        self.call
    }

    /// Raw CUDA runtime error code.
    pub fn code(&self) -> CudaErrorCode {
        self.code
    }
}

impl fmt::Display for CudaError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} failed with CUDA status {}", self.call, self.code)
    }
}

impl std::error::Error for CudaError {}

fn check_cuda(call: &'static str, status: CudaErrorCode) -> CudaResult<()> {
    if status == CUDA_SUCCESS {
        Ok(())
    } else {
        Err(CudaError::new(call, status))
    }
}

/// CUDA runtime event used by [`CudaEventBackend`].
pub struct CudaEvent {
    event: CudaEventHandle,
}

impl CudaEvent {
    /// Creates a CUDA event with the runtime defaults.
    pub fn new() -> CudaResult<Self> {
        let mut event = null_mut();
        unsafe {
            check_cuda("cudaEventCreate", cudaEventCreate(&mut event))?;
        }
        Ok(Self { event })
    }

    /// Records this event on the CUDA default stream.
    pub fn record_default_stream(&self) -> CudaResult<()> {
        unsafe { check_cuda("cudaEventRecord", cudaEventRecord(self.event, null_mut())) }
    }

    /// Blocks until this event has completed.
    pub fn synchronize(&self) -> CudaResult<()> {
        unsafe { check_cuda("cudaEventSynchronize", cudaEventSynchronize(self.event)) }
    }

    /// Returns elapsed device time from `self` to `end`, in milliseconds.
    pub fn elapsed_ms_until(&self, end: &Self) -> CudaResult<f32> {
        let mut ms = 0.0f32;
        unsafe {
            check_cuda(
                "cudaEventElapsedTime",
                cudaEventElapsedTime(&mut ms, self.event, end.event),
            )?;
        }
        Ok(ms)
    }
}

impl Drop for CudaEvent {
    fn drop(&mut self) {
        if !self.event.is_null() {
            unsafe {
                let _ = cudaEventDestroy(self.event);
            }
        }
    }
}

/// [`MeasurementBackend`] backed by CUDA event timing on the default stream.
///
/// This backend is intended for benchmark closures that enqueue CUDA work on
/// the default stream. It records a start event in [`MeasurementBackend::begin`],
/// records and synchronizes a stop event in [`MeasurementBackend::end`], and
/// uses device elapsed time as the benchmark sample duration.
pub struct CudaEventBackend {
    start: CudaEvent,
    stop: CudaEvent,
    host_start: Option<Instant>,
    host_elapsed: Duration,
    device_ms: f64,
    bytes_per_op: u64,
    flops_per_op: u64,
    last_error: Option<CudaError>,
}

impl CudaEventBackend {
    /// Creates a CUDA event backend.
    ///
    /// `bytes_per_op` and `flops_per_op` describe one operation reported by
    /// the benchmark closure. Non-zero values are used to derive `gpu_gib_s`
    /// and `gpu_tflops` from CUDA event elapsed time.
    pub fn new(bytes_per_op: u64, flops_per_op: u64) -> CudaResult<Self> {
        Ok(Self {
            start: CudaEvent::new()?,
            stop: CudaEvent::new()?,
            host_start: None,
            host_elapsed: Duration::ZERO,
            device_ms: 0.0,
            bytes_per_op,
            flops_per_op,
            last_error: None,
        })
    }

    fn record_error(&mut self, error: CudaError) {
        self.last_error = Some(error);
        self.device_ms = 0.0;
    }
}

impl MeasurementBackend for CudaEventBackend {
    fn begin(&mut self) {
        self.last_error = None;
        self.host_start = Some(Instant::now());
        if let Err(error) = self.start.record_default_stream() {
            self.record_error(error);
        }
    }

    fn end(&mut self) {
        self.host_elapsed = self
            .host_start
            .take()
            .map_or(Duration::ZERO, |start| start.elapsed());

        if self.last_error.is_some() {
            return;
        }
        if let Err(error) = self.stop.record_default_stream() {
            self.record_error(error);
            return;
        }
        if let Err(error) = self.stop.synchronize() {
            self.record_error(error);
            return;
        }
        match self.start.elapsed_ms_until(&self.stop) {
            Ok(device_ms) => self.device_ms = f64::from(device_ms),
            Err(error) => self.record_error(error),
        }
    }

    fn collect(
        &mut self,
        host_elapsed: Duration,
        ops: u64,
        _chunk_index: usize,
        results: &mut Results,
        metrics: &mut Vec<MetricValue>,
    ) {
        let host_elapsed = if self.host_elapsed.is_zero() {
            host_elapsed
        } else {
            self.host_elapsed
        };

        results.iterations = ops;
        results.chunks_executed = 1;

        if let Some(error) = &self.last_error {
            results.duration = host_elapsed;
            metrics.push(
                MetricValue::integer("cuda_error_code", i64::from(error.code()), "")
                    .with_display_name("CUDA error"),
            );
            return;
        }

        let device_duration = Duration::from_secs_f64(self.device_ms / 1_000.0);
        let device_seconds = device_duration.as_secs_f64().max(f64::MIN_POSITIVE);
        let host_overhead = host_elapsed.saturating_sub(device_duration);
        let total_bytes = self.bytes_per_op.saturating_mul(ops);
        let total_flops = self.flops_per_op.saturating_mul(ops);

        results.duration = device_duration;

        metrics.push(
            MetricValue::duration_ms("cuda_event_ms", device_duration)
                .with_display_name("CUDA event"),
        );
        metrics.push(
            MetricValue::duration_ms("host_overhead_ms", host_overhead)
                .with_display_name("Host overhead"),
        );
        if total_bytes > 0 {
            metrics.push(
                MetricValue::bandwidth_gib_s("gpu_gib_s", total_bytes, device_seconds)
                    .with_display_name("GPU bandwidth"),
            );
        }
        if total_flops > 0 {
            metrics.push(
                MetricValue::throughput_tflops("gpu_tflops", total_flops, device_seconds)
                    .with_display_name("GPU throughput"),
            );
        }
    }

    fn measurement_label(&self) -> &'static str {
        "timing + CUDA events"
    }

    fn emits_cpu_diagnostics(&self) -> bool {
        false
    }
}
