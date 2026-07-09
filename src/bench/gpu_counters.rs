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

//! NVIDIA CUPTI/NVPerf range-profiler counters.
//!
//! This module is available behind the `gpu-counters` feature. It is intended
//! for diagnostic replay passes, not the normal timing loop.

use crate::{DiagnosticError, DiagnosticResult, MetricValue};
use std::ffi::{CStr, CString, c_char, c_double, c_void};
use std::fmt;
use std::ptr::null_mut;

/// A small default set of NVIDIA GPU profiler metrics with stable
/// micromeasure display mappings.
pub const DEFAULT_NVIDIA_GPU_COUNTERS: &[&str] = &[
    "gpu__compute_memory_throughput.avg.pct_of_peak_sustained_elapsed",
    "lts__throughput.avg.pct_of_peak_sustained_elapsed",
    "sm__throughput.avg.pct_of_peak_sustained_elapsed",
    "sm__inst_executed_pipe_tensor.avg.pct_of_peak_sustained_active",
];

unsafe extern "C" {
    fn micromeasure_gpu_counter_create(
        metric_names: *const *const c_char,
        metric_count: usize,
        out_handle: *mut *mut c_void,
        error: *mut c_char,
        error_len: usize,
    ) -> i32;
    fn micromeasure_gpu_counter_begin(
        handle: *mut c_void,
        range_name: *const c_char,
        error: *mut c_char,
        error_len: usize,
    ) -> i32;
    fn micromeasure_gpu_counter_end(
        handle: *mut c_void,
        all_passes_submitted: *mut i32,
        error: *mut c_char,
        error_len: usize,
    ) -> i32;
    fn micromeasure_gpu_counter_decode(
        handle: *mut c_void,
        error: *mut c_char,
        error_len: usize,
    ) -> i32;
    fn micromeasure_gpu_counter_value_count(handle: *mut c_void) -> usize;
    fn micromeasure_gpu_counter_value(
        handle: *mut c_void,
        index: usize,
        name: *mut *const c_char,
        value: *mut c_double,
    ) -> i32;
    fn micromeasure_gpu_counter_destroy(handle: *mut c_void);
}

/// Convenient result alias for GPU counter operations.
pub type GpuCounterResult<T> = Result<T, GpuCounterError>;

/// Error returned when CUPTI/NVPerf counter collection fails.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum GpuCounterError {
    /// CUPTI/NVPerf counter collection needs elevated counter permissions.
    InsufficientPrivileges(String),
    /// One or more requested metrics are unavailable on the current GPU.
    MetricUnavailable(String),
    /// CUPTI/NVPerf could not initialize or is unavailable.
    ProfilerUnavailable(String),
    /// Counter collection failed during begin/end/decode.
    CollectionFailed(String),
    /// A requested metric or range name contained an interior NUL byte.
    InvalidName(&'static str),
}

impl GpuCounterError {
    fn from_detail(detail: String) -> Self {
        let lower = detail.to_ascii_lowercase();
        if detail.contains("ERR_NVGPUCTRPERM")
            || detail.contains("CUPTI_ERROR_INSUFFICIENT_PRIVILEGES")
            || lower.contains("permission")
            || lower.contains("privilege")
        {
            Self::InsufficientPrivileges(detail)
        } else if lower.contains("configaddmetrics")
            || lower.contains("metric")
            || lower.contains("unavailable")
        {
            Self::MetricUnavailable(detail)
        } else if lower.contains("initialize")
            || lower.contains("getchipname")
            || lower.contains("counteravailability")
            || lower.contains("no current cuda context")
        {
            Self::ProfilerUnavailable(detail)
        } else {
            Self::CollectionFailed(detail)
        }
    }

    /// Machine-friendly metric name for representing this error in a
    /// diagnostic result.
    pub fn metric_name(&self) -> &'static str {
        match self {
            Self::InsufficientPrivileges(_) => "gpu_counter_permission_error",
            Self::MetricUnavailable(_) => "gpu_counter_metric_error",
            Self::ProfilerUnavailable(_) => "gpu_counter_profiler_error",
            Self::CollectionFailed(_) => "gpu_counter_collection_error",
            Self::InvalidName(_) => "gpu_counter_invalid_name",
        }
    }

    /// Human-readable metric display name for representing this error in a
    /// diagnostic result.
    pub fn display_name(&self) -> &'static str {
        match self {
            Self::InsufficientPrivileges(_) => "Counter permission error",
            Self::MetricUnavailable(_) => "Counter metric error",
            Self::ProfilerUnavailable(_) => "Counter profiler error",
            Self::CollectionFailed(_) => "Counter collection error",
            Self::InvalidName(_) => "Counter invalid name",
        }
    }

    /// Converts this error into a `gpu counters` diagnostic result containing
    /// one integer error metric.
    pub fn into_diagnostic_result(self) -> DiagnosticResult {
        DiagnosticResult::new("gpu counters").push_metric(
            MetricValue::integer(self.metric_name(), 1, "errors")
                .with_display_name(self.display_name()),
        )
    }
}

impl fmt::Display for GpuCounterError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InsufficientPrivileges(detail)
            | Self::MetricUnavailable(detail)
            | Self::ProfilerUnavailable(detail)
            | Self::CollectionFailed(detail) => f.write_str(detail),
            Self::InvalidName(label) => write!(f, "{label} contains NUL"),
        }
    }
}

impl std::error::Error for GpuCounterError {}

impl From<GpuCounterError> for DiagnosticError {
    fn from(error: GpuCounterError) -> Self {
        Self::new(error.to_string())
    }
}

/// One evaluated GPU performance counter metric.
#[derive(Clone, Debug)]
pub struct GpuCounterMetric {
    /// Profiler metric name.
    pub name: String,
    /// Evaluated metric value for the profiled range.
    pub value: f64,
}

impl GpuCounterMetric {
    /// Maps known NVIDIA profiler metric names into stable micromeasure metric
    /// names and display labels. Unknown metrics are preserved under the
    /// generic `gpu_counter` name.
    pub fn to_metric_value(&self) -> MetricValue {
        match self.name.as_str() {
            "gpu__compute_memory_throughput.avg.pct_of_peak_sustained_elapsed" => {
                MetricValue::new("gpu_memory_peak_pct", self.value, "%")
                    .with_display_name("GPU memory peak")
            }
            "lts__throughput.avg.pct_of_peak_sustained_elapsed" => {
                MetricValue::new("gpu_l2_peak_pct", self.value, "%")
                    .with_display_name("GPU L2 peak")
            }
            "sm__throughput.avg.pct_of_peak_sustained_elapsed" => {
                MetricValue::new("gpu_sm_peak_pct", self.value, "%")
                    .with_display_name("GPU SM peak")
            }
            "sm__inst_executed_pipe_tensor.avg.pct_of_peak_sustained_active" => {
                MetricValue::new("gpu_tensor_active_pct", self.value, "%")
                    .with_display_name("GPU tensor active")
            }
            _ => MetricValue::new("gpu_counter", self.value, "")
                .with_display_name("GPU counter")
                .with_section("gpu counters"),
        }
    }
}

/// CUPTI/NVPerf range-profiler collector for diagnostic microbenchmark
/// passes.
pub struct GpuCounterCollector {
    handle: *mut c_void,
    _metric_names: Vec<CString>,
    range_name: CString,
}

impl GpuCounterCollector {
    /// Creates a collector for `metrics` on the current CUDA context.
    pub fn new(metrics: &[&str], range_name: &str) -> GpuCounterResult<Self> {
        let metric_names = metrics
            .iter()
            .map(|metric| {
                CString::new(*metric).map_err(|_| GpuCounterError::InvalidName("metric name"))
            })
            .collect::<GpuCounterResult<Vec<_>>>()?;
        let metric_ptrs = metric_names
            .iter()
            .map(|metric| metric.as_ptr())
            .collect::<Vec<_>>();
        let range_name =
            CString::new(range_name).map_err(|_| GpuCounterError::InvalidName("range name"))?;

        let mut handle = null_mut();
        let mut error = ErrorBuffer::new();
        let status = unsafe {
            micromeasure_gpu_counter_create(
                metric_ptrs.as_ptr(),
                metric_ptrs.len(),
                &mut handle,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status != 0 {
            return Err(GpuCounterError::from_detail(error.message()));
        }
        Ok(Self {
            handle,
            _metric_names: metric_names,
            range_name,
        })
    }

    /// Starts one CUPTI user replay profiling pass and pushes the configured
    /// range.
    pub fn begin(&mut self) -> GpuCounterResult<()> {
        let mut error = ErrorBuffer::new();
        let status = unsafe {
            micromeasure_gpu_counter_begin(
                self.handle,
                self.range_name.as_ptr(),
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status == 0 {
            Ok(())
        } else {
            Err(GpuCounterError::from_detail(error.message()))
        }
    }

    /// Pops the current range, stops the pass, and returns whether all replay
    /// passes are complete.
    pub fn end(&mut self) -> GpuCounterResult<bool> {
        let mut all_passes_submitted = 0;
        let mut error = ErrorBuffer::new();
        let status = unsafe {
            micromeasure_gpu_counter_end(
                self.handle,
                &mut all_passes_submitted,
                error.as_mut_ptr(),
                error.len(),
            )
        };
        if status == 0 {
            Ok(all_passes_submitted != 0)
        } else {
            Err(GpuCounterError::from_detail(error.message()))
        }
    }

    /// Decodes collected counter data after all replay passes have completed.
    pub fn decode(&mut self) -> GpuCounterResult<Vec<GpuCounterMetric>> {
        let mut error = ErrorBuffer::new();
        let status = unsafe {
            micromeasure_gpu_counter_decode(self.handle, error.as_mut_ptr(), error.len())
        };
        if status != 0 {
            return Err(GpuCounterError::from_detail(error.message()));
        }

        let count = unsafe { micromeasure_gpu_counter_value_count(self.handle) };
        let mut metrics = Vec::with_capacity(count);
        for index in 0..count {
            let mut name: *const c_char = std::ptr::null();
            let mut value = 0.0 as c_double;
            let status = unsafe {
                micromeasure_gpu_counter_value(self.handle, index, &mut name, &mut value)
            };
            if status != 0 || name.is_null() {
                return Err(GpuCounterError::CollectionFailed(
                    "GPU counter value read failed".to_string(),
                ));
            }
            let name = unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned();
            metrics.push(GpuCounterMetric { name, value });
        }
        Ok(metrics)
    }
}

impl Drop for GpuCounterCollector {
    fn drop(&mut self) {
        if !self.handle.is_null() {
            unsafe {
                micromeasure_gpu_counter_destroy(self.handle);
            }
        }
    }
}

struct ErrorBuffer {
    bytes: [c_char; 1024],
}

impl ErrorBuffer {
    fn new() -> Self {
        Self { bytes: [0; 1024] }
    }

    fn as_mut_ptr(&mut self) -> *mut c_char {
        self.bytes.as_mut_ptr()
    }

    fn len(&self) -> usize {
        self.bytes.len()
    }

    fn message(&self) -> String {
        unsafe { CStr::from_ptr(self.bytes.as_ptr()) }
            .to_string_lossy()
            .into_owned()
    }
}
