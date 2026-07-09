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

//! Pluggable measurement backends.
//!
//! # Why a trait
//!
//! Historically `micromeasure` had exactly two measurement paths hard-wired
//! into the runner:
//!
//! 1. On Linux: a perf-event group around the closure, filling CPU PMU
//!    counters (cycles, instructions, branches, cache misses, frontend/backend
//!    stalls) into [`crate::bench::Results`].
//! 2. Off Linux: a wall-clock span with no counters.
//!
//! That works well for CPU microbenchmarks but is misleading for GPU work,
//! as documented in `book/src/gpu-sharp-edges.md`. A GPU benchmark
//! needs CUDA event elapsed time recorded on the device, host-vs-device
//! latency split, and domain-specific metrics (`cuda_event_ms`, `tflops`,
//! `tensor_tflops`, ...) that do not fit the fixed `Results` shape.
//!
//! The [`MeasurementBackend`] trait lets a benchmark plug in a backend that
//! owns the measurement window without forcing CUDA (or any other device
//! API) to become a dependency of normal `micromeasure` builds. The crate
//! ships a wall-clock fallback and an optional
//! [`CudaEventBackend`](crate::CudaEventBackend) behind the `cuda` feature.
//!
//! # Trait shape
//!
//! The trait is object-safe so a runner can hold either a generic
//! `B: MeasurementBackend` or a `Box<dyn MeasurementBackend>`. The runner is
//! responsible for the outer wall-clock span (via `Instant`) and for invoking
//! the bench closure that returns the operation count; the backend is
//! responsible for any domain-specific counters it records around that same
//! window, plus any derived metrics only it can produce.
//!
//! # Integration status
//!
//! The trait is wired into the runner. The runner calls
//! [`begin`](MeasurementBackend::begin) / [`end`](MeasurementBackend::end)
//! / [`collect`](MeasurementBackend::collect) per sample for all standard
//! (single-threaded) benchmarks, replacing the historic
//! `execute_standard_sample` / `execute_timing_only` split.
//!
//! The platform default backend is [`LinuxPerfBackend`](crate::LinuxPerfBackend)
//! on Linux (preserving the historic perf-event group + individual-counter
//! fallback chain) and [`WallClockBackend`] elsewhere.
//!
//! Custom backends are supplied via
//! [`crate::BenchmarkGroup::backend`] and take precedence over the platform
//! default. The concurrent benchmark path does not yet use the trait —
//! it still calls `execute_standard` directly via
//! `execute_concurrent_worker`.

use crate::bench::Results;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// How a [`MetricValue`] should be formatted in the custom metrics table.
///
/// Most metrics are continuous numbers (latency in ms, throughput in
/// TFLOP/s) and render with the adaptive `format_metric_value` helper.
/// Some are categorical or integer-valued (algorithm IDs, workspace sizes
/// in bytes, selected device index) and should never appear in scientific
/// notation or with decimal places.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MetricFormat {
    /// Adaptive numeric formatting: 3 decimal places for values in
    /// `[0.001, 1000)`, scientific notation outside that range. The
    /// default for latency / throughput / rate metrics.
    #[default]
    Number,
    /// Integer formatting: rounds to the nearest integer, no decimal
    /// places, no scientific notation. Use for algorithm IDs, device
    /// indices, kernel counts, and other categorical or count-valued
    /// metrics.
    Integer,
}

/// What a measured operation reports in addition to the standard `Results`.
///
/// This is the per-sample extension point called for by Phase 2 of
/// `book/src/gpu-sharp-edges.md` ("No Per-Sample Custom Metrics").
/// Backends push names and values here as they collect them; the runner
/// aggregates them across samples (mean, median, p95, min, max) once the
/// `MetricValue` aggregation path is wired up.
///
/// Names are `&'static str` so backends can use string literals without
/// allocation, matching [`crate::bench::CounterValue`]. Units are also
/// `&'static str` for the same reason; rendering scales them with the
/// existing `Throughput::format_rate` helper when appropriate.
///
/// ## Display name
///
/// By default the metric `name` is used as the label in the custom metrics
/// table. When the machine-friendly name is not human-friendly (e.g.
/// `gpu_gib_s`), call [`with_display_name`](Self::with_display_name) to
/// set a human-readable label (e.g. `"GPU bandwidth"`). The aggregation
/// key remains the `name` field; the display name is purely cosmetic.
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct MetricValue {
    pub name: &'static str,
    pub value: f64,
    pub unit: &'static str,
    pub section: &'static str,
    /// Cosmetic override for the table label. When empty, `name` is used.
    pub display_name: &'static str,
    /// Controls how `value` is formatted in the table.
    pub format: MetricFormat,
}

impl MetricValue {
    #[allow(dead_code)]
    pub fn new(name: &'static str, value: f64, unit: &'static str) -> Self {
        Self {
            name,
            value,
            unit,
            section: "",
            display_name: "",
            format: MetricFormat::Number,
        }
    }

    /// Set a human-readable display name for table rendering. The
    /// aggregation key remains `name`; this is purely cosmetic.
    pub fn with_display_name(mut self, display_name: &'static str) -> Self {
        self.display_name = display_name;
        self
    }

    pub fn with_section(mut self, section: &'static str) -> Self {
        self.section = section;
        self
    }

    /// Set the formatting hint. See [`MetricFormat`].
    pub fn with_format(mut self, format: MetricFormat) -> Self {
        self.format = format;
        self
    }

    // ---- Derived metric helpers ----
    // These reduce per-bench boilerplate for common GPU metric shapes.

    /// A duration in milliseconds. Equivalent to `new(name, ms, "ms")`.
    pub fn duration_ms(name: &'static str, duration: Duration) -> Self {
        Self::new(name, duration.as_secs_f64() * 1000.0, "ms")
    }

    /// A bandwidth in GiB/s. `bytes` is the number of bytes transferred;
    /// `seconds` is the elapsed time. Computes `bytes / (1024^3 * seconds)`.
    pub fn bandwidth_gib_s(name: &'static str, bytes: u64, seconds: f64) -> Self {
        let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        Self::new(name, gib / seconds.max(f64::EPSILON), "GiB/s")
    }

    /// A throughput in TFLOP/s. `flops` is the number of floating-point
    /// operations; `seconds` is the elapsed time. Computes
    /// `flops / (1e12 * seconds)`.
    pub fn throughput_tflops(name: &'static str, flops: u64, seconds: f64) -> Self {
        let tflops = flops as f64 / (1e12 * seconds.max(f64::EPSILON));
        Self::new(name, tflops, "TFLOP/s")
    }

    /// A count or ID rendered as an integer (no decimal places, no
    /// scientific notation). Use for algorithm IDs, device indices,
    /// kernel counts, etc.
    pub fn integer(name: &'static str, value: i64, unit: &'static str) -> Self {
        Self::new(name, value as f64, unit).with_format(MetricFormat::Integer)
    }
}

#[derive(Clone, Debug, Default)]
pub struct DiagnosticResult {
    pub section: &'static str,
    pub metrics: Vec<MetricValue>,
}

impl DiagnosticResult {
    pub fn new(section: &'static str) -> Self {
        Self {
            section,
            metrics: Vec::new(),
        }
    }

    pub fn push_metric(mut self, metric: MetricValue) -> Self {
        let metric = if metric.section.is_empty() {
            metric.with_section(self.section)
        } else {
            metric
        };
        self.metrics.push(metric);
        self
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DiagnosticError {
    pub message: String,
}

impl DiagnosticError {
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl From<String> for DiagnosticError {
    fn from(message: String) -> Self {
        Self::new(message)
    }
}

impl From<&str> for DiagnosticError {
    fn from(message: &str) -> Self {
        Self::new(message)
    }
}

/// Per-sample result returned by a benchmark function that wants to report
/// custom metrics in addition to the standard timing/throughput/PMU pipeline.
///
/// This is the Phase 2 extension point called for by
/// `book/src/gpu-sharp-edges.md` ("Fixed `fn(&mut T, usize, usize)`
/// Shape Is Awkward For Rich Results" and "No Per-Sample Custom Metrics").
///
/// A bench function with the richer signature
/// `fn(&mut T, usize, usize) -> BenchSampleResult` returns one of these per
/// sample. The runner:
///
/// 1. Reads `operations` and routes it through the existing
///    throughput/latency aggregation (`Results.iterations`).
/// 2. Collects `metrics` per sample and aggregates them into
///    [`crate::session::MetricSummary`] (mean, median, p95, min, max) once
///    all samples are in.
/// 3. Persists the aggregated summaries in `BenchmarkStats.metrics` (JSON
///    via the existing serde derive) and renders them in a `custom metrics:`
///    table beneath the standard stats table.
///
/// ## When to use `bench_sample` vs `bench`
///
/// - Use [`crate::BenchmarkGroup::bench`] for tight CPU loops where the
///   framework-derived numbers (latency, throughput, PMU counters) tell the
///   whole story. Operation count is implicit (`chunk_size` or
///   `BenchContext::operations_per_chunk()`).
/// - Use [`crate::BenchmarkGroup::bench_sample`] when the measured closure
///   knows facts only available after execution: selected cuBLASLt algorithm,
///   CUDA event elapsed time, TFLOP/s derived from a shape-dependent FLOP
///   count, workspace size, validation codes, etc.
///
/// ## Construction
///
/// `BenchSampleResult` is constructible ergonomically:
///
/// ```
/// use micromeasure::bench::backend::{BenchSampleResult, MetricValue};
///
/// let r = BenchSampleResult::operations(1024)
///     .with_metric("cuda_event_ms", 1.23, "ms")
///     .with_metric("tflops", 12.5, "TFLOP/s");
/// assert_eq!(r.operations, 1024);
/// assert_eq!(r.metrics.len(), 2);
/// ```
///
/// or via `From<u64>` for one-liners that report operations only:
///
/// ```
/// use micromeasure::bench::backend::BenchSampleResult;
/// let r: BenchSampleResult = 1024_u64.into();
/// assert_eq!(r.operations, 1024);
/// ```
#[derive(Clone, Debug, Default)]
#[allow(dead_code)]
pub struct BenchSampleResult {
    /// Number of logical operations performed in this sample. Routed into
    /// `Results.iterations` so throughput and latency aggregation work
    /// unchanged. Required: must be > 0 for meaningful throughput/latency
    /// numbers (a zero value will produce `n/a` throughput and `inf`
    /// ns/op, matching the existing fallback behaviour).
    pub operations: u64,
    /// Custom per-sample metrics. Names should be stable across samples of
    /// the same benchmark so the aggregation path can group them. The
    /// runner keys summaries by `(section, name, unit)` so metrics with
    /// different sections or units are treated as distinct.
    pub metrics: Vec<MetricValue>,
}

#[allow(dead_code)]
impl BenchSampleResult {
    pub fn operations(operations: u64) -> Self {
        Self {
            operations,
            metrics: Vec::new(),
        }
    }

    pub fn with_metric(mut self, name: &'static str, value: f64, unit: &'static str) -> Self {
        self.metrics.push(MetricValue::new(name, value, unit));
        self
    }

    /// Push a fully-constructed [`MetricValue`] (with display name,
    /// formatting hint, etc.) onto the metrics list.
    pub fn push_metric(mut self, metric: MetricValue) -> Self {
        self.metrics.push(metric);
        self
    }
}

impl From<u64> for BenchSampleResult {
    fn from(operations: u64) -> Self {
        Self::operations(operations)
    }
}

/// Coarse classification of what a benchmark measures.
///
/// Used by the diagnostics path to suppress or relabel CPU-PMU bottleneck
/// messages when the measured operation is not CPU work (see "CPU PMU
/// Diagnostics Are Misleading For GPU Kernels" in the sharp-edges doc).
///
/// The value is stored on the benchmark/group and flows into
/// [`BenchmarkStats`] via the runner; the trait itself does not Consult it
/// directly, but a backend implementation can use it to control how much
/// PMU data it records and how it labels the result.
///
/// [`BenchmarkStats`]: crate::session::BenchmarkStats
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)]
pub enum MeasurementDomain {
    /// CPU-bound microbenchmark. This is the historical default: full PMU
    /// counters and the existing bottleneck diagnostics apply unchanged.
    #[default]
    Cpu,
    /// GPU-bound work. CPU PMU data is treated as host-orchestration
    /// context, not as the primary bottleneck, and bottleneck diagnostics
    /// derived from CPU PMU counters are suppressed or relabelled.
    Gpu,
    /// Mix of host and device work; diagnostics are emitted but caveated.
    Mixed,
}

/// Pluggable measurement window for one sample of one benchmark.
///
/// A backend owns whatever domain-specific state it needs across a single
/// measurement window (one perf-event group, one pair of CUDA events, one
/// ROCm profiler range, ...). The runner calls [`begin`](Self::begin)
/// immediately before invoking the bench closure and
/// [`end`](Self::end) immediately after it returns the operation count,
/// then calls [`collect`](Self::collect) to materialize whatever it
/// observed into the shared [`Results`] struct and a list of
/// [`MetricValue`]s for any derived per-sample metrics.
///
/// ## Responsibilities
///
/// - The **runner** owns the outer wall-clock span (via `Instant`) and the
///   bench closure invocation. It passes the resulting host elapsed time and
///   operation count into `collect`.
/// - The **backend** owns its own counters and timing sources where
///   applicable (e.g. perf event `time_enabled`/`time_running` for PMU
///   multiplexing, or CUDA event recorded/elapsed timestamps). It writes
///   whatever subset of `Results` fields it actually populated and leaves
///   the rest at their defaults (`false` / `0`).
///
/// ## Object safety
///
/// The trait is object-safe on purpose. Consuming code may use it as
/// `B: MeasurementBackend` for zero-cost dispatch, or as
/// `Box<dyn MeasurementBackend>` when the backend is selected at runtime
/// (e.g. from a feature flag or a CLI argument).
///
/// ## Failure modes
///
/// Backends report failure by leaving `has_*` flags false in `Results` and
/// not pushing metrics. The existing renderer already handles the
/// "timing only" case gracefully when no PMU fields are populated; the same
/// path covers a GPU backend that could not record CUDA events. A
/// dedicated backend-issues channel (analogous to the current
/// `record_perf_issue` global) can be layered on top without changing the
/// trait shape.
///
/// ## Per-sample correlation
///
/// `collect` receives `chunk_index: usize` so a backend can correlate the
/// current sample with state it captured in [`begin`](Self::begin) /
/// [`end`](Self::end) (e.g. sampling the GPU clock at sample N for
/// thermal-state tracking, or skipping CUDA-event recording during the
/// warm-up phase). Backends that have nothing to correlate ignore it.
///
/// ## Example: a CUDA event backend
///
/// This is sketch code showing the shape of a CUDA event backend. For CUDA
/// default-stream benchmarks, enable `micromeasure`'s `cuda` feature and use
/// [`crate::CudaEventBackend`] instead of writing this adapter by hand.
///
/// ```ignore
/// use micromeasure::bench::backend::{MeasurementBackend, MetricValue, Results};
/// use std::time::Duration;
///
/// pub struct CudaEventBackend {
///     start: cuda::Event,
///     stop: cuda::Event,
///     stream: cuda::Stream,
///     elapsed_ms: f64,
/// }
///
/// impl CudaEventBackend {
///     pub fn new() -> Self {
///         let stream = cuda::Stream::new();
///         Self {
///             start: cuda::Event::new(),
///             stop: cuda::Event::new(),
///             stream,
///             elapsed_ms: 0.0,
///         }
///     }
/// }
///
/// impl MeasurementBackend for CudaEventBackend {
///     fn begin(&mut self) {
///         self.start.record(&self.stream);
///     }
///
///     fn end(&mut self) {
///         self.stop.record(&self.stream);
///         self.stop.synchronize();
///         self.elapsed_ms = self.start.elapsed_ms(&self.stop).unwrap_or(0.0);
///     }
///
///     fn collect(
///         &mut self,
///         host_elapsed: Duration,
///         ops: u64,
///         chunk_index: usize,
///         results: &mut Results,
///         metrics: &mut Vec<MetricValue>,
///     ) {
///         results.duration = host_elapsed;
///         results.iterations = ops;
///         results.chunks_executed = 1;
///         // Intentionally leave has_cycles / has_instructions / ... false:
///         // CPU PMU data on this thread is host-orchestration noise for a
///         // GPU benchmark, not the primary bottleneck.
///         metrics.push(MetricValue::new("cuda_event_ms", self.elapsed_ms, "ms"));
///         let overhead_ms =
///             host_elapsed.as_secs_f64() * 1_000.0 - self.elapsed_ms;
///         metrics.push(MetricValue::new(
///             "host_overhead_ms",
///             overhead_ms.max(0.0),
///             "ms",
///         ));
///         // chunk_index would be used here to skip recording during the
///         // warmup phase, or to log per-sample clock samples.
///         let _ = chunk_index;
///     }
///
///     fn measurement_label(&self) -> &'static str {
///         "timing + CUDA events"
///     }
/// }
/// ```
///
/// # Integration status
///
/// The trait is wired into the standard (single-threaded) benchmark path.
/// The runner calls `begin` / `end` / `collect` per sample. The concurrent
/// path does not yet use the trait — it still calls `execute_standard`
/// directly via `execute_concurrent_worker`.
#[allow(dead_code)]
pub trait MeasurementBackend {
    /// Begin the measurement window. Called on the measuring thread
    /// immediately before the bench closure is invoked.
    fn begin(&mut self);

    /// End the measurement window. Called on the measuring thread
    /// immediately after the bench closure returns its operation count.
    fn end(&mut self);

    /// Materialize the most recent measurement window into the shared
    /// [`Results`] struct and any backend-specific derived metrics.
    ///
    /// `host_elapsed` is the wall-clock span measured by the runner around
    /// the closure. Backends that need it for splitting (e.g.
    /// `host_overhead_ms = host - device`) read it here. Backends that own
    /// their own multiplexing-proof timing source (perf event
    /// `time_enabled`/`time_running`) use that source internally rather than
    /// overriding `results.duration`.
    ///
    /// **Duration contract**: backends decide what `results.duration` means.
    /// - The default `LinuxPerfBackend` and `WallClockBackend` set
    ///   `results.duration = host_elapsed` (host wall-clock around the
    ///   closure).
    /// - A GPU backend (e.g. CUDA event) may set `results.duration` to the
    ///   **device event elapsed time** and separately report
    ///   `host_overhead_ms` as a custom metric. In that case the
    ///   throughput/latency stats derived from `results.duration` describe
    ///   the GPU kernel, not the host orchestration — which is usually
    ///   what a GPU benchmark author wants.
    /// - If a backend leaves `results.duration` at `Duration::ZERO`, the
    ///   runner will fall back to `host_elapsed` to avoid divide-by-zero in
    ///   throughput computation.
    ///
    /// `ops` is the operation count returned by the bench closure. Backends
    /// must write it into `results.iterations` so the existing
    /// throughput/latency aggregation paths keep working unchanged.
    ///
    /// `chunk_index` is the zero-based sample index within the current
    /// benchmark run. Backends use it to correlate per-sample state (e.g.
    /// skip CUDA-event recording during warmup, capture per-sample GPU
    /// clock samples, or log per-algorithm switches). Backends that have
    /// nothing to correlate ignore it.
    ///
    /// Backends must set `results.chunks_executed = 1` per sample to match
    /// the existing single-chunk measurement model.
    fn collect(
        &mut self,
        host_elapsed: Duration,
        ops: u64,
        chunk_index: usize,
        results: &mut Results,
        metrics: &mut Vec<MetricValue>,
    );

    /// Short label shown in the "Measurement" row of the stats table, e.g.
    /// `"timing + PMU"` or `"timing + CUDA events"`. Returning an empty
    /// string falls back to the runner default.
    fn measurement_label(&self) -> &'static str {
        ""
    }

    /// Hints the runner whether CPU-PMU bottleneck diagnostics derived from
    /// this backend's `Results` should be emitted. GPU backends that leave
    /// CPU PMU fields empty should return `false`; CPU-PMU backends that
    /// populate `has_cycles`/`has_instructions`/... should return `true`.
    /// The default matches the historical behavior of treating any filled
    /// PMU field as eligible for `diagnose_stats`.
    ///
    /// This is the trait-level hook for the "CPU PMU Diagnostics Are
    /// Misleading For GPU Kernels" sharp edge. The runner is still free to
    /// consult [`MeasurementDomain`] declared on the group for an
    /// additional layer of suppression.
    fn emits_cpu_diagnostics(&self) -> bool {
        true
    }
}

/// Minimal backend that records only wall-clock time around the closure.
///
/// This is the in-tree fallback. It mirrors the current non-Linux
/// `execute_timing_only` path: no PMU fields, no metrics, just duration and
/// iteration count. It is also the shape a CUDA adapter would start from
/// before adding device-event recording.
#[derive(Default)]
#[allow(dead_code)]
pub struct WallClockBackend;

#[allow(dead_code)]
impl WallClockBackend {
    pub fn new() -> Self {
        Self
    }
}

impl MeasurementBackend for WallClockBackend {
    fn begin(&mut self) {}

    fn end(&mut self) {}

    fn collect(
        &mut self,
        host_elapsed: Duration,
        ops: u64,
        _chunk_index: usize,
        results: &mut Results,
        _metrics: &mut Vec<MetricValue>,
    ) {
        results.duration = host_elapsed;
        results.iterations = ops;
        results.chunks_executed = 1;
    }

    fn measurement_label(&self) -> &'static str {
        "timing only"
    }

    fn emits_cpu_diagnostics(&self) -> bool {
        false
    }
}

// LinuxPerfBackend is implemented in `perf.rs` (behind `cfg(target_os =
// "linux")`) and re-exported from `bench.rs`. It preserves the historic
// `run_with_perf_group` / `run_with_individual_counters` fallback chain,
// including `record_perf_issue` and `scale_multiplexed_count`.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wall_clock_backend_records_host_elapsed_and_ops() {
        let mut backend = WallClockBackend::new();
        backend.begin();
        let ops = 42_u64;
        backend.end();

        let mut results = Results::default();
        let mut metrics = Vec::new();
        backend.collect(Duration::from_millis(5), ops, 7, &mut results, &mut metrics);

        assert_eq!(results.duration, Duration::from_millis(5));
        assert_eq!(results.iterations, 42);
        assert_eq!(results.chunks_executed, 1);
        assert!(metrics.is_empty());
        assert!(!results.has_cycles);
        assert_eq!(backend.measurement_label(), "timing only");
        assert!(!backend.emits_cpu_diagnostics());
    }

    #[test]
    fn metric_value_default_is_zero_empty() {
        let m = MetricValue::default();
        assert_eq!(m.value, 0.0);
        assert_eq!(m.name, "");
        assert_eq!(m.unit, "");
    }

    #[test]
    fn diagnostic_result_section_is_default_not_override() {
        let result = DiagnosticResult::new("diagnostic")
            .push_metric(MetricValue::new("defaulted", 1.0, "u"))
            .push_metric(MetricValue::new("explicit", 2.0, "u").with_section("profiler"));

        assert_eq!(result.metrics[0].section, "diagnostic");
        assert_eq!(result.metrics[1].section, "profiler");
    }

    #[test]
    fn measurement_domain_serializes_snake_case() {
        let json = serde_json::to_string(&MeasurementDomain::Gpu).unwrap();
        assert_eq!(json, "\"gpu\"");
        let parsed: MeasurementDomain = serde_json::from_str("\"mixed\"").unwrap();
        assert_eq!(parsed, MeasurementDomain::Mixed);
    }
}
