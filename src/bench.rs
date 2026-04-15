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
use stats::{
    benchmark_stats_from_samples, colorize_section_heading, render_combined_stats_table,
    render_stats_table,
};
use std::time::Instant;
use std::{
    hint::black_box,
    io::{self, Write},
    sync::Barrier,
    thread,
    time::Duration,
};

#[cfg(target_os = "linux")]
pub use perf::PerfCounters;

const MIN_CHUNK_SIZE: usize = 100_000;
const MAX_CHUNK_SIZE: usize = 50_000_000;
const TARGET_CHUNK_DURATION: Duration = Duration::from_millis(50);
const WARM_UP_DURATION: Duration = Duration::from_secs(1);
const MIN_BENCHMARK_DURATION: Duration = Duration::from_secs(5);
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
    pub instructions: u64,
    pub branches: u64,
    pub branch_misses: u64,
    pub cache_misses: u64,
    pub has_instructions: bool,
    pub has_branches: bool,
    pub has_branch_misses: bool,
    pub has_cache_misses: bool,
    pub pmu_time_enabled_ns: u64,
    pub pmu_time_running_ns: u64,
    pub duration: Duration,
    pub iterations: u64,
    pub chunks_executed: u64,
}

impl Results {
    pub fn add(&mut self, other: &Results) {
        self.instructions += other.instructions;
        self.branches += other.branches;
        self.branch_misses += other.branch_misses;
        self.cache_misses += other.cache_misses;
        self.has_instructions |= other.has_instructions;
        self.has_branches |= other.has_branches;
        self.has_branch_misses |= other.has_branch_misses;
        self.has_cache_misses |= other.has_cache_misses;
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

        self.instructions /= divisor;
        self.branches /= divisor;
        self.branch_misses /= divisor;
        self.cache_misses /= divisor;
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
    pub estimated_ops_per_sec: f64,
}

struct ConcurrentBenchmarkConfig {
    sample_duration: Duration,
    target_samples: usize,
    estimated_ops_per_sec: f64,
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

fn estimated_mops_display(ops_per_sec: f64) -> String {
    if ops_per_sec > 0.0 {
        format!("{:.2} Mops/s", ops_per_sec / 1_000_000.0)
    } else {
        "n/a".to_string()
    }
}

fn has_perf_counters(results: &Results) -> bool {
    results.has_instructions
        || results.has_branches
        || results.has_branch_misses
        || results.has_cache_misses
}

fn has_full_perf_counters(results: &Results) -> bool {
    results.has_instructions
        && results.has_branches
        && results.has_branch_misses
        && results.has_cache_misses
}

fn measurement_results_from_stats(stats: &crate::BenchmarkStats) -> Results {
    Results {
        has_instructions: stats.has_instructions,
        has_branches: stats.has_branches,
        has_branch_misses: stats.has_branch_misses,
        has_cache_misses: stats.has_cache_misses,
        pmu_time_enabled_ns: stats.pmu_time_enabled_ns,
        pmu_time_running_ns: stats.pmu_time_running_ns,
        ..Results::default()
    }
}

fn update_running_throughput(running_throughput: &mut f64, result: &Results) {
    let sample_throughput_mops =
        safe_ratio_f64(result.iterations as f64, result.duration.as_secs_f64()) / 1_000_000.0;
    if sample_throughput_mops > 0.0 {
        *running_throughput = *running_throughput * 0.9 + sample_throughput_mops * 0.1;
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

fn calibrate_engine<T: BenchContext, F: Fn() -> T + ?Sized>(
    f: &BenchFunction<T>,
    factory: &F,
) -> BenchmarkConfig {
    rewrite_line("🔥 calibrating benchmark");

    if let Some(preferred_chunk_size) = T::chunk_size() {
        let warm_up_end = Instant::now() + WARM_UP_DURATION;
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
            target_samples: MIN_SAMPLES,
            estimated_ops_per_sec: 0.0,
        };
    }

    let mut chunk_size = MIN_CHUNK_SIZE;
    let mut best_chunk_size = chunk_size;
    let mut ops_per_sec = 0.0;

    for i in 0..15 {
        let mut prepared = factory();
        let started = Instant::now();
        black_box(|| f(&mut prepared, chunk_size, 0))();
        let duration = started.elapsed();
        let duration_secs = duration.as_secs_f64();

        if duration_secs >= 0.0001 {
            ops_per_sec = chunk_size as f64 / duration_secs;
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
            "🔥 calibrating benchmark  pass: {:>2}/15  chunk: {:>9}  est: {:>8.2} Mops/s",
            i + 1,
            chunk_size,
            ops_per_sec / 1_000_000.0
        ));
    }

    let warm_up_end = Instant::now() + WARM_UP_DURATION;
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

    let estimated_chunk_duration_secs = if ops_per_sec > 0.0 {
        best_chunk_size as f64 / ops_per_sec
    } else {
        TARGET_CHUNK_DURATION.as_secs_f64()
    };
    let target_samples = ((MIN_BENCHMARK_DURATION.as_secs_f64() / estimated_chunk_duration_secs)
        as usize)
        .clamp(MIN_SAMPLES, MAX_SAMPLES);

    clear_line();
    BenchmarkConfig {
        chunk_size: best_chunk_size,
        target_samples,
        estimated_ops_per_sec: ops_per_sec,
    }
}

fn total_worker_threads<T>(workers: &[ConcurrentWorker<T>]) -> usize {
    workers.iter().map(|worker| worker.threads).sum()
}

fn warm_up_concurrent_engine<T: ConcurrentBenchContext + Sync, F: Fn(usize) -> T + ?Sized>(
    sample_duration: Duration,
    workers: &[ConcurrentWorker<T>],
    factory: &F,
) -> ConcurrentBenchmarkConfig {
    rewrite_line("🔥 calibrating benchmark");

    let total_threads = total_worker_threads(workers);
    let warm_up_end = Instant::now() + WARM_UP_DURATION;
    let mut estimated_ops_per_sec = 0.0;

    while Instant::now() < warm_up_end {
        let prepared = factory(total_threads);
        let result = execute_concurrent_timing_only(&prepared, sample_duration, workers);
        let throughput = safe_ratio_f64(
            result.results.iterations as f64,
            result.results.duration.as_secs_f64(),
        );
        if throughput > 0.0 {
            estimated_ops_per_sec = if estimated_ops_per_sec > 0.0 {
                estimated_ops_per_sec * 0.8 + throughput * 0.2
            } else {
                throughput
            };
        }
        let remaining_ms = warm_up_end
            .saturating_duration_since(Instant::now())
            .as_millis();
        rewrite_line(&format!(
            "🔥 calibrating benchmark  warmup remaining: {remaining_ms:>4} ms  sample: {:>4} ms  est: {}",
            sample_duration.as_millis(),
            estimated_mops_display(estimated_ops_per_sec)
        ));
    }

    clear_line();
    let target_samples = ((MIN_BENCHMARK_DURATION.as_secs_f64() / sample_duration.as_secs_f64())
        as usize)
        .clamp(MIN_SAMPLES, MAX_SAMPLES);

    ConcurrentBenchmarkConfig {
        sample_duration,
        target_samples,
        estimated_ops_per_sec,
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

fn update_progress_bar(current: usize, total: usize, current_throughput: f64) {
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
        if current_throughput > 1000.0 {
            format!("{current_throughput:.0} Mops/s")
        } else {
            format!("{current_throughput:.1} Mops/s")
        }
    } else {
        "Calculating...".to_string()
    };

    print!("] {percentage}% ({current}/{total}) {throughput_display}");
    flush_stdout();
}

pub struct BenchmarkRunner {
    session: std::sync::Arc<BenchmarkSession>,
    filter: Option<String>,
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

    pub fn group<T: BenchContext>(&self, name: &'static str, f: impl FnOnce(&BenchmarkGroup<T>)) {
        let group = BenchmarkGroup {
            runner: self,
            name,
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
        self.run_with_factory(name, group, f, &|| T::prepare(MIN_CHUNK_SIZE));
    }

    pub fn run_with_factory<T: BenchContext, F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        group: &str,
        f: BenchFunction<T>,
        factory: &F,
    ) {
        if !self.should_run(name, group) {
            return;
        }

        #[cfg(target_os = "linux")]
        clear_perf_issues();

        let _affinity_guard = BenchAffinityGuard::acquire();
        println!("\nBenchmark: {name}");

        let config = calibrate_engine(&f, factory);
        println!(
            "  calibrated: chunk={} samples={} estimate={}",
            config.chunk_size,
            config.target_samples,
            estimated_mops_display(config.estimated_ops_per_sec),
        );

        rewrite_line(&format!("⚡ running 0/{} samples", config.target_samples));
        let mut all_results = Vec::with_capacity(config.target_samples);
        let mut summed_results = Results::default();
        let mut running_throughput = config.estimated_ops_per_sec / 1_000_000.0;

        for sample in 0..config.target_samples {
            let mut prepared = factory();
            let ops = T::operations_per_chunk().unwrap_or(config.chunk_size as u64);
            let sample_result =
                execute_standard_sample(&f, &mut prepared, config.chunk_size, sample, ops);

            update_running_throughput(&mut running_throughput, &sample_result);
            summed_results.add(&sample_result);
            all_results.push(sample_result);

            if sample % 2 == 0 || sample == config.target_samples - 1 {
                update_progress_bar(sample + 1, config.target_samples, running_throughput);
            }
        }

        clear_line();
        println!("  samples complete: {}", config.target_samples);

        let stats =
            benchmark_stats_from_samples(&summed_results, &all_results, config.target_samples);
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
        self.run_concurrent_with_factory(name, group, sample_duration, workers, &|num_threads| {
            T::prepare(num_threads)
        });
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

        let config = warm_up_concurrent_engine(sample_duration, workers, factory);
        println!(
            "  calibrated: sample={}ms samples={} estimate={}",
            config.sample_duration.as_millis(),
            config.target_samples,
            estimated_mops_display(config.estimated_ops_per_sec),
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
        let mut running_throughput = config.estimated_ops_per_sec / 1_000_000.0;

        for sample in 0..config.target_samples {
            let prepared = factory(total_threads);
            let sample_result = execute_concurrent_sample(
                &prepared,
                config.sample_duration,
                workers,
                #[cfg(target_os = "linux")]
                &pin_cores,
            );

            update_running_throughput(&mut running_throughput, &sample_result.results);
            for (index, worker_summary) in sample_result.worker_summaries.iter().enumerate() {
                summed_worker_results[index].add(&worker_summary.results);
                merge_counter_values(&mut summed_worker_counters[index], &worker_summary.counters);
                all_worker_results[index].push(worker_summary.results.clone());
            }

            summed_results.add(&sample_result.results);
            all_results.push(sample_result.results);

            if sample % 2 == 0 || sample == config.target_samples - 1 {
                update_progress_bar(sample + 1, config.target_samples, running_throughput);
            }
        }

        clear_line();
        println!("  samples complete: {}", config.target_samples);

        let stats =
            benchmark_stats_from_samples(&summed_results, &all_results, config.target_samples);
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
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: BenchContext> BenchmarkGroup<'a, T> {
    pub fn bench(&self, name: &str, f: BenchFunction<T>) {
        self.runner.run(name, self.name, f);
    }

    pub fn bench_with_factory<F: Fn() -> T + ?Sized>(
        &self,
        name: &str,
        factory: &F,
        f: BenchFunction<T>,
    ) {
        self.runner.run_with_factory(name, self.name, f, factory);
    }
}

pub struct ConcurrentBenchmarkGroup<'a, T: ConcurrentBenchContext> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: ConcurrentBenchContext> ConcurrentBenchmarkGroup<'a, T>
where
    T: Send + Sync,
{
    pub fn bench(&self, name: &str, sample_duration: Duration, workers: &[ConcurrentWorker<T>]) {
        self.runner
            .run_concurrent(name, self.name, sample_duration, workers);
    }

    pub fn bench_with_factory<F: Fn(usize) -> T + ?Sized>(
        &self,
        name: &str,
        sample_duration: Duration,
        workers: &[ConcurrentWorker<T>],
        factory: &F,
    ) {
        self.runner
            .run_concurrent_with_factory(name, self.name, sample_duration, workers, factory);
    }
}

#[cfg(test)]
mod tests {
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
}
