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

mod affinity;
pub(crate) mod backend;
#[cfg(feature = "cuda")]
mod cuda;
#[cfg(feature = "gpu-counters")]
mod gpu_counters;
mod perf;
mod stats;

use crate::session::BenchmarkSession;
use crate::{
    Alignment, BenchmarkKind, BenchmarkReport, BenchmarkResult, TableFormatter,
    WorkerCounterSummary, WorkerSummary,
};
#[cfg(target_os = "linux")]
use affinity::concurrent_worker_pin_cores;
use affinity::{BenchAffinityGuard, warn_affinity_once};
#[cfg(target_os = "linux")]
use perf::{clear_perf_issues, execute_concurrent_worker, prepare_concurrent_worker_measurement};
use perf::{enforce_pmu_quality, measurement_label, warn_perf_status};
use serde::{Deserialize, Serialize};
use stats::{
    benchmark_stats_from_samples, colorize_section_heading, render_combined_stats_table,
    render_custom_metrics, render_stats_table,
};
use std::time::Instant;
use std::{
    collections::BTreeMap,
    hint::black_box,
    io::{self, IsTerminal, Write},
    rc::Rc,
    sync::{
        Barrier,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
    time::Duration,
};

pub use backend::{
    DiagnosticError, DiagnosticResult, MeasurementBackend, MeasurementDomain, MetricFormat,
    MetricValue, WallClockBackend,
};
#[cfg(feature = "cuda")]
pub use cuda::{CudaError, CudaEvent, CudaEventBackend, CudaResult};
#[cfg(feature = "gpu-counters")]
pub use gpu_counters::{
    DEFAULT_NVIDIA_GPU_COUNTERS, GpuCounterCollector, GpuCounterError, GpuCounterMetric,
    GpuCounterResult,
};
#[cfg(target_os = "linux")]
pub use perf::LinuxPerfBackend;
#[cfg(target_os = "linux")]
pub use perf::PerfCounters;

/// Construct the platform-default [`MeasurementBackend`].
///
/// On Linux this returns a [`LinuxPerfBackend`] (preserving the historic
/// perf-event group + individual-counter fallback chain). On other
/// platforms it returns a [`WallClockBackend`].
///
/// Benchmarks that need a different backend (e.g. a CUDA event adapter)
/// supply a factory via [`BenchmarkGroup::backend`].
fn default_backend() -> Box<dyn MeasurementBackend> {
    #[cfg(target_os = "linux")]
    {
        Box::new(LinuxPerfBackend::new())
    }
    #[cfg(not(target_os = "linux"))]
    {
        Box::new(WallClockBackend::new())
    }
}

const MIN_CHUNK_SIZE: usize = 1;
const MAX_CHUNK_SIZE: usize = 50_000_000;
const TARGET_CHUNK_DURATION: Duration = Duration::from_millis(50);
const DEFAULT_CONCURRENT_SAMPLE_DURATION: Duration = Duration::from_millis(50);
const DEFAULT_WARM_UP_DURATION: Duration = Duration::from_secs(1);
const DEFAULT_BENCHMARK_DURATION: Duration = Duration::from_secs(5);
const MIN_SAMPLES: usize = 20;
const MAX_SAMPLES: usize = 100;

type BenchFunction<T> = fn(&mut T, usize, usize);
type BenchSampleFunction<T> = fn(&mut T, usize, usize) -> crate::bench::backend::BenchSampleResult;
type DiagnosticPassFunction<T> =
    fn(&mut T, usize, usize) -> Result<DiagnosticResult, DiagnosticError>;
type ConcurrentBenchFunction<T> = fn(&T, &ConcurrentBenchControl) -> ConcurrentWorkerResult;
type ConcurrentLifecycleFactory<T> = Rc<dyn Fn() -> Box<dyn ConcurrentSampleLifecycle<T>>>;

#[derive(Clone, Copy)]
pub struct ConcurrentBenchControl {
    deadline: Instant,
    thread_index: usize,
    role_thread_index: usize,
}

impl ConcurrentBenchControl {
    pub fn should_stop(&self) -> bool {
        Instant::now() >= self.deadline
    }

    pub fn thread_index(&self) -> usize {
        self.thread_index
    }

    pub fn role_thread_index(&self) -> usize {
        self.role_thread_index
    }
}

pub trait ConcurrentBenchContext {
    fn prepare(num_threads: usize) -> Self;
}

/// Identifies whether a concurrent lifecycle callback surrounds warm-up or a
/// persisted measurement sample.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConcurrentSamplePhase {
    WarmUp,
    Measurement,
}

/// Stable identity passed to concurrent sample lifecycle callbacks.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ConcurrentSampleInfo {
    pub phase: ConcurrentSamplePhase,
    pub sample_index: usize,
}

/// Setup, quiescence, and shared telemetry around a concurrent sample.
///
/// Both callbacks run on the coordinator thread outside the backend's
/// measurement window. `after_sample` runs only after every worker has joined.
/// Metrics returned for warm-up samples are discarded; measurement metrics are
/// persisted under scenario scope and aggregated normally.
pub trait ConcurrentSampleLifecycle<T> {
    fn before_sample(&mut self, _context: &mut T, _sample: ConcurrentSampleInfo) {}

    fn after_sample(
        &mut self,
        _context: &mut T,
        _sample: ConcurrentSampleInfo,
    ) -> Vec<MetricValue> {
        Vec::new()
    }
}

pub struct ConcurrentWorker<T> {
    pub name: &'static str,
    pub threads: usize,
    pub run: ConcurrentBenchFunction<T>,
}

#[derive(Clone, Debug)]
pub struct CounterValue {
    pub name: &'static str,
    pub value: u64,
}

impl CounterValue {
    pub fn new(name: &'static str, value: u64) -> Self {
        Self { name, value }
    }
}

#[derive(Clone, Debug, Default)]
pub struct ConcurrentWorkerResult {
    pub operations: u64,
    pub counters: Vec<CounterValue>,
}

impl ConcurrentWorkerResult {
    pub fn operations(operations: u64) -> Self {
        Self {
            operations,
            counters: Vec::new(),
        }
    }

    pub fn with_counter(mut self, name: &'static str, value: u64) -> Self {
        self.counters.push(CounterValue::new(name, value));
        self
    }
}

impl From<u64> for ConcurrentWorkerResult {
    fn from(operations: u64) -> Self {
        Self::operations(operations)
    }
}

#[derive(Clone)]
struct WorkerSampleSummary {
    results: Results,
    counters: Vec<CounterValue>,
}

struct ConcurrentSampleResult {
    results: Results,
    worker_summaries: Vec<WorkerSampleSummary>,
}

struct ConcurrentGroupOptions<T> {
    throughput: Throughput,
    measurement_domain: MeasurementDomain,
    backend_factory: Option<Rc<dyn Fn() -> Box<dyn MeasurementBackend>>>,
    lifecycle_factory: Option<ConcurrentLifecycleFactory<T>>,
    metadata: BTreeMap<String, String>,
}

impl<T> Clone for ConcurrentGroupOptions<T> {
    fn clone(&self) -> Self {
        Self {
            throughput: self.throughput.clone(),
            measurement_domain: self.measurement_domain,
            backend_factory: self.backend_factory.clone(),
            lifecycle_factory: self.lifecycle_factory.clone(),
            metadata: self.metadata.clone(),
        }
    }
}

pub(super) struct ConcurrentWorkerMeasurement {
    pub results: Results,
    pub counters: Vec<CounterValue>,
}

#[derive(Clone, Default)]
pub struct Results {
    pub cycles: u64,
    pub instructions: u64,
    pub cache_references: u64,
    pub l1i_misses: u64,
    pub branches: u64,
    pub branch_misses: u64,
    pub cache_misses: u64,
    pub stalled_cycles_frontend: u64,
    pub stalled_cycles_backend: u64,
    pub has_cycles: bool,
    pub has_instructions: bool,
    pub has_cache_references: bool,
    pub has_l1i_misses: bool,
    pub has_branches: bool,
    pub has_branch_misses: bool,
    pub has_cache_misses: bool,
    pub has_stalled_cycles_frontend: bool,
    pub has_stalled_cycles_backend: bool,
    pub pmu_time_enabled_ns: u64,
    pub pmu_time_running_ns: u64,
    pub duration: Duration,
    pub iterations: u64,
    pub chunks_executed: u64,
}

impl Results {
    pub fn add(&mut self, other: &Results) {
        self.cycles += other.cycles;
        self.instructions += other.instructions;
        self.cache_references += other.cache_references;
        self.l1i_misses += other.l1i_misses;
        self.branches += other.branches;
        self.branch_misses += other.branch_misses;
        self.cache_misses += other.cache_misses;
        self.stalled_cycles_frontend += other.stalled_cycles_frontend;
        self.stalled_cycles_backend += other.stalled_cycles_backend;
        self.has_cycles |= other.has_cycles;
        self.has_instructions |= other.has_instructions;
        self.has_cache_references |= other.has_cache_references;
        self.has_l1i_misses |= other.has_l1i_misses;
        self.has_branches |= other.has_branches;
        self.has_branch_misses |= other.has_branch_misses;
        self.has_cache_misses |= other.has_cache_misses;
        self.has_stalled_cycles_frontend |= other.has_stalled_cycles_frontend;
        self.has_stalled_cycles_backend |= other.has_stalled_cycles_backend;
        self.pmu_time_enabled_ns += other.pmu_time_enabled_ns;
        self.pmu_time_running_ns += other.pmu_time_running_ns;
        self.duration += other.duration;
        self.iterations += other.iterations;
        self.chunks_executed += other.chunks_executed;
    }

    pub fn divide(&mut self, divisor: u64) {
        if divisor == 0 {
            return;
        }

        self.cycles /= divisor;
        self.instructions /= divisor;
        self.cache_references /= divisor;
        self.l1i_misses /= divisor;
        self.branches /= divisor;
        self.branch_misses /= divisor;
        self.cache_misses /= divisor;
        self.stalled_cycles_frontend /= divisor;
        self.stalled_cycles_backend /= divisor;
        self.pmu_time_enabled_ns /= divisor;
        self.pmu_time_running_ns /= divisor;
        self.duration /= divisor as u32;
        self.iterations /= divisor;
        self.chunks_executed /= divisor;
    }
}

pub struct BenchmarkConfig {
    pub chunk_size: usize,
    pub target_samples: usize,
    pub estimated_throughput_per_sec: f64,
}

struct ConcurrentBenchmarkConfig {
    sample_duration: Duration,
    target_samples: usize,
    estimated_throughput_per_sec: f64,
}

#[derive(Clone, Debug)]
pub struct BenchmarkRuntimeOptions {
    pub warm_up_duration: Duration,
    pub benchmark_duration: Duration,
    pub min_samples: usize,
    pub max_samples: usize,
}

/// Deterministic ordering policy for a caller-owned list of benchmark cases.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkCaseOrder {
    #[default]
    Declared,
    Randomized {
        seed: u64,
    },
}

impl BenchmarkCaseOrder {
    /// Return indices in the order cases should be executed. Randomization is
    /// deterministic and dependency-free so a seed can be retained as report
    /// metadata and replayed exactly.
    pub fn indices(self, case_count: usize) -> Vec<usize> {
        let mut indices: Vec<usize> = (0..case_count).collect();
        let Self::Randomized { mut seed } = self else {
            return indices;
        };
        for index in (1..indices.len()).rev() {
            seed = seed.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut value = seed;
            value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            value ^= value >> 31;
            indices.swap(index, value as usize % (index + 1));
        }
        indices
    }
}

impl Default for BenchmarkRuntimeOptions {
    fn default() -> Self {
        Self {
            warm_up_duration: DEFAULT_WARM_UP_DURATION,
            benchmark_duration: DEFAULT_BENCHMARK_DURATION,
            min_samples: MIN_SAMPLES,
            max_samples: MAX_SAMPLES,
        }
    }
}

impl BenchmarkRuntimeOptions {
    fn validate(&self) {
        assert!(
            self.warm_up_duration > Duration::ZERO,
            "warm_up_duration must be > 0"
        );
        assert!(
            self.benchmark_duration > Duration::ZERO,
            "benchmark_duration must be > 0"
        );
        assert!(self.min_samples > 0, "min_samples must be > 0");
        assert!(self.max_samples > 0, "max_samples must be > 0");
        assert!(
            self.min_samples <= self.max_samples,
            "min_samples must be <= max_samples"
        );
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct Throughput {
    amount_per_operation: u64,
    unit: String,
}

impl Default for Throughput {
    fn default() -> Self {
        Self::ops()
    }
}

impl Throughput {
    pub fn per_operation(amount_per_operation: u64, unit: impl Into<String>) -> Self {
        let unit = unit.into();
        assert!(
            amount_per_operation > 0,
            "throughput amount per operation must be > 0"
        );
        assert!(!unit.trim().is_empty(), "throughput unit must not be empty");

        Self {
            amount_per_operation,
            unit,
        }
    }

    pub fn bytes(bytes_per_operation: u64) -> Self {
        Self::per_operation(bytes_per_operation, "bytes")
    }

    pub fn ops() -> Self {
        Self::per_operation(1, "ops")
    }

    pub fn amount_per_operation(&self) -> u64 {
        self.amount_per_operation
    }

    pub fn unit(&self) -> &str {
        &self.unit
    }

    pub(crate) fn rate_for_operations(&self, operations: u64, duration_secs: f64) -> f64 {
        safe_ratio_f64(
            operations as f64 * self.amount_per_operation as f64,
            duration_secs,
        )
    }

    pub(crate) fn format_rate(&self, throughput_per_sec: f64) -> String {
        if !throughput_per_sec.is_finite() || throughput_per_sec <= 0.0 {
            return "n/a".to_string();
        }

        let (scaled, prefix) = if throughput_per_sec >= 1_000_000_000.0 {
            (throughput_per_sec / 1_000_000_000.0, "G")
        } else if throughput_per_sec >= 1_000_000.0 {
            (throughput_per_sec / 1_000_000.0, "M")
        } else if throughput_per_sec >= 1_000.0 {
            (throughput_per_sec / 1_000.0, "k")
        } else {
            (throughput_per_sec, "")
        };

        format!("{scaled:.2} {prefix}{}/s", self.unit)
    }
}

/// Generic benchmark context that can hold any preparation data
pub trait BenchContext {
    fn prepare(num_chunks: usize) -> Self;

    fn chunk_size() -> Option<usize> {
        None
    }

    fn operations_per_chunk() -> Option<u64> {
        None
    }
}

pub struct NoContext;

impl BenchContext for NoContext {
    fn prepare(_num_chunks: usize) -> Self {
        NoContext
    }
}

impl ConcurrentBenchContext for NoContext {
    fn prepare(_num_threads: usize) -> Self {
        NoContext
    }
}

fn flush_stdout() {
    let _ = io::stdout().flush();
}

fn rewrite_line(text: &str) {
    print!("\r\x1b[2K{text}");
    flush_stdout();
}

fn clear_line() {
    print!("\r\x1b[2K\r");
    flush_stdout();
}

fn safe_ratio_f64(numerator: f64, denominator: f64) -> f64 {
    if denominator > 0.0 && denominator.is_finite() && numerator.is_finite() {
        numerator / denominator
    } else {
        0.0
    }
}

fn throughput_ops_per_sec(result: &Results) -> Option<f64> {
    let seconds = result.duration.as_secs_f64();
    if seconds <= f64::EPSILON || !seconds.is_finite() || result.iterations == 0 {
        return None;
    }

    Some(result.iterations as f64 / seconds)
}

fn has_perf_counters(results: &Results) -> bool {
    results.has_cycles
        || results.has_instructions
        || results.has_cache_references
        || results.has_l1i_misses
        || results.has_branches
        || results.has_branch_misses
        || results.has_cache_misses
        || results.has_stalled_cycles_frontend
        || results.has_stalled_cycles_backend
}

fn has_full_perf_counters(results: &Results) -> bool {
    results.has_cycles
        && results.has_instructions
        && results.has_cache_references
        && results.has_branches
        && results.has_branch_misses
        && results.has_cache_misses
}

fn measurement_results_from_stats(stats: &crate::BenchmarkStats) -> Results {
    Results {
        has_cycles: stats.has_cycles,
        has_instructions: stats.has_instructions,
        has_cache_references: stats.has_cache_references,
        has_l1i_misses: stats.has_l1i_misses,
        has_branches: stats.has_branches,
        has_branch_misses: stats.has_branch_misses,
        has_cache_misses: stats.has_cache_misses,
        has_stalled_cycles_frontend: stats.has_stalled_cycles_frontend,
        has_stalled_cycles_backend: stats.has_stalled_cycles_backend,
        pmu_time_enabled_ns: stats.pmu_time_enabled_ns,
        pmu_time_running_ns: stats.pmu_time_running_ns,
        ..Results::default()
    }
}

fn update_running_throughput(
    running_throughput: &mut f64,
    result: &Results,
    throughput: &Throughput,
) {
    let sample_throughput =
        throughput.rate_for_operations(result.iterations, result.duration.as_secs_f64());
    if sample_throughput > 0.0 {
        *running_throughput = *running_throughput * 0.9 + sample_throughput * 0.1;
    }
}

fn merge_counter_values(acc: &mut Vec<CounterValue>, new_values: &[CounterValue]) {
    for new_value in new_values {
        if let Some(existing) = acc.iter_mut().find(|value| value.name == new_value.name) {
            existing.value = existing.value.saturating_add(new_value.value);
        } else {
            acc.push(new_value.clone());
        }
    }
    acc.sort_by(|left, right| left.name.cmp(right.name));
}

fn summarize_worker_counters(
    counters: &[CounterValue],
    operations: u64,
    total_duration_sec: f64,
) -> Vec<WorkerCounterSummary> {
    counters
        .iter()
        .map(|counter| WorkerCounterSummary {
            name: counter.name.to_string(),
            total: counter.value,
            per_op: if operations > 0 {
                counter.value as f64 / operations as f64
            } else {
                0.0
            },
            per_sec: if total_duration_sec > 0.0 {
                counter.value as f64 / total_duration_sec
            } else {
                0.0
            },
        })
        .collect()
}

fn render_worker_counters(counters: &[WorkerCounterSummary]) {
    if counters.is_empty() {
        return;
    }

    println!("  bench event counters:");
    let mut table = TableFormatter::new(
        vec!["Event", "Total", "Per Op", "Per Sec"],
        vec![28, 16, 14, 16],
    )
    .with_alignments(vec![
        Alignment::Left,
        Alignment::Right,
        Alignment::Right,
        Alignment::Right,
    ]);

    for counter in counters {
        table.add_row(vec![
            &counter.name,
            &counter.total.to_string(),
            &format_small_rate(counter.per_op),
            &format!("{:.2}", counter.per_sec),
        ]);
    }

    table.print();
}

fn format_small_rate(value: f64) -> String {
    if !value.is_finite() {
        return "n/a".to_string();
    }

    if value == 0.0 {
        return "0".to_string();
    }

    let abs = value.abs();
    if abs >= 0.001 {
        format!("{value:.4}")
    } else {
        format!("{value:.3e}")
    }
}

fn render_result_section(
    title: &str,
    stats: &crate::BenchmarkStats,
    combined: bool,
    border_color: Option<crate::BorderColor>,
) {
    println!("  {}", colorize_section_heading(title));
    let render = if combined {
        render_combined_stats_table
    } else {
        render_stats_table
    };

    if let Some(pmu_byline) = render(
        stats,
        &effective_measurement_label(
            stats,
            has_perf_counters(&measurement_results_from_stats(stats)),
        ),
        border_color,
    ) {
        println!("{pmu_byline}");
    }

    render_diagnostics(stats);
    render_custom_metrics(stats);
}

fn effective_measurement_label(stats: &crate::BenchmarkStats, has_perf: bool) -> String {
    if !stats.measurement_label.is_empty() {
        stats.measurement_label.clone()
    } else {
        measurement_label(has_perf).to_string()
    }
}

fn render_standard_results(name: &str, stats: &crate::BenchmarkStats) {
    let results = measurement_results_from_stats(stats);
    let has_perf = has_perf_counters(&results);

    // For GPU-domain benchmarks, no CPU PMU is expected — the backend
    // intentionally leaves has_* false. Suppress the "PMU unavailable"
    // warning in that case; it is only relevant for CPU-domain benchmarks
    // that expected PMU but didn't get it.
    let suppress_perf_warning = stats.measurement_domain != MeasurementDomain::Cpu;
    if !suppress_perf_warning {
        warn_perf_status(has_perf, has_full_perf_counters(&results));
    }
    enforce_pmu_quality(name, has_perf, &results);

    let label = effective_measurement_label(stats, has_perf);
    println!("  results:");
    if let Some(pmu_byline) = render_stats_table(stats, &label, None) {
        println!("{pmu_byline}");
    }
    render_diagnostics(stats);
    render_custom_metrics(stats);
}

fn render_concurrent_results(
    name: &str,
    combined_stats: &crate::BenchmarkStats,
    worker_summaries: &[WorkerSummary],
) {
    let results = measurement_results_from_stats(combined_stats);
    let has_perf = has_perf_counters(&results);
    let suppress_perf_warning = combined_stats.measurement_domain != MeasurementDomain::Cpu;
    if !suppress_perf_warning {
        warn_perf_status(has_perf, has_full_perf_counters(&results));
    }
    enforce_pmu_quality(name, has_perf, &results);

    println!("  results:");
    for worker_summary in worker_summaries {
        render_result_section(
            &format!(
                "worker: {} ({} threads)",
                worker_summary.name, worker_summary.threads
            ),
            &worker_summary.stats,
            false,
            Some(crate::BorderColor::Cyan),
        );
        render_worker_counters(&worker_summary.counters);
        println!();
    }
    render_result_section("workers combined", combined_stats, true, None);
}

fn render_diagnostics(stats: &crate::BenchmarkStats) {
    let diagnostics = diagnose_stats(stats);
    if diagnostics.is_empty() {
        return;
    }

    println!("  possible bottlenecks:");
    for diagnostic in diagnostics {
        println!("    - {}", colorize_problem(&diagnostic));
    }
}

fn colorize_problem(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }

    format!("\x1b[31m{text}\x1b[0m")
}

/// Emit CPU-PMU-derived bottleneck diagnostics for a benchmark.
///
/// Domain rules (see `book/src/gpu-sharp-edges.md`):
/// - `Cpu`: full historic diagnostics, unchanged.
/// - `Gpu`: skip all CPU-PMU bottleneck diagnostics. The host thread's
///   counters describe launch/sync orchestration, not the measured GPU
///   kernel, so emitting them as the primary bottleneck would mislead.
///   Run-stability warnings (CV, outliers) are kept because sample
///   stability matters for any benchmark.
/// - `Mixed`: emit each CPU-PMU diagnostic with a `[host]` prefix so the
///   reader knows the signal is host-side context, not the primary
///   bottleneck.
///
/// Additionally, when [`crate::MeasurementBackend::emits_cpu_diagnostics`]
/// returned `false` (captured in
/// [`crate::session::BenchmarkStats::emits_cpu_diagnostics`]), CPU-PMU
/// diagnostics are suppressed regardless of domain. This lets a custom
/// backend like a CUDA event adapter opt out of CPU-PMU bottleneck
/// messages even for `Mixed` workloads where some host work is expected.
fn diagnose_stats(stats: &crate::BenchmarkStats) -> Vec<String> {
    let mut diagnostics = Vec::new();

    let host_prefix = match stats.measurement_domain {
        MeasurementDomain::Cpu => "",
        MeasurementDomain::Gpu => "",
        MeasurementDomain::Io => "[host] ",
        MeasurementDomain::Mixed => "[host] ",
    };

    // Suppress CPU-PMU diagnostics when the backend declared it does not
    // emit them (e.g. a CUDA event backend) OR the measurement domain is
    // Gpu. Mixed domain keeps diagnostics (with [host] prefix) unless the
    // backend explicitly opts out via emits_cpu_diagnostics.
    let suppress_cpu_pmu = !stats.emits_cpu_diagnostics
        || matches!(
            stats.measurement_domain,
            MeasurementDomain::Gpu | MeasurementDomain::Io
        );

    if !suppress_cpu_pmu
        && stats.has_cycles
        && stats.has_stalled_cycles_backend
        && stats.backend_stall_percent >= 20.0
        && stats.has_cache_misses
        && ((stats.has_cache_references && stats.cache_miss_percent >= 5.0)
            || stats.cache_misses_per_op >= 0.05)
    {
        diagnostics.push(format!(
            "{host_prefix}Likely data-side memory latency: backend stall is {:.1}% with cache pressure at {:.2}% miss rate and {:.4} misses/op",
            stats.backend_stall_percent,
            stats.cache_miss_percent,
            stats.cache_misses_per_op
        ));
    }

    if !suppress_cpu_pmu
        && stats.has_cycles
        && stats.has_stalled_cycles_frontend
        && stats.frontend_stall_percent >= 20.0
        && stats.has_branches
        && stats.has_branch_misses
        && stats.branch_miss_rate >= 3.0
    {
        diagnostics.push(format!(
            "{host_prefix}Likely branch predictor or fetch disruption: frontend stall is {:.1}% with {:.2}% branch misses",
            stats.frontend_stall_percent,
            stats.branch_miss_rate
        ));
    }

    if !suppress_cpu_pmu
        && stats.has_cycles
        && stats.has_stalled_cycles_frontend
        && stats.frontend_stall_percent >= 20.0
        && stats.has_l1i_misses
        && stats.l1i_misses_per_op >= 0.01
    {
        diagnostics.push(format!(
            "{host_prefix}Likely instruction-cache pressure: frontend stall is {:.1}% with {:.4} L1I misses/op",
            stats.frontend_stall_percent, stats.l1i_misses_per_op
        ));
    }

    if !suppress_cpu_pmu
        && stats.has_cycles
        && stats.has_instructions
        && stats.ipc > 0.0
        && stats.ipc < 1.0
        && diagnostics.is_empty()
    {
        diagnostics.push(format!(
            "{host_prefix}Low IPC ({:.3}) suggests poor machine utilization; look for dependency chains, execution-port pressure, or latent memory effects",
            stats.ipc
        ));
    }

    // Run-stability warnings apply to every domain: GPU benchmarks also
    // need stable samples, and a noisy GPU run is still worth flagging
    // (thermal throttling, algorithm selection changes, queue contention).
    if stats.cv_percent >= 10.0 || stats.outlier_count >= (stats.samples / 10).max(2) {
        diagnostics.push(format!(
            "Run stability is weak: CV {:.2}% with {} outliers across {} samples",
            stats.cv_percent, stats.outlier_count, stats.samples
        ));
    }

    diagnostics
}

fn calibrate_engine<T: BenchContext, F: Fn() -> T + ?Sized, G: Fn(&mut T, usize, usize)>(
    f: G,
    factory: &F,
    throughput: &Throughput,
    runtime: &BenchmarkRuntimeOptions,
    backend: &mut dyn MeasurementBackend,
) -> BenchmarkConfig {
    rewrite_line("🔥 calibrating benchmark");

    if let Some(preferred_chunk_size) = T::chunk_size() {
        assert!(preferred_chunk_size > 0, "chunk_size must be > 0");
        let warm_up_end = Instant::now() + runtime.warm_up_duration;
        let mut warm_up_count = 0;
        let mut measured_warm_up_duration = Duration::ZERO;
        let mut last_progress = Instant::now();
        while Instant::now() < warm_up_end {
            let mut prepared = factory();
            backend.begin();
            let started = Instant::now();
            black_box(|| f(&mut prepared, preferred_chunk_size, warm_up_count))();
            let host_elapsed = started.elapsed();
            backend.end();

            let mut results = Results::default();
            let mut metrics = Vec::new();
            let operations = T::operations_per_chunk().unwrap_or(preferred_chunk_size as u64);
            backend.collect(
                host_elapsed,
                operations,
                warm_up_count,
                &mut results,
                &mut metrics,
            );
            measured_warm_up_duration += if results.duration > Duration::ZERO {
                results.duration
            } else {
                host_elapsed
            };
            warm_up_count += 1;

            // Throttle progress updates to avoid spamming the terminal
            // when the chunk size is small and the work loop is very
            // fast (e.g. GPU benchmarks with fixed chunk_size()).
            if last_progress.elapsed() >= Duration::from_millis(50) {
                let remaining_ms = warm_up_end
                    .saturating_duration_since(Instant::now())
                    .as_millis();
                rewrite_line(&format!(
                    "🔥 calibrating benchmark  warmup remaining: {remaining_ms:>4} ms  chunk: {preferred_chunk_size}"
                ));
                last_progress = Instant::now();
            }
        }

        let estimated_chunk_duration_secs =
            measured_warm_up_duration.as_secs_f64() / warm_up_count as f64;
        let estimated_throughput_per_sec = throughput
            .rate_for_operations(preferred_chunk_size as u64, estimated_chunk_duration_secs);
        let target_samples = target_sample_count(runtime, estimated_chunk_duration_secs);

        clear_line();
        return BenchmarkConfig {
            chunk_size: preferred_chunk_size,
            target_samples,
            estimated_throughput_per_sec,
        };
    }

    let mut chunk_size = MIN_CHUNK_SIZE;
    let mut best_chunk_size = chunk_size;
    let mut estimated_throughput_per_sec = 0.0;

    for i in 0..15 {
        let mut prepared = factory();
        let started = Instant::now();
        black_box(|| f(&mut prepared, chunk_size, 0))();
        let duration = started.elapsed();
        let duration_secs = duration.as_secs_f64();

        if duration_secs >= 0.0001 {
            estimated_throughput_per_sec =
                throughput.rate_for_operations(chunk_size as u64, duration_secs);
            if duration >= TARGET_CHUNK_DURATION.mul_f64(0.8)
                && duration <= TARGET_CHUNK_DURATION.mul_f64(1.2)
            {
                best_chunk_size = chunk_size;
                break;
            }

            let scaling_factor = TARGET_CHUNK_DURATION.as_secs_f64() / duration_secs;
            let next_chunk_size = ((chunk_size as f64) * scaling_factor)
                .round()
                .clamp(MIN_CHUNK_SIZE as f64, MAX_CHUNK_SIZE as f64)
                as usize;
            best_chunk_size = next_chunk_size;
            if next_chunk_size == chunk_size {
                break;
            }
            chunk_size = next_chunk_size;
        } else {
            let next_chunk_size = chunk_size.saturating_mul(10).min(MAX_CHUNK_SIZE);
            best_chunk_size = next_chunk_size;
            if next_chunk_size == chunk_size {
                break;
            }
            chunk_size = next_chunk_size;
        }

        rewrite_line(&format!(
            "🔥 calibrating benchmark  pass: {:>2}/15  chunk: {:>9}  est: {}",
            i + 1,
            chunk_size,
            throughput.format_rate(estimated_throughput_per_sec)
        ));
    }

    let warm_up_end = Instant::now() + runtime.warm_up_duration;
    let mut warm_up_count = 0;
    let mut last_progress = Instant::now();
    while Instant::now() < warm_up_end {
        let mut prepared = factory();
        black_box(|| f(&mut prepared, best_chunk_size, warm_up_count))();
        warm_up_count += 1;

        if last_progress.elapsed() >= Duration::from_millis(50) {
            let remaining_ms = warm_up_end
                .saturating_duration_since(Instant::now())
                .as_millis();
            rewrite_line(&format!(
                "🔥 calibrating benchmark  warmup remaining: {remaining_ms:>4} ms  chunk: {best_chunk_size:>9}"
            ));
            last_progress = Instant::now();
        }
    }

    let estimated_chunk_duration_secs = if estimated_throughput_per_sec > 0.0 {
        let chunk_throughput_amount =
            best_chunk_size as f64 * throughput.amount_per_operation as f64;
        chunk_throughput_amount / estimated_throughput_per_sec
    } else {
        TARGET_CHUNK_DURATION.as_secs_f64()
    };
    let target_samples = target_sample_count(runtime, estimated_chunk_duration_secs);

    clear_line();
    BenchmarkConfig {
        chunk_size: best_chunk_size,
        target_samples,
        estimated_throughput_per_sec,
    }
}

fn target_sample_count(
    runtime: &BenchmarkRuntimeOptions,
    estimated_sample_duration_secs: f64,
) -> usize {
    if !estimated_sample_duration_secs.is_finite() || estimated_sample_duration_secs <= 0.0 {
        return runtime.max_samples;
    }

    ((runtime.benchmark_duration.as_secs_f64() / estimated_sample_duration_secs) as usize)
        .clamp(runtime.min_samples, runtime.max_samples)
}

fn total_worker_threads<T>(workers: &[ConcurrentWorker<T>]) -> usize {
    workers.iter().map(|worker| worker.threads).sum()
}

fn warm_up_concurrent_engine<T: ConcurrentBenchContext + Sync, F: Fn(usize) -> T + ?Sized>(
    sample_duration: Duration,
    workers: &[ConcurrentWorker<T>],
    factory: &F,
    throughput: &Throughput,
    runtime: &BenchmarkRuntimeOptions,
    lifecycle: &mut Option<Box<dyn ConcurrentSampleLifecycle<T>>>,
) -> ConcurrentBenchmarkConfig {
    rewrite_line("🔥 calibrating benchmark");

    let total_threads = total_worker_threads(workers);
    let warm_up_end = Instant::now() + runtime.warm_up_duration;
    let mut estimated_throughput_per_sec = 0.0;

    let mut warm_up_index = 0usize;
    while Instant::now() < warm_up_end {
        let mut prepared = factory(total_threads);
        let sample = ConcurrentSampleInfo {
            phase: ConcurrentSamplePhase::WarmUp,
            sample_index: warm_up_index,
        };
        if let Some(lifecycle) = lifecycle.as_deref_mut() {
            lifecycle.before_sample(&mut prepared, sample);
        }
        let result = execute_concurrent_timing_only(&prepared, sample_duration, workers);
        if let Some(lifecycle) = lifecycle.as_deref_mut() {
            let _ = lifecycle.after_sample(&mut prepared, sample);
        }
        warm_up_index += 1;
        let sample_throughput = throughput.rate_for_operations(
            result.results.iterations,
            result.results.duration.as_secs_f64(),
        );
        if sample_throughput > 0.0 {
            estimated_throughput_per_sec = if estimated_throughput_per_sec > 0.0 {
                estimated_throughput_per_sec * 0.8 + sample_throughput * 0.2
            } else {
                sample_throughput
            };
        }
        let remaining_ms = warm_up_end
            .saturating_duration_since(Instant::now())
            .as_millis();
        rewrite_line(&format!(
            "🔥 calibrating benchmark  warmup remaining: {remaining_ms:>4} ms  sample: {:>4} ms  est: {}",
            sample_duration.as_millis(),
            throughput.format_rate(estimated_throughput_per_sec)
        ));
    }

    clear_line();
    let target_samples = target_sample_count(runtime, sample_duration.as_secs_f64());

    ConcurrentBenchmarkConfig {
        sample_duration,
        target_samples,
        estimated_throughput_per_sec,
    }
}

#[cfg(not(target_os = "linux"))]
fn execute_timing_only(run: impl FnOnce() -> u64) -> Results {
    let start_time = Instant::now();
    let iterations = run();
    Results {
        duration: start_time.elapsed(),
        iterations,
        chunks_executed: 1,
        ..Results::default()
    }
}

fn execute_concurrent_timing_only_measurement(
    run: impl FnOnce() -> ConcurrentWorkerResult,
) -> ConcurrentWorkerMeasurement {
    let start_time = Instant::now();
    let worker_result = run();
    ConcurrentWorkerMeasurement {
        results: Results {
            duration: start_time.elapsed(),
            iterations: worker_result.operations,
            chunks_executed: 1,
            ..Results::default()
        },
        counters: worker_result.counters,
    }
}

/// Sample execution path for `bench(...)` benchmarks. The bench function
/// returns no metrics; any metrics pushed by the backend via
/// [`MeasurementBackend::collect`] are **dropped** here intentionally.
///
/// To capture backend-pushed metrics (e.g. `cuda_event_ms` from a CUDA
/// event backend), use [`execute_standard_sample_with_metrics`] instead,
/// which is the execution path behind `bench_sample(...)`.
fn execute_standard_sample<T: BenchContext>(
    f: &BenchFunction<T>,
    prepared: &mut T,
    chunk_size: usize,
    chunk_num: usize,
    ops: u64,
    backend: &mut dyn MeasurementBackend,
) -> Results {
    backend.begin();
    let start = Instant::now();
    black_box(|| f(prepared, chunk_size, chunk_num))();
    let host_elapsed = start.elapsed();
    backend.end();

    let mut results = Results::default();
    let mut metrics = Vec::new();
    backend.collect(host_elapsed, ops, chunk_num, &mut results, &mut metrics);
    // Intentionally dropped: bench() does not capture per-sample custom
    // metrics. Use bench_sample() to capture both bench-function metrics
    // (from BenchSampleResult) and backend-pushed metrics.
    drop(metrics);
    results
}

/// Sample execution path for `bench_sample` benchmarks whose function
/// returns [`crate::BenchSampleResult`] per sample, carrying both the
/// operation count and any custom metrics. Same PMU/timing measurement as
/// [`execute_standard_sample`] but the operation count and any custom
/// metrics come from the closure's return value instead of
/// `BenchContext::operations_per_chunk()`.
///
/// Backends that push their own metrics via
/// [`MeasurementBackend::collect`] (e.g. a CUDA event backend pushing
/// `cuda_event_ms` and `host_overhead_ms`) append to the same `metrics`
/// Vec that the bench function already populated.
fn execute_standard_sample_with_metrics<T: BenchContext>(
    f: &BenchSampleFunction<T>,
    prepared: &mut T,
    chunk_size: usize,
    chunk_num: usize,
    backend: &mut dyn MeasurementBackend,
) -> (Results, Vec<crate::bench::backend::MetricValue>) {
    backend.begin();
    let start = Instant::now();
    let sample_result = black_box(|| f(prepared, chunk_size, chunk_num))();
    let host_elapsed = start.elapsed();
    backend.end();

    let mut results = Results::default();
    let mut metrics = sample_result.metrics;
    backend.collect(
        host_elapsed,
        sample_result.operations,
        chunk_num,
        &mut results,
        &mut metrics,
    );
    (results, metrics)
}

fn execute_diagnostic_pass<T: BenchContext, F: Fn() -> T + ?Sized>(
    diagnostic_pass: Option<DiagnosticPassFunction<T>>,
    factory: &F,
    chunk_size: usize,
    diagnostic_samples: usize,
) -> Vec<Vec<MetricValue>> {
    let Some(diagnostic_pass) = diagnostic_pass else {
        return Vec::new();
    };

    let mut all_metrics = Vec::new();
    for sample in 0..diagnostic_samples {
        let mut prepared = factory();
        match black_box(|| diagnostic_pass(&mut prepared, chunk_size, sample))() {
            Ok(result) => {
                let metrics = result
                    .metrics
                    .into_iter()
                    .map(|metric| {
                        if metric.section.is_empty() && !result.section.is_empty() {
                            metric.with_section(result.section)
                        } else {
                            metric
                        }
                    })
                    .collect();
                all_metrics.push(metrics);
            }
            Err(_) => {
                all_metrics.push(vec![
                    MetricValue::integer("diagnostic_pass_failed", 1, "errors")
                        .with_display_name("Diagnostic pass failed")
                        .with_section("diagnostic"),
                ]);
                break;
            }
        }
    }
    all_metrics
}

fn execute_concurrent_timing_only<T: ConcurrentBenchContext + Sync>(
    prepared: &T,
    sample_duration: Duration,
    workers: &[ConcurrentWorker<T>],
) -> ConcurrentSampleResult {
    let mut backend = None;
    execute_concurrent_sample_inner(
        prepared,
        sample_duration,
        workers,
        #[cfg(target_os = "linux")]
        &[],
        false,
        &mut backend,
    )
}

fn execute_concurrent_sample<T: ConcurrentBenchContext + Sync>(
    prepared: &T,
    sample_duration: Duration,
    workers: &[ConcurrentWorker<T>],
    #[cfg(target_os = "linux")] pin_cores: &[usize],
    backend: &mut Option<Box<dyn MeasurementBackend>>,
) -> ConcurrentSampleResult {
    execute_concurrent_sample_inner(
        prepared,
        sample_duration,
        workers,
        #[cfg(target_os = "linux")]
        pin_cores,
        true,
        backend,
    )
}

fn execute_concurrent_sample_inner<T: ConcurrentBenchContext + Sync>(
    prepared: &T,
    sample_duration: Duration,
    workers: &[ConcurrentWorker<T>],
    #[cfg(target_os = "linux")] pin_cores: &[usize],
    use_perf_counters: bool,
    backend: &mut Option<Box<dyn MeasurementBackend>>,
) -> ConcurrentSampleResult {
    let total_threads = total_worker_threads(workers);
    let ready_barrier = Barrier::new(total_threads + 1);
    let measurement_ready_barrier = Barrier::new(total_threads + 1);
    let start_barrier = Barrier::new(total_threads + 1);
    let start_instant = std::sync::OnceLock::new();

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(total_threads);
        let ready_barrier = &ready_barrier;
        let measurement_ready_barrier = &measurement_ready_barrier;
        let start_barrier = &start_barrier;
        let start_instant = &start_instant;

        let mut next_thread_index = 0usize;
        for (worker_index, worker) in workers.iter().enumerate() {
            for role_thread_index in 0..worker.threads {
                let run = worker.run;
                let thread_index = next_thread_index;
                next_thread_index += 1;

                handles.push(scope.spawn(move || {
                    #[cfg(target_os = "linux")]
                    if use_perf_counters
                        && let Some(core_id) = pin_cores.get(thread_index).copied()
                        && let Err(error) = crate::threading::pin_current_thread_to_core(core_id)
                    {
                        warn_affinity_once(format!(
                            "Could not pin concurrent benchmark worker {thread_index} to core {core_id}: {error}. Continuing without worker pinning"
                        ));
                    }

                    #[cfg(target_os = "linux")]
                    let mut perf_measurement =
                        use_perf_counters.then(prepare_concurrent_worker_measurement);

                    ready_barrier.wait();
                    #[cfg(target_os = "linux")]
                    if let Some(perf_measurement) = perf_measurement.as_mut() {
                        perf_measurement.begin_prepared();
                    }
                    measurement_ready_barrier.wait();
                    start_barrier.wait();

                    let benchmark_start = *start_instant.get().expect("missing benchmark start");
                    let control = ConcurrentBenchControl {
                        deadline: benchmark_start + sample_duration,
                        thread_index,
                        role_thread_index,
                    };

                    let worker_result = if use_perf_counters {
                        #[cfg(target_os = "linux")]
                        {
                            execute_concurrent_worker(
                                perf_measurement
                                    .as_mut()
                                    .expect("missing prepared perf measurement"),
                                prepared,
                                &control,
                                run,
                            )
                        }
                        #[cfg(not(target_os = "linux"))]
                        {
                            execute_concurrent_timing_only_measurement(|| {
                                black_box(|| run(prepared, &control))()
                            })
                        }
                    } else {
                        execute_concurrent_timing_only_measurement(|| {
                            black_box(|| run(prepared, &control))()
                        })
                    };

                    (worker_index, worker_result)
                }));
            }
        }

        ready_barrier.wait();
        measurement_ready_barrier.wait();
        if let Some(backend) = backend.as_deref_mut() {
            backend.begin();
        }
        let benchmark_start = Instant::now();
        let _ = start_instant.set(benchmark_start);
        start_barrier.wait();

        let mut aggregate = Results::default();
        let mut worker_results = vec![Results::default(); workers.len()];
        let mut worker_counters = vec![Vec::<CounterValue>::new(); workers.len()];
        for handle in handles {
            let (worker_index, worker_result) =
                handle.join().expect("concurrent benchmark worker panicked");
            worker_results[worker_index].add(&worker_result.results);
            merge_counter_values(&mut worker_counters[worker_index], &worker_result.counters);
            aggregate.add(&worker_result.results);
        }
        let wall_duration = benchmark_start.elapsed();
        if let Some(backend) = backend.as_deref_mut() {
            backend.end();
        }

        aggregate.duration = wall_duration;
        aggregate.chunks_executed = 1;
        for worker_result in &mut worker_results {
            worker_result.duration = wall_duration;
            worker_result.chunks_executed = 1;
        }

        ConcurrentSampleResult {
            results: aggregate,
            worker_summaries: worker_results
                .into_iter()
                .zip(worker_counters)
                .map(|(results, counters)| WorkerSampleSummary { results, counters })
                .collect(),
        }
    })
}

/// A concurrent backend owns the scenario timing window while worker-local
/// PMU measurements remain the source of host-orchestration counters. Replace
/// only fields the backend explicitly populated.
fn apply_concurrent_backend_results(results: &mut Results, backend: Results) {
    if backend.duration > Duration::ZERO {
        results.duration = backend.duration;
    }
    if backend.iterations > 0 {
        results.iterations = backend.iterations;
    }
    if backend.chunks_executed > 0 {
        results.chunks_executed = backend.chunks_executed;
    }

    macro_rules! replace_counter {
        ($has:ident, $value:ident) => {
            if backend.$has {
                results.$has = true;
                results.$value = backend.$value;
            }
        };
    }
    replace_counter!(has_cycles, cycles);
    replace_counter!(has_instructions, instructions);
    replace_counter!(has_cache_references, cache_references);
    replace_counter!(has_l1i_misses, l1i_misses);
    replace_counter!(has_branches, branches);
    replace_counter!(has_branch_misses, branch_misses);
    replace_counter!(has_cache_misses, cache_misses);
    replace_counter!(has_stalled_cycles_frontend, stalled_cycles_frontend);
    replace_counter!(has_stalled_cycles_backend, stalled_cycles_backend);
    if backend.pmu_time_enabled_ns > 0 {
        results.pmu_time_enabled_ns = backend.pmu_time_enabled_ns;
    }
    if backend.pmu_time_running_ns > 0 {
        results.pmu_time_running_ns = backend.pmu_time_running_ns;
    }
}

fn update_progress_bar(
    current: usize,
    total: usize,
    current_throughput: f64,
    throughput: &Throughput,
) {
    let width = 40;
    let filled = (current * width / total.max(1)).min(width);
    let empty = width - filled;
    let percentage = (current * 100 / total.max(1)).min(100);

    print!("\r\x1b[2K⚡ running [");
    for i in 0..filled {
        if i == filled - 1 && current < total {
            print!(">");
        } else {
            print!("=");
        }
    }
    for _ in 0..empty {
        print!(" ");
    }

    let throughput_display = if current_throughput.is_finite() && current_throughput > 0.0 {
        throughput.format_rate(current_throughput)
    } else {
        "Calculating...".to_string()
    };

    print!("] {percentage}% ({current}/{total}) {throughput_display}");
    flush_stdout();
}

pub struct BenchmarkRunner {
    session: std::sync::Arc<BenchmarkSession>,
    filter: Option<String>,
    runtime: BenchmarkRuntimeOptions,
    case_cooldown: Duration,
    cases_started: AtomicUsize,
}

impl Default for BenchmarkRunner {
    fn default() -> Self {
        Self::new()
    }
}

impl BenchmarkRunner {
    pub fn new() -> Self {
        Self {
            session: std::sync::Arc::new(BenchmarkSession::new()),
            filter: None,
            runtime: BenchmarkRuntimeOptions::default(),
            case_cooldown: Duration::ZERO,
            cases_started: AtomicUsize::new(0),
        }
    }

    pub fn with_suite(mut self, suite: impl Into<String>) -> Self {
        self.session = std::sync::Arc::new(BenchmarkSession::new_with_suite(suite));
        self
    }

    pub fn with_filter(mut self, filter: Option<&str>) -> Self {
        self.filter = filter.map(str::to_string);
        self
    }

    pub fn with_runtime(mut self, runtime: BenchmarkRuntimeOptions) -> Self {
        self.set_runtime(runtime);
        self
    }

    /// Wait between benchmark cases, outside all warm-up and measurement
    /// windows. Useful for device quiescence or thermal cooldown.
    pub fn with_case_cooldown(mut self, cooldown: Duration) -> Self {
        self.set_case_cooldown(cooldown);
        self
    }

    /// Mutable counterpart to [`Self::with_case_cooldown`], suitable for the
    /// `&mut BenchmarkRunner` supplied by [`crate::benchmark_main!`].
    pub fn set_case_cooldown(&mut self, cooldown: Duration) -> &mut Self {
        self.case_cooldown = cooldown;
        self
    }

    /// Apply a deterministic ordering policy to a caller-owned case list.
    pub fn ordered_case_indices(&self, case_count: usize, order: BenchmarkCaseOrder) -> Vec<usize> {
        order.indices(case_count)
    }

    fn begin_case(&self) {
        if self.cases_started.fetch_add(1, Ordering::Relaxed) > 0
            && self.case_cooldown > Duration::ZERO
        {
            thread::sleep(self.case_cooldown);
        }
    }

    pub fn set_runtime(&mut self, runtime: BenchmarkRuntimeOptions) -> &mut Self {
        runtime.validate();
        self.runtime = runtime;
        self
    }

    pub fn group<T: BenchContext>(&self, name: &'static str, f: impl FnOnce(&BenchmarkGroup<T>)) {
        let group = BenchmarkGroup {
            runner: self,
            name,
            throughput: Throughput::ops(),
            measurement_domain: MeasurementDomain::default(),
            backend_factory: None,
            diagnostic_pass: None,
            diagnostic_samples: 1,
            _marker: std::marker::PhantomData,
        };
        f(&group);
    }

    pub fn concurrent_group<T: ConcurrentBenchContext + Send + Sync>(
        &self,
        name: &'static str,
        f: impl FnOnce(&ConcurrentBenchmarkGroup<T>),
    ) {
        let group = ConcurrentBenchmarkGroup {
            runner: self,
            name,
            options: ConcurrentGroupOptions {
                throughput: Throughput::ops(),
                measurement_domain: MeasurementDomain::default(),
                backend_factory: None,
                lifecycle_factory: None,
                metadata: BTreeMap::new(),
            },
        };
        f(&group);
    }

    pub fn report(&self) -> BenchmarkReport {
        self.session.report()
    }

    pub fn results(&self) -> Vec<BenchmarkResult> {
        self.session.get_results()
    }

    fn should_run(&self, name: &str, group: &str) -> bool {
        let Some(filter) = &self.filter else {
            return true;
        };
        filter == "all" || name.contains(filter) || group.contains(filter) || filter == group
    }

    pub fn run<T: BenchContext>(&self, name: &str, group: &str, f: BenchFunction<T>) {
        self.run_with_factory_and_throughput(
            name,
            group,
            f,
            &|| T::prepare(T::chunk_size().unwrap_or(MIN_CHUNK_SIZE)),
            Throughput::ops(),
            MeasurementDomain::default(),
            default_backend(),
            None,
            1,
        );
    }

    pub fn run_with_factory<T: BenchContext, F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        group: &str,
        f: BenchFunction<T>,
        factory: &F,
    ) {
        self.run_with_factory_and_throughput(
            name,
            group,
            f,
            factory,
            Throughput::ops(),
            MeasurementDomain::default(),
            default_backend(),
            None,
            1,
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_with_factory_and_throughput<T: BenchContext, F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        group: &str,
        f: BenchFunction<T>,
        factory: &F,
        throughput: Throughput,
        measurement_domain: MeasurementDomain,
        mut backend: Box<dyn MeasurementBackend>,
        diagnostic_pass: Option<DiagnosticPassFunction<T>>,
        diagnostic_samples: usize,
    ) {
        if !self.should_run(name, group) {
            return;
        }
        self.begin_case();

        #[cfg(target_os = "linux")]
        clear_perf_issues();

        let _affinity_guard = BenchAffinityGuard::acquire();
        println!("\nBenchmark: {name}");

        let config = calibrate_engine(f, factory, &throughput, &self.runtime, backend.as_mut());
        println!(
            "  calibrated: chunk={} samples={} estimate={}",
            config.chunk_size,
            config.target_samples,
            throughput.format_rate(config.estimated_throughput_per_sec),
        );

        rewrite_line(&format!("⚡ running 0/{} samples", config.target_samples));
        let mut all_results = Vec::with_capacity(config.target_samples);
        let mut summed_results = Results::default();
        let mut running_throughput = config.estimated_throughput_per_sec;
        let mut all_metrics: Vec<Vec<MetricValue>> = vec![Vec::new(); config.target_samples];

        for sample in 0..config.target_samples {
            let mut prepared = factory();
            let ops = T::operations_per_chunk().unwrap_or(config.chunk_size as u64);
            let sample_result = execute_standard_sample(
                &f,
                &mut prepared,
                config.chunk_size,
                sample,
                ops,
                backend.as_mut(),
            );

            update_running_throughput(&mut running_throughput, &sample_result, &throughput);
            summed_results.add(&sample_result);
            all_results.push(sample_result);

            if sample % 2 == 0 || sample == config.target_samples - 1 {
                update_progress_bar(
                    sample + 1,
                    config.target_samples,
                    running_throughput,
                    &throughput,
                );
            }
        }

        clear_line();
        println!("  samples complete: {}", config.target_samples);

        let diagnostic_metrics = execute_diagnostic_pass(
            diagnostic_pass,
            factory,
            config.chunk_size,
            diagnostic_samples,
        );
        all_metrics.extend(diagnostic_metrics);

        let stats = benchmark_stats_from_samples(
            &summed_results,
            &all_results,
            config.target_samples,
            &throughput,
            measurement_domain,
            backend.measurement_label(),
            backend.emits_cpu_diagnostics(),
            &all_metrics,
        );
        render_standard_results(name, &stats);

        self.session.add_result(BenchmarkResult {
            name: name.to_string(),
            group: group.to_string(),
            kind: BenchmarkKind::Standard,
            execution_index: 0,
            stats,
            worker_summaries: Vec::new(),
            metadata: BTreeMap::new(),
        });
    }

    /// Variant of [`run_with_factory_and_throughput`](Self::run_with_factory_and_throughput)
    /// for benches that return a [`crate::BenchSampleResult`] per sample,
    /// carrying both operation count and custom metrics (e.g.
    /// `cuda_event_ms`, `tflops`, `host_overhead_ms`).
    ///
    /// This is the Phase 2 entry point of
    /// `book/src/gpu-sharp-edges.md` ("No Per-Sample Custom
    /// Metrics"). It accepts a `BenchSampleFunction<T>` rather than the
    /// plain `BenchFunction<T>`, threads each sample's returned metrics
    /// through the same PMU/timing pipeline, aggregates them into
    /// [`crate::session::MetricSummary`] (mean, median, p95, min, max) and
    /// renders a `custom metrics:` table beneath the standard stats table.
    ///
    /// Prefer the plain `run_with_factory_and_throughput` for tight CPU
    /// loops where the framework-derived numbers tell the whole story; the
    /// richer path is intended for GPU work and other cases where the
    /// measured closure knows facts only available after execution.
    #[allow(clippy::too_many_arguments)]
    pub fn run_sample_with_factory_and_throughput<T: BenchContext, F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        group: &str,
        f: BenchSampleFunction<T>,
        factory: &F,
        throughput: Throughput,
        measurement_domain: MeasurementDomain,
        mut backend: Box<dyn MeasurementBackend>,
        diagnostic_pass: Option<DiagnosticPassFunction<T>>,
        diagnostic_samples: usize,
    ) {
        if !self.should_run(name, group) {
            return;
        }
        self.begin_case();

        #[cfg(target_os = "linux")]
        clear_perf_issues();

        let _affinity_guard = BenchAffinityGuard::acquire();
        println!("\nBenchmark: {name}");

        // For calibration we wrap `f` so its `BenchSampleResult` return is
        // discarded; calibration only needs to time the work, not collect
        // metrics. Using a non-capturing closure here would require a
        // function-pointer wrapper, which is awkward; the generalised
        // `calibrate_engine` signature accepts `impl Fn(...)` so this
        // closure works directly.
        let calibrate_fn = |ctx: &mut T, cs: usize, cn: usize| {
            let _ = f(ctx, cs, cn);
        };
        let config = calibrate_engine(
            calibrate_fn,
            factory,
            &throughput,
            &self.runtime,
            backend.as_mut(),
        );
        println!(
            "  calibrated: chunk={} samples={} estimate={}",
            config.chunk_size,
            config.target_samples,
            throughput.format_rate(config.estimated_throughput_per_sec),
        );

        rewrite_line(&format!("⚡ running 0/{} samples", config.target_samples));
        let mut all_results = Vec::with_capacity(config.target_samples);
        let mut summed_results = Results::default();
        let mut running_throughput = config.estimated_throughput_per_sec;
        let mut all_metrics: Vec<Vec<crate::bench::backend::MetricValue>> =
            Vec::with_capacity(config.target_samples);

        for sample in 0..config.target_samples {
            let mut prepared = factory();
            let (sample_result, sample_metrics) = execute_standard_sample_with_metrics(
                &f,
                &mut prepared,
                config.chunk_size,
                sample,
                backend.as_mut(),
            );

            update_running_throughput(&mut running_throughput, &sample_result, &throughput);
            summed_results.add(&sample_result);
            all_results.push(sample_result);
            all_metrics.push(sample_metrics);

            if sample % 2 == 0 || sample == config.target_samples - 1 {
                update_progress_bar(
                    sample + 1,
                    config.target_samples,
                    running_throughput,
                    &throughput,
                );
            }
        }

        clear_line();
        println!("  samples complete: {}", config.target_samples);

        let diagnostic_metrics = execute_diagnostic_pass(
            diagnostic_pass,
            factory,
            config.chunk_size,
            diagnostic_samples,
        );
        all_metrics.extend(diagnostic_metrics);

        let stats = benchmark_stats_from_samples(
            &summed_results,
            &all_results,
            config.target_samples,
            &throughput,
            measurement_domain,
            backend.measurement_label(),
            backend.emits_cpu_diagnostics(),
            &all_metrics,
        );
        render_standard_results(name, &stats);

        self.session.add_result(BenchmarkResult {
            name: name.to_string(),
            group: group.to_string(),
            kind: BenchmarkKind::Standard,
            execution_index: 0,
            stats,
            worker_summaries: Vec::new(),
            metadata: BTreeMap::new(),
        });
    }

    pub fn run_concurrent<T: ConcurrentBenchContext + Send + Sync>(
        &self,
        name: &str,
        group: &str,
        sample_duration: Duration,
        workers: &[ConcurrentWorker<T>],
    ) {
        self.run_concurrent_with_factory_and_throughput(
            name,
            group,
            sample_duration,
            workers,
            &|num_threads| T::prepare(num_threads),
            Throughput::ops(),
            MeasurementDomain::default(),
        );
    }

    pub fn run_concurrent_with_factory<
        T: ConcurrentBenchContext + Send + Sync,
        F: Fn(usize) -> T + ?Sized,
    >(
        &self,
        name: &str,
        group: &str,
        sample_duration: Duration,
        workers: &[ConcurrentWorker<T>],
        factory: &F,
    ) {
        self.run_concurrent_with_factory_and_throughput(
            name,
            group,
            sample_duration,
            workers,
            factory,
            Throughput::ops(),
            MeasurementDomain::default(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    pub fn run_concurrent_with_factory_and_throughput<
        T: ConcurrentBenchContext + Send + Sync,
        F: Fn(usize) -> T + ?Sized,
    >(
        &self,
        name: &str,
        group: &str,
        sample_duration: Duration,
        workers: &[ConcurrentWorker<T>],
        factory: &F,
        throughput: Throughput,
        measurement_domain: MeasurementDomain,
    ) {
        self.run_concurrent_with_options(
            name,
            group,
            sample_duration,
            workers,
            factory,
            throughput,
            measurement_domain,
            None,
            None,
            BTreeMap::new(),
        );
    }

    #[allow(clippy::too_many_arguments)]
    fn run_concurrent_with_options<
        T: ConcurrentBenchContext + Send + Sync,
        F: Fn(usize) -> T + ?Sized,
    >(
        &self,
        name: &str,
        group: &str,
        sample_duration: Duration,
        workers: &[ConcurrentWorker<T>],
        factory: &F,
        throughput: Throughput,
        measurement_domain: MeasurementDomain,
        backend_factory: Option<&Rc<dyn Fn() -> Box<dyn MeasurementBackend>>>,
        lifecycle_factory: Option<&ConcurrentLifecycleFactory<T>>,
        metadata: BTreeMap<String, String>,
    ) {
        if !self.should_run(name, group) {
            return;
        }
        self.begin_case();
        assert!(
            sample_duration > Duration::ZERO,
            "concurrent benchmark sample_duration must be > 0"
        );
        assert!(
            !workers.is_empty() && total_worker_threads(workers) > 0,
            "concurrent benchmark requires at least one worker thread"
        );

        #[cfg(target_os = "linux")]
        clear_perf_issues();

        println!("\nBenchmark: {name}");

        let mut lifecycle = lifecycle_factory.map(|factory| factory());
        let mut backend = backend_factory.map(|factory| factory());

        let config = warm_up_concurrent_engine(
            sample_duration,
            workers,
            factory,
            &throughput,
            &self.runtime,
            &mut lifecycle,
        );
        println!(
            "  calibrated: sample={}ms samples={} estimate={}",
            config.sample_duration.as_millis(),
            config.target_samples,
            throughput.format_rate(config.estimated_throughput_per_sec),
        );

        rewrite_line(&format!("⚡ running 0/{} samples", config.target_samples));

        let total_threads = total_worker_threads(workers);
        #[cfg(target_os = "linux")]
        let pin_cores = concurrent_worker_pin_cores(total_threads);
        #[cfg(target_os = "linux")]
        if pin_cores.len() < total_threads {
            warn_affinity_once(format!(
                "Concurrent benchmark requested {total_threads} worker threads but only {} distinct candidate cores were available for pinning; PMU multiplexing may still occur",
                pin_cores.len()
            ));
        }

        let mut all_results = Vec::with_capacity(config.target_samples);
        let mut summed_results = Results::default();
        let mut summed_worker_results = vec![Results::default(); workers.len()];
        let mut summed_worker_counters = vec![Vec::<CounterValue>::new(); workers.len()];
        let mut all_worker_results = vec![Vec::with_capacity(config.target_samples); workers.len()];
        let mut running_throughput = config.estimated_throughput_per_sec;
        let mut all_metrics = Vec::with_capacity(config.target_samples);

        for sample in 0..config.target_samples {
            let mut prepared = factory(total_threads);
            let sample_info = ConcurrentSampleInfo {
                phase: ConcurrentSamplePhase::Measurement,
                sample_index: sample,
            };
            if let Some(lifecycle) = lifecycle.as_deref_mut() {
                lifecycle.before_sample(&mut prepared, sample_info);
            }
            let mut sample_result = execute_concurrent_sample(
                &prepared,
                config.sample_duration,
                workers,
                #[cfg(target_os = "linux")]
                &pin_cores,
                &mut backend,
            );

            let mut sample_metrics = Vec::new();
            if let Some(backend) = backend.as_deref_mut() {
                let host_elapsed = sample_result.results.duration;
                let operations = sample_result.results.iterations;
                let mut backend_results = Results::default();
                backend.collect(
                    host_elapsed,
                    operations,
                    sample,
                    &mut backend_results,
                    &mut sample_metrics,
                );
                apply_concurrent_backend_results(&mut sample_result.results, backend_results);
            }
            if let Some(lifecycle) = lifecycle.as_deref_mut() {
                sample_metrics.extend(lifecycle.after_sample(&mut prepared, sample_info));
            }
            for metric in &mut sample_metrics {
                if metric.section.is_empty() {
                    metric.section = "scenario";
                }
            }
            all_metrics.push(sample_metrics);

            update_running_throughput(&mut running_throughput, &sample_result.results, &throughput);
            for (index, worker_summary) in sample_result.worker_summaries.iter().enumerate() {
                summed_worker_results[index].add(&worker_summary.results);
                merge_counter_values(&mut summed_worker_counters[index], &worker_summary.counters);
                all_worker_results[index].push(worker_summary.results.clone());
            }

            summed_results.add(&sample_result.results);
            all_results.push(sample_result.results);

            if sample % 2 == 0 || sample == config.target_samples - 1 {
                update_progress_bar(
                    sample + 1,
                    config.target_samples,
                    running_throughput,
                    &throughput,
                );
            }
        }

        clear_line();
        println!("  samples complete: {}", config.target_samples);

        let stats = benchmark_stats_from_samples(
            &summed_results,
            &all_results,
            config.target_samples,
            &throughput,
            measurement_domain,
            backend
                .as_deref()
                .map(MeasurementBackend::measurement_label)
                .unwrap_or(""),
            backend
                .as_deref()
                .map(MeasurementBackend::emits_cpu_diagnostics)
                .unwrap_or(true),
            &all_metrics,
        );
        let worker_summaries: Vec<WorkerSummary> = workers
            .iter()
            .zip(summed_worker_results.iter())
            .zip(all_worker_results.iter())
            .zip(summed_worker_counters.iter())
            .map(
                |(((worker, worker_results), worker_samples), worker_counters)| {
                    let stats = benchmark_stats_from_samples(
                        worker_results,
                        worker_samples,
                        config.target_samples,
                        &throughput,
                        measurement_domain,
                        "",
                        true,
                        &[],
                    );
                    WorkerSummary {
                        name: worker.name.to_string(),
                        threads: worker.threads,
                        counters: summarize_worker_counters(
                            worker_counters,
                            worker_results.iterations,
                            stats.total_duration_sec,
                        ),
                        stats,
                    }
                },
            )
            .collect();

        render_concurrent_results(name, &stats, &worker_summaries);

        self.session.add_result(BenchmarkResult {
            name: name.to_string(),
            group: group.to_string(),
            kind: BenchmarkKind::Concurrent,
            execution_index: 0,
            stats,
            worker_summaries,
            metadata,
        });
    }
}

pub struct BenchmarkGroup<'a, T: BenchContext> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    throughput: Throughput,
    measurement_domain: MeasurementDomain,
    backend_factory: Option<Rc<dyn Fn() -> Box<dyn MeasurementBackend>>>,
    diagnostic_pass: Option<DiagnosticPassFunction<T>>,
    diagnostic_samples: usize,
    _marker: std::marker::PhantomData<T>,
}

pub struct BenchmarkGroupWithFactory<'a, 'f, T: BenchContext, F: Fn() -> T + ?Sized> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    throughput: Throughput,
    measurement_domain: MeasurementDomain,
    backend_factory: Option<Rc<dyn Fn() -> Box<dyn MeasurementBackend>>>,
    diagnostic_pass: Option<DiagnosticPassFunction<T>>,
    diagnostic_samples: usize,
    factory: &'f F,
}

impl<'a, T: BenchContext> BenchmarkGroup<'a, T> {
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput,
            measurement_domain: self.measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples: self.diagnostic_samples,
            _marker: std::marker::PhantomData,
        }
    }

    /// Declare what this group measures. The runner consults this to
    /// suppress or relabel CPU-PMU bottleneck diagnostics when the measured
    /// operation is not CPU work (see `book/src/gpu-sharp-edges.md`).
    /// Defaults to [`MeasurementDomain::Cpu`], preserving historical
    /// behavior.
    pub fn measurement_domain(&self, measurement_domain: MeasurementDomain) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples: self.diagnostic_samples,
            _marker: std::marker::PhantomData,
        }
    }

    /// Supply a custom [`MeasurementBackend`] factory for this group.
    ///
    /// The factory is called once per benchmark to create a fresh backend
    /// instance, which is then reused across all samples via
    /// [`MeasurementBackend::begin`] / [`MeasurementBackend::end`] /
    /// [`MeasurementBackend::collect`].
    ///
    /// When not set, the runner uses the platform default
    /// ([`LinuxPerfBackend`] on Linux, [`WallClockBackend`] elsewhere).
    ///
    /// ```
    /// use micromeasure::{MeasurementBackend, MeasurementDomain, Throughput, benchmark_main};
    ///
    /// struct MyBackend;
    /// impl MeasurementBackend for MyBackend {
    ///     fn begin(&mut self) {}
    ///     fn end(&mut self) {}
    ///     fn collect(
    ///         &mut self,
    ///         host_elapsed: std::time::Duration,
    ///         ops: u64,
    ///         _chunk_index: usize,
    ///         results: &mut micromeasure::bench::Results,
    ///         _metrics: &mut Vec<micromeasure::MetricValue>,
    ///     ) {
    ///         results.duration = host_elapsed;
    ///         results.iterations = ops;
    ///         results.chunks_executed = 1;
    ///     }
    /// }
    /// ```
    pub fn backend<F: Fn() -> Box<dyn MeasurementBackend> + 'static>(
        &self,
        backend_factory: F,
    ) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain: self.measurement_domain,
            backend_factory: Some(Rc::new(backend_factory)),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples: self.diagnostic_samples,
            _marker: std::marker::PhantomData,
        }
    }

    /// Supply a diagnostic replay pass for this group.
    ///
    /// The runner executes this once after the normal timing samples finish,
    /// using the calibrated chunk size. Metrics returned by this pass are
    /// aggregated and rendered with the normal custom metrics, but they do not
    /// contribute to latency, throughput, or sample stability statistics.
    pub fn diagnostic_pass(&self, diagnostic_pass: DiagnosticPassFunction<T>) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain: self.measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: Some(diagnostic_pass),
            diagnostic_samples: self.diagnostic_samples,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn diagnostic_samples(&self, diagnostic_samples: usize) -> Self {
        assert!(diagnostic_samples > 0, "diagnostic_samples must be > 0");
        Self {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain: self.measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn factory<'f, F: Fn() -> T + ?Sized>(
        &self,
        factory: &'f F,
    ) -> BenchmarkGroupWithFactory<'a, 'f, T, F> {
        BenchmarkGroupWithFactory {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain: self.measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples: self.diagnostic_samples,
            factory,
        }
    }

    pub fn with_factory<'f, F: Fn() -> T + ?Sized>(
        &self,
        factory: &'f F,
    ) -> BenchmarkGroupWithFactory<'a, 'f, T, F> {
        self.factory(factory)
    }

    fn make_backend(&self) -> Box<dyn MeasurementBackend> {
        match &self.backend_factory {
            Some(factory) => factory(),
            None => default_backend(),
        }
    }

    pub fn bench(&self, name: &str, f: BenchFunction<T>) {
        self.runner.run_with_factory_and_throughput(
            name,
            self.name,
            f,
            &|| T::prepare(T::chunk_size().unwrap_or(MIN_CHUNK_SIZE)),
            self.throughput.clone(),
            self.measurement_domain,
            self.make_backend(),
            self.diagnostic_pass,
            self.diagnostic_samples,
        );
    }

    /// Like [`bench`](Self::bench) but accepts a richer bench function that
    /// returns a [`crate::BenchSampleResult`] per sample, carrying both the
    /// operation count and any custom metrics (e.g. `cuda_event_ms`,
    /// `tflops`, `host_overhead_ms`). The runner aggregates the metrics
    /// across samples and renders a `custom metrics:` table beneath the
    /// standard stats table.
    ///
    /// Use this when the measured closure knows facts only available after
    /// execution — selected algorithm ID, device event elapsed time,
    /// scenario-dependent bytes/flops, validation codes. For tight CPU
    /// loops where the framework-derived numbers tell the whole story,
    /// prefer `bench`.
    pub fn bench_sample(&self, name: &str, f: BenchSampleFunction<T>) {
        self.runner.run_sample_with_factory_and_throughput(
            name,
            self.name,
            f,
            &|| T::prepare(T::chunk_size().unwrap_or(MIN_CHUNK_SIZE)),
            self.throughput.clone(),
            self.measurement_domain,
            self.make_backend(),
            self.diagnostic_pass,
            self.diagnostic_samples,
        );
    }

    #[deprecated(note = "use g.throughput(...).bench(...) instead")]
    pub fn bench_with_throughput(&self, name: &str, throughput: Throughput, f: BenchFunction<T>) {
        self.throughput(throughput).bench(name, f);
    }

    #[deprecated(note = "use g.factory(...).bench(...) instead")]
    pub fn bench_with_factory<F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        factory: &F,
        f: BenchFunction<T>,
    ) {
        self.factory(factory).bench(name, f);
    }

    #[deprecated(note = "use g.throughput(...).factory(...).bench(...) instead")]
    pub fn bench_with_factory_and_throughput<F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        factory: &F,
        throughput: Throughput,
        f: BenchFunction<T>,
    ) {
        self.throughput(throughput).factory(factory).bench(name, f);
    }
}

impl<'a, 'f, T: BenchContext, F: Fn() -> T + ?Sized> BenchmarkGroupWithFactory<'a, 'f, T, F> {
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput,
            measurement_domain: self.measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples: self.diagnostic_samples,
            factory: self.factory,
        }
    }

    /// Declare what this group measures. See
    /// [`BenchmarkGroup::measurement_domain`].
    pub fn measurement_domain(&self, measurement_domain: MeasurementDomain) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples: self.diagnostic_samples,
            factory: self.factory,
        }
    }

    /// Supply a custom [`MeasurementBackend`] factory for this group. See
    /// [`BenchmarkGroup::backend`].
    pub fn backend<B: Fn() -> Box<dyn MeasurementBackend> + 'static>(
        &self,
        backend_factory: B,
    ) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain: self.measurement_domain,
            backend_factory: Some(Rc::new(backend_factory)),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples: self.diagnostic_samples,
            factory: self.factory,
        }
    }

    /// Supply a diagnostic replay pass for this group.
    ///
    /// The runner executes this once after the normal timing samples finish,
    /// using the calibrated chunk size. Metrics returned by this pass are
    /// aggregated and rendered with the normal custom metrics, but they do not
    /// contribute to latency, throughput, or sample stability statistics.
    pub fn diagnostic_pass(&self, diagnostic_pass: DiagnosticPassFunction<T>) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain: self.measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: Some(diagnostic_pass),
            diagnostic_samples: self.diagnostic_samples,
            factory: self.factory,
        }
    }

    pub fn diagnostic_samples(&self, diagnostic_samples: usize) -> Self {
        assert!(diagnostic_samples > 0, "diagnostic_samples must be > 0");
        Self {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
            measurement_domain: self.measurement_domain,
            backend_factory: self.backend_factory.clone(),
            diagnostic_pass: self.diagnostic_pass,
            diagnostic_samples,
            factory: self.factory,
        }
    }

    fn make_backend(&self) -> Box<dyn MeasurementBackend> {
        match &self.backend_factory {
            Some(factory) => factory(),
            None => default_backend(),
        }
    }

    pub fn bench(&self, name: &str, f: BenchFunction<T>) {
        self.runner.run_with_factory_and_throughput(
            name,
            self.name,
            f,
            self.factory,
            self.throughput.clone(),
            self.measurement_domain,
            self.make_backend(),
            self.diagnostic_pass,
            self.diagnostic_samples,
        );
    }

    /// Like [`bench`](Self::bench) but accepts a richer bench function that
    /// returns a [`crate::BenchSampleResult`] per sample. See
    /// [`BenchmarkGroup::bench_sample`] for when to use this instead of
    /// `bench`.
    pub fn bench_sample(&self, name: &str, f: BenchSampleFunction<T>) {
        self.runner.run_sample_with_factory_and_throughput(
            name,
            self.name,
            f,
            self.factory,
            self.throughput.clone(),
            self.measurement_domain,
            self.make_backend(),
            self.diagnostic_pass,
            self.diagnostic_samples,
        );
    }
}

pub struct ConcurrentBenchmarkGroup<'a, T: ConcurrentBenchContext> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    options: ConcurrentGroupOptions<T>,
}

pub struct ConcurrentBenchmarkGroupWithDuration<'a, T: ConcurrentBenchContext> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    options: ConcurrentGroupOptions<T>,
    sample_duration: Duration,
}

pub struct ConcurrentBenchmarkGroupWithFactory<
    'a,
    'f,
    T: ConcurrentBenchContext,
    F: Fn(usize) -> T + ?Sized,
> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    options: ConcurrentGroupOptions<T>,
    sample_duration: Duration,
    factory: &'f F,
}

impl<T> ConcurrentGroupOptions<T> {
    fn with_throughput(&self, throughput: Throughput) -> Self {
        let mut options = self.clone();
        options.throughput = throughput;
        options
    }

    fn with_domain(&self, measurement_domain: MeasurementDomain) -> Self {
        let mut options = self.clone();
        options.measurement_domain = measurement_domain;
        options
    }

    fn with_backend<B>(&self, backend_factory: B) -> Self
    where
        B: Fn() -> Box<dyn MeasurementBackend> + 'static,
    {
        let mut options = self.clone();
        options.backend_factory = Some(Rc::new(backend_factory));
        options
    }

    fn with_lifecycle<L, P>(&self, lifecycle_factory: P) -> Self
    where
        L: ConcurrentSampleLifecycle<T> + 'static,
        P: Fn() -> L + 'static,
    {
        let mut options = self.clone();
        options.lifecycle_factory = Some(Rc::new(move || Box::new(lifecycle_factory())));
        options
    }

    fn with_metadata(&self, key: impl Into<String>, value: impl Into<String>) -> Self {
        let mut options = self.clone();
        options.metadata.insert(key.into(), value.into());
        options
    }
}

impl<'a, T: ConcurrentBenchContext + Send + Sync> ConcurrentBenchmarkGroup<'a, T> {
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_throughput(throughput),
        }
    }

    pub fn measurement_domain(&self, measurement_domain: MeasurementDomain) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_domain(measurement_domain),
        }
    }

    /// Measure the coordinated worker window with a custom backend.
    pub fn backend<B>(&self, backend_factory: B) -> Self
    where
        B: Fn() -> Box<dyn MeasurementBackend> + 'static,
    {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_backend(backend_factory),
        }
    }

    /// Install setup/quiescence callbacks and scenario-scoped metrics.
    pub fn lifecycle<L, P>(&self, lifecycle_factory: P) -> Self
    where
        L: ConcurrentSampleLifecycle<T> + 'static,
        P: Fn() -> L + 'static,
    {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_lifecycle(lifecycle_factory),
        }
    }

    /// Attach benchmark-defined environment or execution context to JSON.
    pub fn metadata(&self, key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_metadata(key, value),
        }
    }

    pub fn sample_duration(
        &self,
        sample_duration: Duration,
    ) -> ConcurrentBenchmarkGroupWithDuration<'a, T> {
        assert!(
            sample_duration > Duration::ZERO,
            "concurrent benchmark sample_duration must be > 0"
        );
        ConcurrentBenchmarkGroupWithDuration {
            runner: self.runner,
            name: self.name,
            options: self.options.clone(),
            sample_duration,
        }
    }

    pub fn bench(&self, name: &str, sample_duration: Duration, workers: &[ConcurrentWorker<T>]) {
        self.sample_duration(sample_duration).bench(name, workers);
    }

    #[deprecated(note = "use g.sample_duration(...).throughput(...).bench(...) instead")]
    pub fn bench_with_throughput(
        &self,
        name: &str,
        sample_duration: Duration,
        workers: &[ConcurrentWorker<T>],
        throughput: Throughput,
    ) {
        self.sample_duration(sample_duration)
            .throughput(throughput)
            .bench(name, workers);
    }

    pub fn factory<'f, F: Fn(usize) -> T + ?Sized>(
        &self,
        factory: &'f F,
    ) -> ConcurrentBenchmarkGroupWithFactory<'a, 'f, T, F> {
        ConcurrentBenchmarkGroupWithFactory {
            runner: self.runner,
            name: self.name,
            options: self.options.clone(),
            sample_duration: DEFAULT_CONCURRENT_SAMPLE_DURATION,
            factory,
        }
    }

    pub fn with_factory<'f, F: Fn(usize) -> T + ?Sized>(
        &self,
        factory: &'f F,
    ) -> ConcurrentBenchmarkGroupWithFactory<'a, 'f, T, F> {
        self.factory(factory)
    }

    #[deprecated(note = "use g.sample_duration(...).factory(...).bench(...) instead")]
    pub fn bench_with_factory<F: Fn(usize) -> T + ?Sized>(
        &self,
        name: &str,
        sample_duration: Duration,
        workers: &[ConcurrentWorker<T>],
        factory: &F,
    ) {
        self.sample_duration(sample_duration)
            .factory(factory)
            .bench(name, workers);
    }

    #[deprecated(
        note = "use g.sample_duration(...).throughput(...).factory(...).bench(...) instead"
    )]
    pub fn bench_with_factory_and_throughput<F: Fn(usize) -> T + ?Sized>(
        &self,
        name: &str,
        sample_duration: Duration,
        workers: &[ConcurrentWorker<T>],
        factory: &F,
        throughput: Throughput,
    ) {
        self.sample_duration(sample_duration)
            .throughput(throughput)
            .factory(factory)
            .bench(name, workers);
    }
}

impl<'a, T: ConcurrentBenchContext + Send + Sync> ConcurrentBenchmarkGroupWithDuration<'a, T> {
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_throughput(throughput),
            sample_duration: self.sample_duration,
        }
    }

    pub fn measurement_domain(&self, measurement_domain: MeasurementDomain) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_domain(measurement_domain),
            sample_duration: self.sample_duration,
        }
    }

    pub fn backend<B>(&self, backend_factory: B) -> Self
    where
        B: Fn() -> Box<dyn MeasurementBackend> + 'static,
    {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_backend(backend_factory),
            sample_duration: self.sample_duration,
        }
    }

    pub fn lifecycle<L, P>(&self, lifecycle_factory: P) -> Self
    where
        L: ConcurrentSampleLifecycle<T> + 'static,
        P: Fn() -> L + 'static,
    {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_lifecycle(lifecycle_factory),
            sample_duration: self.sample_duration,
        }
    }

    pub fn metadata(&self, key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_metadata(key, value),
            sample_duration: self.sample_duration,
        }
    }

    pub fn factory<'f, F: Fn(usize) -> T + ?Sized>(
        &self,
        factory: &'f F,
    ) -> ConcurrentBenchmarkGroupWithFactory<'a, 'f, T, F> {
        ConcurrentBenchmarkGroupWithFactory {
            runner: self.runner,
            name: self.name,
            options: self.options.clone(),
            sample_duration: self.sample_duration,
            factory,
        }
    }

    pub fn with_factory<'f, F: Fn(usize) -> T + ?Sized>(
        &self,
        factory: &'f F,
    ) -> ConcurrentBenchmarkGroupWithFactory<'a, 'f, T, F> {
        self.factory(factory)
    }

    pub fn bench(&self, name: &str, workers: &[ConcurrentWorker<T>]) {
        self.runner.run_concurrent_with_options(
            name,
            self.name,
            self.sample_duration,
            workers,
            &|num_threads| T::prepare(num_threads),
            self.options.throughput.clone(),
            self.options.measurement_domain,
            self.options.backend_factory.as_ref(),
            self.options.lifecycle_factory.as_ref(),
            self.options.metadata.clone(),
        );
    }
}

impl<'a, 'f, T, F> ConcurrentBenchmarkGroupWithFactory<'a, 'f, T, F>
where
    T: ConcurrentBenchContext + Send + Sync,
    F: Fn(usize) -> T + ?Sized,
{
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_throughput(throughput),
            sample_duration: self.sample_duration,
            factory: self.factory,
        }
    }

    pub fn measurement_domain(&self, measurement_domain: MeasurementDomain) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_domain(measurement_domain),
            sample_duration: self.sample_duration,
            factory: self.factory,
        }
    }

    pub fn backend<B>(&self, backend_factory: B) -> Self
    where
        B: Fn() -> Box<dyn MeasurementBackend> + 'static,
    {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_backend(backend_factory),
            sample_duration: self.sample_duration,
            factory: self.factory,
        }
    }

    pub fn lifecycle<L, P>(&self, lifecycle_factory: P) -> Self
    where
        L: ConcurrentSampleLifecycle<T> + 'static,
        P: Fn() -> L + 'static,
    {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_lifecycle(lifecycle_factory),
            sample_duration: self.sample_duration,
            factory: self.factory,
        }
    }

    pub fn metadata(&self, key: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.with_metadata(key, value),
            sample_duration: self.sample_duration,
            factory: self.factory,
        }
    }

    pub fn sample_duration(&self, sample_duration: Duration) -> Self {
        assert!(
            sample_duration > Duration::ZERO,
            "concurrent benchmark sample_duration must be > 0"
        );
        Self {
            runner: self.runner,
            name: self.name,
            options: self.options.clone(),
            sample_duration,
            factory: self.factory,
        }
    }

    pub fn bench(&self, name: &str, workers: &[ConcurrentWorker<T>]) {
        self.runner.run_concurrent_with_options(
            name,
            self.name,
            self.sample_duration,
            workers,
            self.factory,
            self.options.throughput.clone(),
            self.options.measurement_domain,
            self.options.backend_factory.as_ref(),
            self.options.lifecycle_factory.as_ref(),
            self.options.metadata.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::stats::{median, median_absolute_deviation, percentile, tukey_outlier_count};
    use super::{DiagnosticError, DiagnosticResult, MeasurementDomain, MetricValue, Throughput};

    use crate::BenchmarkStats;

    fn stats_with_domain(domain: MeasurementDomain) -> BenchmarkStats {
        // A benchmark whose CPU PMU fields would normally trigger the
        // "data-side memory latency" diagnostic: high backend stalls plus
        // cache pressure. The same numbers are reused across all three
        // domain tests below so the only variable is `measurement_domain`.
        BenchmarkStats {
            throughput: Throughput::ops(),
            throughput_per_sec: 1.0,
            median_throughput_per_sec: 1.0,
            ns_per_op: 100.0,
            median_ns_per_op: 100.0,
            p95_ns_per_op: 110.0,
            mad_ns_per_op: 1.0,
            cycles_per_op: 1000.0,
            instructions_per_op: 100.0,
            ipc: 0.1,
            cache_references_per_op: 10.0,
            l1i_misses_per_op: 0.5,
            branches_per_op: 50.0,
            branch_miss_rate: 10.0,
            branch_misses_per_op: 5.0,
            cache_misses_per_op: 5.0,
            cache_miss_percent: 50.0,
            frontend_stall_cycles_per_op: 50.0,
            frontend_stall_percent: 50.0,
            backend_stall_cycles_per_op: 50.0,
            backend_stall_percent: 50.0,
            cv_percent: 1.0,
            outlier_count: 0,
            samples: 30,
            operations: 1000,
            total_duration_sec: 1.0,
            sample_throughput_per_sec: vec![1.0],
            sample_latency_ns_per_op: vec![100.0],
            has_cycles: true,
            has_instructions: true,
            has_cache_references: true,
            has_l1i_misses: true,
            has_branches: true,
            has_branch_misses: true,
            has_cache_misses: true,
            has_stalled_cycles_frontend: true,
            has_stalled_cycles_backend: true,
            pmu_time_enabled_ns: 1_000_000_000,
            pmu_time_running_ns: 1_000_000_000,
            measurement_domain: domain,
            measurement_label: String::new(),
            emits_cpu_diagnostics: true,
            metrics: Vec::new(),
            sample_metrics: Vec::new(),
        }
    }

    #[test]
    fn diagnose_stats_emits_cpu_pmu_bottlenecks_for_cpu_domain() {
        let stats = stats_with_domain(MeasurementDomain::Cpu);
        let diagnostics = super::diagnose_stats(&stats);
        // On CPU domain, at least one CPU-PMU bottleneck (memory latency,
        // branch/predictor, iCache pressure, low IPC) should fire.
        assert!(
            diagnostics.iter().any(|d| d.contains("memory latency")
                || d.contains("branch predictor")
                || d.contains("instruction-cache")
                || d.contains("Low IPC")),
            "expected CPU-PMU bottleneck diagnostic for Cpu domain, got: {diagnostics:?}"
        );
    }

    #[test]
    fn diagnose_stats_suppresses_cpu_pmu_bottlenecks_for_gpu_domain() {
        let stats = stats_with_domain(MeasurementDomain::Gpu);
        let diagnostics = super::diagnose_stats(&stats);
        // GPU domain must not surface CPU-PMU bottleneck messages — the
        // host thread's counters describe launch/sync orchestration, not
        // the GPU kernel. Stability warnings are still allowed.
        for diagnostic in &diagnostics {
            let is_cpu_pmu = diagnostic.contains("memory latency")
                || diagnostic.contains("branch predictor")
                || diagnostic.contains("instruction-cache")
                || diagnostic.contains("Low IPC");
            assert!(
                !is_cpu_pmu,
                "GPU domain should suppress CPU-PMU diagnostic: {diagnostic}"
            );
        }
    }

    #[test]
    fn diagnose_stats_suppresses_cpu_pmu_bottlenecks_for_io_domain() {
        let stats = stats_with_domain(MeasurementDomain::Io);
        let diagnostics = super::diagnose_stats(&stats);
        assert!(diagnostics.iter().all(|diagnostic| {
            !diagnostic.contains("memory latency")
                && !diagnostic.contains("branch predictor")
                && !diagnostic.contains("instruction-cache")
                && !diagnostic.contains("Low IPC")
        }));
    }

    #[test]
    fn concurrent_counter_per_op_uses_total_operations() {
        let counters = vec![super::CounterValue::new("retries", 30)];
        let summaries = super::summarize_worker_counters(&counters, 6, 2.0);
        assert_eq!(summaries[0].total, 30);
        assert_eq!(summaries[0].per_op, 5.0);
        assert_eq!(summaries[0].per_sec, 15.0);
    }

    #[test]
    fn concurrent_lifecycle_backend_metrics_and_metadata_are_persisted() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::time::Duration;

        struct Context;
        impl crate::ConcurrentBenchContext for Context {
            fn prepare(_num_threads: usize) -> Self {
                Self
            }
        }

        fn worker(
            _context: &Context,
            control: &crate::ConcurrentBenchControl,
        ) -> crate::ConcurrentWorkerResult {
            let mut operations = 0;
            while !control.should_stop() {
                operations += 1;
                std::hint::spin_loop();
            }
            crate::ConcurrentWorkerResult::operations(operations)
        }

        #[derive(Default)]
        struct Counts {
            measured_before: AtomicUsize,
            measured_after: AtomicUsize,
            backend_begin: AtomicUsize,
            backend_end: AtomicUsize,
            backend_collect: AtomicUsize,
        }

        struct Lifecycle {
            counts: Arc<Counts>,
        }
        impl crate::ConcurrentSampleLifecycle<Context> for Lifecycle {
            fn before_sample(
                &mut self,
                _context: &mut Context,
                sample: crate::ConcurrentSampleInfo,
            ) {
                if sample.phase == crate::ConcurrentSamplePhase::Measurement {
                    self.counts.measured_before.fetch_add(1, Ordering::SeqCst);
                }
            }

            fn after_sample(
                &mut self,
                _context: &mut Context,
                sample: crate::ConcurrentSampleInfo,
            ) -> Vec<MetricValue> {
                if sample.phase == crate::ConcurrentSamplePhase::Measurement {
                    self.counts.measured_after.fetch_add(1, Ordering::SeqCst);
                    vec![MetricValue::new(
                        "queue_depth",
                        sample.sample_index as f64,
                        "requests",
                    )]
                } else {
                    Vec::new()
                }
            }
        }

        struct Backend {
            counts: Arc<Counts>,
        }
        impl crate::MeasurementBackend for Backend {
            fn begin(&mut self) {
                self.counts.backend_begin.fetch_add(1, Ordering::SeqCst);
            }

            fn end(&mut self) {
                self.counts.backend_end.fetch_add(1, Ordering::SeqCst);
                std::thread::sleep(Duration::from_millis(10));
            }

            fn collect(
                &mut self,
                host_elapsed: Duration,
                operations: u64,
                sample_index: usize,
                results: &mut super::Results,
                metrics: &mut Vec<MetricValue>,
            ) {
                self.counts.backend_collect.fetch_add(1, Ordering::SeqCst);
                results.duration = host_elapsed;
                results.iterations = operations;
                results.chunks_executed = 1;
                metrics.push(MetricValue::new(
                    "service_window",
                    sample_index as f64,
                    "samples",
                ));
            }

            fn measurement_label(&self) -> &'static str {
                "service window"
            }
        }

        let counts = Arc::new(Counts::default());
        let lifecycle_counts = counts.clone();
        let backend_counts = counts.clone();
        let runner = crate::BenchmarkRunner::new().with_runtime(crate::BenchmarkRuntimeOptions {
            warm_up_duration: Duration::from_millis(2),
            benchmark_duration: Duration::from_millis(2),
            min_samples: 2,
            max_samples: 2,
        });
        let workers = [crate::ConcurrentWorker {
            name: "writer",
            threads: 1,
            run: worker,
        }];

        let run_started = std::time::Instant::now();
        runner.concurrent_group::<Context>("io", |g| {
            g.measurement_domain(MeasurementDomain::Io)
                .lifecycle(move || Lifecycle {
                    counts: lifecycle_counts.clone(),
                })
                .backend(move || {
                    Box::new(Backend {
                        counts: backend_counts.clone(),
                    })
                })
                .metadata("filesystem", "ext4")
                .sample_duration(Duration::from_millis(1))
                .bench("wal", &workers);
        });

        assert_eq!(counts.measured_before.load(Ordering::SeqCst), 2);
        assert_eq!(counts.measured_after.load(Ordering::SeqCst), 2);
        assert_eq!(counts.backend_begin.load(Ordering::SeqCst), 2);
        assert_eq!(counts.backend_end.load(Ordering::SeqCst), 2);
        assert_eq!(counts.backend_collect.load(Ordering::SeqCst), 2);

        let results = runner.results();
        let run_elapsed = run_started.elapsed();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].metadata.get("filesystem"), Some(&"ext4".into()));
        assert_eq!(results[0].stats.measurement_domain, MeasurementDomain::Io);
        assert_eq!(results[0].stats.measurement_label, "service window");
        assert_eq!(results[0].stats.sample_metrics.len(), 2);
        let measured_duration = Duration::from_secs_f64(results[0].stats.total_duration_sec);
        assert!(
            run_elapsed.saturating_sub(measured_duration) >= Duration::from_millis(15),
            "backend end overhead leaked into measured duration: run={run_elapsed:?} measured={measured_duration:?}"
        );
        for (index, sample) in results[0].stats.sample_metrics.iter().enumerate() {
            assert_eq!(sample.sample_index, index);
            assert_eq!(sample.metrics.len(), 2);
            let lifecycle_metric = sample
                .metrics
                .iter()
                .find(|metric| metric.name == "queue_depth")
                .unwrap();
            assert_eq!(lifecycle_metric.section, "scenario");
            assert_eq!(lifecycle_metric.value, index as f64);
        }
    }

    #[test]
    fn case_cooldown_has_a_mutable_setter() {
        let mut runner = crate::BenchmarkRunner::new();
        runner.set_case_cooldown(std::time::Duration::from_millis(25));
        assert_eq!(runner.case_cooldown, std::time::Duration::from_millis(25));
    }

    #[test]
    fn diagnose_stats_prefixes_cpu_pmu_bottlenecks_for_mixed_domain() {
        let stats = stats_with_domain(MeasurementDomain::Mixed);
        let diagnostics = super::diagnose_stats(&stats);
        // Mixed domain emits CPU-PMU diagnostics but tags them as host-side
        // context so a reader does not read them as the primary bottleneck.
        let any_cpu_pmu_prefixed = diagnostics.iter().any(|d| {
            d.starts_with("[host] ")
                && (d.contains("memory latency")
                    || d.contains("branch predictor")
                    || d.contains("instruction-cache")
                    || d.contains("Low IPC"))
        });
        assert!(
            any_cpu_pmu_prefixed,
            "expected at least one [host]-prefixed CPU-PMU diagnostic for Mixed domain, got: {diagnostics:?}"
        );
    }

    #[test]
    fn diagnose_stats_keeps_stability_warnings_for_gpu_domain() {
        let mut stats = stats_with_domain(MeasurementDomain::Gpu);
        // Make the run stability diagnostic fire: high CV and outliers.
        stats.cv_percent = 25.0;
        stats.outlier_count = 10;
        stats.samples = 30;
        let diagnostics = super::diagnose_stats(&stats);
        assert!(
            diagnostics
                .iter()
                .any(|d| d.contains("Run stability is weak")),
            "expected run-stability warning for GPU domain, got: {diagnostics:?}"
        );
    }

    #[test]
    fn percentile_interpolates_sorted_values() {
        let values = [1.0, 2.0, 3.0, 4.0];
        assert_eq!(percentile(&values, 0.0), 1.0);
        assert_eq!(percentile(&values, 0.5), 2.5);
        assert_eq!(percentile(&values, 1.0), 4.0);
    }

    #[test]
    fn mad_is_zero_for_uniform_values() {
        let values = [5.0, 5.0, 5.0];
        assert_eq!(median(&values), 5.0);
        assert_eq!(median_absolute_deviation(&values, 5.0), 0.0);
    }

    #[test]
    fn tukey_outlier_count_flags_far_values() {
        let values = [10.0, 10.0, 11.0, 11.0, 12.0, 100.0];
        assert_eq!(tukey_outlier_count(&values), 1);
    }

    #[test]
    fn throughput_format_scales_custom_units() {
        let throughput = Throughput::per_operation(1000, "lines");
        assert_eq!(throughput.format_rate(12_500.0), "12.50 klines/s");
    }

    #[test]
    fn automatic_calibration_can_select_small_chunks_for_expensive_operations() {
        use super::{BenchmarkRuntimeOptions, calibrate_engine};
        use crate::BenchContext;
        use std::time::Duration;

        struct SlowContext;

        impl BenchContext for SlowContext {
            fn prepare(_num_chunks: usize) -> Self {
                Self
            }
        }

        fn slow_operation(_ctx: &mut SlowContext, chunk_size: usize, _chunk_num: usize) {
            std::thread::sleep(Duration::from_millis(chunk_size as u64));
        }

        let runtime = BenchmarkRuntimeOptions {
            warm_up_duration: Duration::from_millis(1),
            benchmark_duration: Duration::from_millis(100),
            min_samples: 2,
            max_samples: 10,
        };
        let config = calibrate_engine(
            slow_operation,
            &|| SlowContext,
            &Throughput::ops(),
            &runtime,
            &mut crate::WallClockBackend::new(),
        );

        assert!(
            (10..=100).contains(&config.chunk_size),
            "expected a roughly 50 ms chunk, got {}",
            config.chunk_size
        );
    }

    #[test]
    fn fixed_chunk_sample_count_uses_measured_warm_up_duration() {
        use super::{BenchmarkRuntimeOptions, calibrate_engine};
        use crate::{BenchContext, MeasurementBackend, MetricValue, bench::Results};
        use std::time::Duration;

        struct FixedContext;

        impl BenchContext for FixedContext {
            fn prepare(_num_chunks: usize) -> Self {
                Self
            }

            fn chunk_size() -> Option<usize> {
                Some(1)
            }
        }

        struct FixedDurationBackend;

        impl MeasurementBackend for FixedDurationBackend {
            fn begin(&mut self) {}

            fn end(&mut self) {}

            fn collect(
                &mut self,
                _host_elapsed: Duration,
                operations: u64,
                _chunk_index: usize,
                results: &mut Results,
                _metrics: &mut Vec<MetricValue>,
            ) {
                results.duration = Duration::from_millis(4);
                results.iterations = operations;
                results.chunks_executed = 1;
            }
        }

        fn fixed_operation(_ctx: &mut FixedContext, _chunk_size: usize, _chunk_num: usize) {
            std::thread::sleep(Duration::from_millis(1));
        }

        let runtime = BenchmarkRuntimeOptions {
            warm_up_duration: Duration::from_millis(5),
            benchmark_duration: Duration::from_millis(20),
            min_samples: 2,
            max_samples: 20,
        };
        let config = calibrate_engine(
            fixed_operation,
            &|| FixedContext,
            &Throughput::ops(),
            &runtime,
            &mut FixedDurationBackend,
        );

        assert_eq!(config.chunk_size, 1);
        assert_eq!(config.target_samples, 5);
        assert!(config.estimated_throughput_per_sec > 0.0);
    }

    /// End-to-end regression test: `bench_sample(...)` must populate
    /// `BenchmarkStats.metrics` with the metrics returned by the bench
    /// function. This test exists because a previous commit collected
    /// `all_metrics` per sample but then passed `&[]` to
    /// `benchmark_stats_from_samples`, silently dropping them.
    #[test]
    fn bench_sample_populates_custom_metrics() {
        use crate::bench::backend::BenchSampleResult;
        use std::time::Duration;

        struct DummyCtx;
        impl crate::BenchContext for DummyCtx {
            fn prepare(_num_chunks: usize) -> Self {
                DummyCtx
            }
            fn chunk_size() -> Option<usize> {
                Some(1_000)
            }
        }

        fn sample_bench(
            _ctx: &mut DummyCtx,
            chunk_size: usize,
            _chunk_num: usize,
        ) -> BenchSampleResult {
            BenchSampleResult::operations(chunk_size as u64)
                .with_metric("cuda_event_ms", 0.5, "ms")
                .with_metric("tflops", 10.0, "TFLOP/s")
        }

        let runner = crate::BenchmarkRunner::new().with_runtime(crate::BenchmarkRuntimeOptions {
            warm_up_duration: Duration::from_millis(10),
            benchmark_duration: Duration::from_millis(50),
            min_samples: 3,
            max_samples: 5,
        });

        runner.run_sample_with_factory_and_throughput(
            "test_bench_sample_metrics",
            "test",
            sample_bench as fn(&mut DummyCtx, usize, usize) -> BenchSampleResult,
            &|| DummyCtx,
            Throughput::ops(),
            MeasurementDomain::Cpu,
            Box::new(crate::WallClockBackend::new()),
            None,
            1,
        );

        let results = runner.results();
        assert_eq!(results.len(), 1, "expected exactly one result");
        let stats = &results[0].stats;
        assert!(
            !stats.metrics.is_empty(),
            "bench_sample must populate stats.metrics — this is the regression where \
             all_metrics was collected but &[] was passed to benchmark_stats_from_samples"
        );
        let names: Vec<&str> = stats.metrics.iter().map(|m| m.name.as_str()).collect();
        assert!(
            names.contains(&"cuda_event_ms"),
            "expected cuda_event_ms in {names:?}"
        );
        assert!(names.contains(&"tflops"), "expected tflops in {names:?}");
    }

    #[test]
    fn diagnostic_pass_populates_metrics_without_timing_samples() {
        use std::time::Duration;

        struct DummyCtx;
        impl crate::BenchContext for DummyCtx {
            fn prepare(_num_chunks: usize) -> Self {
                DummyCtx
            }
            fn chunk_size() -> Option<usize> {
                Some(1_000)
            }
        }

        fn timed_bench(_ctx: &mut DummyCtx, chunk_size: usize, _chunk_num: usize) {
            let mut value = 0usize;
            for i in 0..chunk_size {
                value = value.wrapping_add(i);
            }
            std::hint::black_box(value);
        }

        fn diagnostic_bench(
            _ctx: &mut DummyCtx,
            chunk_size: usize,
            _chunk_num: usize,
        ) -> Result<DiagnosticResult, DiagnosticError> {
            timed_bench(_ctx, chunk_size, _chunk_num);
            Ok(DiagnosticResult::new("gpu counters")
                .push_metric(MetricValue::new("dram_pct", 72.0, "%").with_display_name("DRAM %")))
        }

        let runner = crate::BenchmarkRunner::new().with_runtime(crate::BenchmarkRuntimeOptions {
            warm_up_duration: Duration::from_millis(10),
            benchmark_duration: Duration::from_millis(50),
            min_samples: 3,
            max_samples: 3,
        });

        runner.group::<DummyCtx>("test", |g| {
            g.diagnostic_pass(diagnostic_bench)
                .bench("timed_then_diagnostic", timed_bench);
        });

        let results = runner.results();
        assert_eq!(results.len(), 1, "expected exactly one result");
        let stats = &results[0].stats;
        assert_eq!(
            stats.samples, 3,
            "diagnostic pass must not add timing samples"
        );

        let metric = stats
            .metrics
            .iter()
            .find(|m| m.name == "dram_pct")
            .expect("expected diagnostic metric");
        assert_eq!(
            metric.samples, 1,
            "diagnostic metric should come from one replay pass"
        );
        assert_eq!(metric.median, 72.0);
    }
}
