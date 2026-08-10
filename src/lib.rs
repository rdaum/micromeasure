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

//! Microbenchmark harness for tiny operations where PMU behaviour matters.
//!
//! This crate provides a reusable microbenchmark framework with:
//! - Performance counter integration (Linux only)
//! - Console output with Unicode tables
//! - Explicit report rendering and JSON persistence
//! - Structured, serializable comparison of persisted reports
//! - Portable raw-sample reports for external orchestrators
//! - Advisory or gating policy with terminal, Markdown, and JSON rendering
//! - Multi-suite file and directory validation and comparison
//! - Warm-up and calibration phases
//! - Progress indicators
//! - Generic table formatting

pub mod bench;
mod comparison;
mod context;
mod launcher;
mod policy;
mod render;
mod series;
mod session;
mod suite;
pub mod table;
mod threading;

pub use bench::backend::BenchSampleResult;
pub use bench::{
    BenchContext, BenchmarkCaseOrder, BenchmarkRunner, BenchmarkRuntimeOptions,
    ConcurrentBenchContext, ConcurrentBenchControl, ConcurrentBenchmarkGroup, ConcurrentSampleInfo,
    ConcurrentSampleLifecycle, ConcurrentSamplePhase, ConcurrentWorker, ConcurrentWorkerResult,
    CounterValue, DiagnosticError, DiagnosticResult, EnergyScope, MeasurementBackend,
    MeasurementDomain, MemoryBandwidthScope, MetricFormat, MetricValue, NoContext,
    PmuCounterProfile, PmuScope, Throughput, WallClockBackend,
};
#[cfg(feature = "cuda")]
pub use bench::{CudaError, CudaEvent, CudaEventBackend, CudaResult};
#[cfg(feature = "gpu-counters")]
pub use bench::{
    DEFAULT_NVIDIA_GPU_COUNTERS, GpuCounterCollector, GpuCounterError, GpuCounterMetric,
    GpuCounterResult,
};
#[cfg(target_os = "linux")]
pub use bench::{LinuxPerfBackend, LinuxPerfThreadSet};
pub use comparison::{
    BenchmarkCaseIdentity, COMPARISON_SCHEMA_VERSION, ComparisonCaseIdentity,
    ComparisonCaseSnapshot, ComparisonError, ComparisonOptions, ComparisonReport, ComparisonSide,
    ComparisonStatistics, ComparisonSummary, EnvironmentComparison, EnvironmentOverride,
    MatchedBenchmark, MeasurementDirection, MeasurementKind, MetricComparison,
    NativeMeasurementProjection, PrimaryMeasurement, ReportDocument, ReportDocumentType,
    ReportError, ReportReference, SeriesCaseIdentity, UnmatchedBenchmark, compare_reports,
};
pub use context::{ContextError, ReportContext};
pub use launcher::{
    BASELINE_PATH_ENVIRONMENT, BenchmarkMainOptions, CONTEXT_FILE_ENVIRONMENT,
    OUTPUT_PATH_ENVIRONMENT, benchmark_filter_from_args, benchmark_filter_from_env,
    benchmark_options_with_default_suite, run_benchmark_main,
};
pub use policy::{
    ChangeClassification, ComparisonExitStatus, DEFAULT_MAXIMUM_CV_PERCENT,
    DEFAULT_MAXIMUM_OUTLIER_FRACTION, DEFAULT_MINIMUM_CHANGE_PERCENT, PolicyCaseEvaluation,
    PolicyError, PolicyEvaluation, PolicyFinding, PolicySummary, RegressionPolicy,
};
pub use render::{ANALYSIS_SCHEMA_VERSION, ComparisonAnalysis, render_markdown, render_terminal};
pub use series::{
    SERIES_DOCUMENT_TYPE, SERIES_SCHEMA_VERSION, SeriesReport, SeriesResult, SeriesValidationError,
    Validity, ValidityStatus,
};
pub use suite::{
    ReportInputRole, SUITE_ANALYSIS_SCHEMA_VERSION, SuiteComparisonAnalysis, SuiteComparisonError,
    SuiteComparisonSummary, compare_report_inputs, validate_report_input,
};
pub use table::{Alignment, BorderColor, TableFormatter};

#[cfg(target_os = "linux")]
pub use bench::PerfCounters;
pub use session::{
    BenchmarkKind, BenchmarkReport, BenchmarkResult, BenchmarkStats, ComparisonPolicy,
    MetricSummary, REPORT_SCHEMA_VERSION, SampleMetric, SampleMetricSet, WorkerCounterSummary,
    WorkerSummary,
};

// Re-export key types for convenience
pub use std::hint::black_box;
pub use std::time::Instant;

#[cfg(target_os = "linux")]
pub use perf_event;

#[macro_export]
macro_rules! benchmark_main {
    (|$runner:ident| $body:block) => {
        fn main() {
            let options = $crate::benchmark_options_with_default_suite(
                $crate::BenchmarkMainOptions::default(),
                env!("CARGO_CRATE_NAME"),
            );
            let _ = $crate::run_benchmark_main(options, |$runner| $body);
        }
    };
    ($options:expr, |$runner:ident| $body:block) => {
        fn main() {
            let options =
                $crate::benchmark_options_with_default_suite($options, env!("CARGO_CRATE_NAME"));
            let _ = $crate::run_benchmark_main(options, |$runner| $body);
        }
    };
}
