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
use perf::{clear_perf_issues, execute_concurrent_worker, execute_standard};
use perf::{enforce_pmu_quality, measurement_label, warn_perf_status};
use serde::{Deserialize, Serialize};
use stats::{
    benchmark_stats_from_samples, colorize_section_heading, render_combined_stats_table,
    render_stats_table,
};
use std::time::Instant;
use std::{
    hint::black_box,
    io::{self, IsTerminal, Write},
    sync::Barrier,
    thread,
    time::Duration,
};

#[cfg(target_os = "linux")]
pub use perf::PerfCounters;

const MIN_CHUNK_SIZE: usize = 100_000;
const MAX_CHUNK_SIZE: usize = 50_000_000;
const TARGET_CHUNK_DURATION: Duration = Duration::from_millis(50);
const DEFAULT_CONCURRENT_SAMPLE_DURATION: Duration = Duration::from_millis(50);
const DEFAULT_WARM_UP_DURATION: Duration = Duration::from_secs(1);
const DEFAULT_BENCHMARK_DURATION: Duration = Duration::from_secs(5);
const MIN_SAMPLES: usize = 20;
const MAX_SAMPLES: usize = 100;

type BenchFunction<T> = fn(&mut T, usize, usize);
type ConcurrentBenchFunction<T> = fn(&T, &ConcurrentBenchControl) -> ConcurrentWorkerResult;

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
        measurement_label(has_perf_counters(&measurement_results_from_stats(stats))),
        border_color,
    ) {
        println!("{pmu_byline}");
    }

    render_diagnostics(stats);
}

fn render_standard_results(name: &str, stats: &crate::BenchmarkStats) {
    let results = measurement_results_from_stats(stats);
    let has_perf = has_perf_counters(&results);
    warn_perf_status(has_perf, has_full_perf_counters(&results));
    enforce_pmu_quality(name, has_perf, &results);

    println!("  results:");
    if let Some(pmu_byline) = render_stats_table(stats, measurement_label(has_perf), None) {
        println!("{pmu_byline}");
    }
    render_diagnostics(stats);
}

fn render_concurrent_results(
    name: &str,
    combined_stats: &crate::BenchmarkStats,
    worker_summaries: &[WorkerSummary],
) {
    let results = measurement_results_from_stats(combined_stats);
    let has_perf = has_perf_counters(&results);
    warn_perf_status(has_perf, has_full_perf_counters(&results));
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

fn diagnose_stats(stats: &crate::BenchmarkStats) -> Vec<String> {
    let mut diagnostics = Vec::new();

    if stats.has_cycles
        && stats.has_stalled_cycles_backend
        && stats.backend_stall_percent >= 20.0
        && stats.has_cache_misses
        && ((stats.has_cache_references && stats.cache_miss_percent >= 5.0)
            || stats.cache_misses_per_op >= 0.05)
    {
        diagnostics.push(format!(
            "Likely data-side memory latency: backend stall is {:.1}% with cache pressure at {:.2}% miss rate and {:.4} misses/op",
            stats.backend_stall_percent,
            stats.cache_miss_percent,
            stats.cache_misses_per_op
        ));
    }

    if stats.has_cycles
        && stats.has_stalled_cycles_frontend
        && stats.frontend_stall_percent >= 20.0
        && stats.has_branches
        && stats.has_branch_misses
        && stats.branch_miss_rate >= 3.0
    {
        diagnostics.push(format!(
            "Likely branch predictor or fetch disruption: frontend stall is {:.1}% with {:.2}% branch misses",
            stats.frontend_stall_percent,
            stats.branch_miss_rate
        ));
    }

    if stats.has_cycles
        && stats.has_stalled_cycles_frontend
        && stats.frontend_stall_percent >= 20.0
        && stats.has_l1i_misses
        && stats.l1i_misses_per_op >= 0.01
    {
        diagnostics.push(format!(
            "Likely instruction-cache pressure: frontend stall is {:.1}% with {:.4} L1I misses/op",
            stats.frontend_stall_percent, stats.l1i_misses_per_op
        ));
    }

    if stats.has_cycles
        && stats.has_instructions
        && stats.ipc > 0.0
        && stats.ipc < 1.0
        && diagnostics.is_empty()
    {
        diagnostics.push(format!(
            "Low IPC ({:.3}) suggests poor machine utilization; look for dependency chains, execution-port pressure, or latent memory effects",
            stats.ipc
        ));
    }

    if stats.cv_percent >= 10.0 || stats.outlier_count >= (stats.samples / 10).max(2) {
        diagnostics.push(format!(
            "Run stability is weak: CV {:.2}% with {} outliers across {} samples",
            stats.cv_percent, stats.outlier_count, stats.samples
        ));
    }

    diagnostics
}

fn calibrate_engine<T: BenchContext, F: Fn() -> T + ?Sized>(
    f: &BenchFunction<T>,
    factory: &F,
    throughput: &Throughput,
    runtime: &BenchmarkRuntimeOptions,
) -> BenchmarkConfig {
    rewrite_line("🔥 calibrating benchmark");

    if let Some(preferred_chunk_size) = T::chunk_size() {
        let warm_up_end = Instant::now() + runtime.warm_up_duration;
        let mut warm_up_count = 0;
        while Instant::now() < warm_up_end {
            let mut prepared = factory();
            black_box(|| f(&mut prepared, preferred_chunk_size, warm_up_count))();
            warm_up_count += 1;
            let remaining_ms = warm_up_end
                .saturating_duration_since(Instant::now())
                .as_millis();
            rewrite_line(&format!(
                "🔥 calibrating benchmark  warmup remaining: {remaining_ms:>4} ms  chunk: {preferred_chunk_size}"
            ));
        }

        clear_line();
        return BenchmarkConfig {
            chunk_size: preferred_chunk_size,
            target_samples: runtime.min_samples,
            estimated_throughput_per_sec: 0.0,
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
            chunk_size = ((chunk_size as f64) * scaling_factor)
                .round()
                .clamp(MIN_CHUNK_SIZE as f64, MAX_CHUNK_SIZE as f64)
                as usize;
            best_chunk_size = chunk_size;
        } else {
            chunk_size = (chunk_size * 10).min(MAX_CHUNK_SIZE);
            best_chunk_size = chunk_size;
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
    while Instant::now() < warm_up_end {
        let mut prepared = factory();
        black_box(|| f(&mut prepared, best_chunk_size, warm_up_count))();
        warm_up_count += 1;
        let remaining_ms = warm_up_end
            .saturating_duration_since(Instant::now())
            .as_millis();
        rewrite_line(&format!(
            "🔥 calibrating benchmark  warmup remaining: {remaining_ms:>4} ms  chunk: {best_chunk_size:>9}"
        ));
    }

    let estimated_chunk_duration_secs = if estimated_throughput_per_sec > 0.0 {
        let chunk_throughput_amount =
            best_chunk_size as f64 * throughput.amount_per_operation as f64;
        chunk_throughput_amount / estimated_throughput_per_sec
    } else {
        TARGET_CHUNK_DURATION.as_secs_f64()
    };
    let target_samples =
        ((runtime.benchmark_duration.as_secs_f64() / estimated_chunk_duration_secs) as usize)
            .clamp(runtime.min_samples, runtime.max_samples);

    clear_line();
    BenchmarkConfig {
        chunk_size: best_chunk_size,
        target_samples,
        estimated_throughput_per_sec,
    }
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
) -> ConcurrentBenchmarkConfig {
    rewrite_line("🔥 calibrating benchmark");

    let total_threads = total_worker_threads(workers);
    let warm_up_end = Instant::now() + runtime.warm_up_duration;
    let mut estimated_throughput_per_sec = 0.0;

    while Instant::now() < warm_up_end {
        let prepared = factory(total_threads);
        let result = execute_concurrent_timing_only(&prepared, sample_duration, workers);
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
    let target_samples =
        ((runtime.benchmark_duration.as_secs_f64() / sample_duration.as_secs_f64()) as usize)
            .clamp(runtime.min_samples, runtime.max_samples);

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

fn execute_standard_sample<T: BenchContext>(
    f: &BenchFunction<T>,
    prepared: &mut T,
    chunk_size: usize,
    chunk_num: usize,
    ops: u64,
) -> Results {
    #[cfg(target_os = "linux")]
    {
        execute_standard(|| {
            black_box(|| f(prepared, chunk_size, chunk_num))();
            ops
        })
    }

    #[cfg(not(target_os = "linux"))]
    {
        execute_timing_only(|| {
            black_box(|| f(prepared, chunk_size, chunk_num))();
            ops
        })
    }
}

fn execute_concurrent_timing_only<T: ConcurrentBenchContext + Sync>(
    prepared: &T,
    sample_duration: Duration,
    workers: &[ConcurrentWorker<T>],
) -> ConcurrentSampleResult {
    execute_concurrent_sample_inner(
        prepared,
        sample_duration,
        workers,
        #[cfg(target_os = "linux")]
        &[],
        false,
    )
}

fn execute_concurrent_sample<T: ConcurrentBenchContext + Sync>(
    prepared: &T,
    sample_duration: Duration,
    workers: &[ConcurrentWorker<T>],
    #[cfg(target_os = "linux")] pin_cores: &[usize],
) -> ConcurrentSampleResult {
    execute_concurrent_sample_inner(
        prepared,
        sample_duration,
        workers,
        #[cfg(target_os = "linux")]
        pin_cores,
        true,
    )
}

fn execute_concurrent_sample_inner<T: ConcurrentBenchContext + Sync>(
    prepared: &T,
    sample_duration: Duration,
    workers: &[ConcurrentWorker<T>],
    #[cfg(target_os = "linux")] pin_cores: &[usize],
    use_perf_counters: bool,
) -> ConcurrentSampleResult {
    let total_threads = total_worker_threads(workers);
    let ready_barrier = Barrier::new(total_threads + 1);
    let start_barrier = Barrier::new(total_threads + 1);
    let start_instant = std::sync::OnceLock::new();

    thread::scope(|scope| {
        let mut handles = Vec::with_capacity(total_threads);
        let ready_barrier = &ready_barrier;
        let start_barrier = &start_barrier;
        let start_instant = &start_instant;

        let mut next_thread_index = 0usize;
        for (worker_index, worker) in workers.iter().enumerate() {
            for role_thread_index in 0..worker.threads {
                let run = worker.run;
                let thread_index = next_thread_index;
                next_thread_index += 1;

                handles.push(scope.spawn(move || {
                    ready_barrier.wait();
                    start_barrier.wait();

                    let benchmark_start = *start_instant.get().expect("missing benchmark start");
                    let control = ConcurrentBenchControl {
                        deadline: benchmark_start + sample_duration,
                        thread_index,
                        role_thread_index,
                    };

                    #[cfg(target_os = "linux")]
                    if use_perf_counters {
                        if let Some(core_id) = pin_cores.get(thread_index).copied() {
                            if let Err(error) = crate::threading::pin_current_thread_to_core(core_id) {
                                warn_affinity_once(format!(
                                    "Could not pin concurrent benchmark worker {thread_index} to core {core_id}: {error}. Continuing without worker pinning"
                                ));
                            }
                        }
                    }

                    let worker_result = if use_perf_counters {
                        #[cfg(target_os = "linux")]
                        {
                            execute_concurrent_worker(prepared, &control, run)
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
            throughput: Throughput::ops(),
            _marker: std::marker::PhantomData,
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
        );
    }

    pub fn run_with_factory<T: BenchContext, F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        group: &str,
        f: BenchFunction<T>,
        factory: &F,
    ) {
        self.run_with_factory_and_throughput(name, group, f, factory, Throughput::ops());
    }

    pub fn run_with_factory_and_throughput<T: BenchContext, F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        group: &str,
        f: BenchFunction<T>,
        factory: &F,
        throughput: Throughput,
    ) {
        if !self.should_run(name, group) {
            return;
        }

        #[cfg(target_os = "linux")]
        clear_perf_issues();

        let _affinity_guard = BenchAffinityGuard::acquire();
        println!("\nBenchmark: {name}");

        let config = calibrate_engine(&f, factory, &throughput, &self.runtime);
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

        for sample in 0..config.target_samples {
            let mut prepared = factory();
            let ops = T::operations_per_chunk().unwrap_or(config.chunk_size as u64);
            let sample_result =
                execute_standard_sample(&f, &mut prepared, config.chunk_size, sample, ops);

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

        let stats = benchmark_stats_from_samples(
            &summed_results,
            &all_results,
            config.target_samples,
            &throughput,
        );
        render_standard_results(name, &stats);

        self.session.add_result(BenchmarkResult {
            name: name.to_string(),
            group: group.to_string(),
            kind: BenchmarkKind::Standard,
            stats,
            worker_summaries: Vec::new(),
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
        );
    }

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
    ) {
        if !self.should_run(name, group) {
            return;
        }
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

        let config = warm_up_concurrent_engine(
            sample_duration,
            workers,
            factory,
            &throughput,
            &self.runtime,
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

        for sample in 0..config.target_samples {
            let prepared = factory(total_threads);
            let sample_result = execute_concurrent_sample(
                &prepared,
                config.sample_duration,
                workers,
                #[cfg(target_os = "linux")]
                &pin_cores,
            );

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
                    );
                    WorkerSummary {
                        name: worker.name.to_string(),
                        threads: worker.threads,
                        counters: summarize_worker_counters(
                            worker_counters,
                            stats.operations,
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
            stats,
            worker_summaries,
        });
    }
}

pub struct BenchmarkGroup<'a, T: BenchContext> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    throughput: Throughput,
    _marker: std::marker::PhantomData<T>,
}

pub struct BenchmarkGroupWithFactory<'a, 'f, T: BenchContext, F: Fn() -> T + ?Sized> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    throughput: Throughput,
    factory: &'f F,
}

impl<'a, T: BenchContext> BenchmarkGroup<'a, T> {
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput,
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
            factory,
        }
    }

    pub fn with_factory<'f, F: Fn() -> T + ?Sized>(
        &self,
        factory: &'f F,
    ) -> BenchmarkGroupWithFactory<'a, 'f, T, F> {
        self.factory(factory)
    }

    pub fn bench(&self, name: &str, f: BenchFunction<T>) {
        self.runner.run_with_factory_and_throughput(
            name,
            self.name,
            f,
            &|| T::prepare(T::chunk_size().unwrap_or(MIN_CHUNK_SIZE)),
            self.throughput.clone(),
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
            factory: self.factory,
        }
    }

    pub fn bench(&self, name: &str, f: BenchFunction<T>) {
        self.runner.run_with_factory_and_throughput(
            name,
            self.name,
            f,
            self.factory,
            self.throughput.clone(),
        );
    }
}

pub struct ConcurrentBenchmarkGroup<'a, T: ConcurrentBenchContext> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    throughput: Throughput,
    _marker: std::marker::PhantomData<T>,
}

pub struct ConcurrentBenchmarkGroupWithDuration<'a, T: ConcurrentBenchContext> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    throughput: Throughput,
    sample_duration: Duration,
    _marker: std::marker::PhantomData<T>,
}

pub struct ConcurrentBenchmarkGroupWithFactory<
    'a,
    'f,
    T: ConcurrentBenchContext,
    F: Fn(usize) -> T + ?Sized,
> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    throughput: Throughput,
    sample_duration: Duration,
    factory: &'f F,
}

impl<'a, T: ConcurrentBenchContext> ConcurrentBenchmarkGroup<'a, T>
where
    T: Send + Sync,
{
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput,
            _marker: std::marker::PhantomData,
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
            throughput: self.throughput.clone(),
            sample_duration,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn bench(&self, name: &str, sample_duration: Duration, workers: &[ConcurrentWorker<T>]) {
        self.runner.run_concurrent_with_factory_and_throughput(
            name,
            self.name,
            sample_duration,
            workers,
            &|num_threads| T::prepare(num_threads),
            self.throughput.clone(),
        );
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
            throughput: self.throughput.clone(),
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
            .throughput(self.throughput.clone())
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

impl<'a, T: ConcurrentBenchContext> ConcurrentBenchmarkGroupWithDuration<'a, T>
where
    T: Send + Sync,
{
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput,
            sample_duration: self.sample_duration,
            _marker: std::marker::PhantomData,
        }
    }

    pub fn factory<'f, F: Fn(usize) -> T + ?Sized>(
        &self,
        factory: &'f F,
    ) -> ConcurrentBenchmarkGroupWithFactory<'a, 'f, T, F> {
        ConcurrentBenchmarkGroupWithFactory {
            runner: self.runner,
            name: self.name,
            throughput: self.throughput.clone(),
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
        self.runner.run_concurrent_with_factory_and_throughput(
            name,
            self.name,
            self.sample_duration,
            workers,
            &|num_threads| T::prepare(num_threads),
            self.throughput.clone(),
        );
    }
}

impl<'a, 'f, T: ConcurrentBenchContext, F: Fn(usize) -> T + ?Sized>
    ConcurrentBenchmarkGroupWithFactory<'a, 'f, T, F>
where
    T: Send + Sync,
{
    pub fn throughput(&self, throughput: Throughput) -> Self {
        Self {
            runner: self.runner,
            name: self.name,
            throughput,
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
            throughput: self.throughput.clone(),
            sample_duration,
            factory: self.factory,
        }
    }

    pub fn bench(&self, name: &str, workers: &[ConcurrentWorker<T>]) {
        self.runner.run_concurrent_with_factory_and_throughput(
            name,
            self.name,
            self.sample_duration,
            workers,
            self.factory,
            self.throughput.clone(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::Throughput;
    use super::stats::{median, median_absolute_deviation, percentile, tukey_outlier_count};

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
}
