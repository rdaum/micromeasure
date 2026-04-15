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

use crate::{Alignment, BenchmarkKind, BenchmarkReport, BenchmarkResult, TableFormatter};
use crate::session::BenchmarkSession;
use crate::threading::{
    DetectionResult, detect_performance_cores, pin_current_thread_to_core,
};
use std::time::Instant;
use std::{
    hint::black_box,
    io::{self, IsTerminal, Write},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

#[cfg(target_os = "linux")]
use perf_event::{Builder, Group, events::Hardware};
#[cfg(target_os = "linux")]
use std::sync::{Mutex, OnceLock};

const MIN_CHUNK_SIZE: usize = 100_000; // Large enough for reliable timing
const MAX_CHUNK_SIZE: usize = 50_000_000; // Maximum reasonable chunk
const TARGET_CHUNK_DURATION: Duration = Duration::from_millis(50); // Target 50ms per chunk for accurate timing
const WARM_UP_DURATION: Duration = Duration::from_secs(1); // 1 second warm-up
const MIN_BENCHMARK_DURATION: Duration = Duration::from_secs(5); // At least 5 seconds of actual benchmarking
const MIN_SAMPLES: usize = 20; // More samples for better statistics
const MAX_SAMPLES: usize = 100; // Increased upper bound for better statistics
const MIN_PMU_ACTIVE_PERCENT: f64 = 90.0; // Slightly more relaxed but still strict

type BenchFunction<T> = fn(&mut T, usize, usize);

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

fn colorize_label(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }
    let color = if text.contains("Throughput") {
        "32"
    } else if text.contains("Latency") || text == "P95" || text == "MAD" {
        "33"
    } else {
        "36"
    };
    format!("\x1b[{color}m{text}\x1b[0m")
}

fn colorize_value(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }
    format!("\x1b[97m{text}\x1b[0m")
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

fn coefficient_of_variation_percent(samples: &[Results]) -> f64 {
    let throughputs: Vec<f64> = samples.iter().filter_map(throughput_ops_per_sec).collect();
    if throughputs.is_empty() {
        return 0.0;
    }

    let mean = throughputs.iter().sum::<f64>() / throughputs.len() as f64;
    if mean <= f64::EPSILON || !mean.is_finite() {
        return 0.0;
    }

    let variance = throughputs
        .iter()
        .map(|&throughput| (throughput - mean).powi(2))
        .sum::<f64>()
        / throughputs.len() as f64;

    if !variance.is_finite() || variance < 0.0 {
        return 0.0;
    }

    (variance.sqrt() / mean) * 100.0
}

fn sample_mops_per_sec(samples: &[Results]) -> Vec<f64> {
    samples.iter().filter_map(throughput_ops_per_sec).map(|v| v / 1_000_000.0).collect()
}

fn sample_ns_per_op(samples: &[Results]) -> Vec<f64> {
    samples
        .iter()
        .filter_map(|sample| {
            if sample.iterations == 0 {
                return None;
            }
            let ns = safe_ratio_f64(sample.duration.as_nanos() as f64, sample.iterations as f64);
            if ns.is_finite() { Some(ns) } else { None }
        })
        .collect()
}

fn percentile(sorted_values: &[f64], percentile: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }

    let percentile = percentile.clamp(0.0, 1.0);
    let last_index = sorted_values.len() - 1;
    let position = percentile * last_index as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        return sorted_values[lower];
    }

    let weight = position - lower as f64;
    sorted_values[lower] * (1.0 - weight) + sorted_values[upper] * weight
}

fn median(sorted_values: &[f64]) -> f64 {
    percentile(sorted_values, 0.5)
}

fn median_absolute_deviation(values: &[f64], median_value: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    let mut deviations: Vec<f64> = values.iter().map(|value| (value - median_value).abs()).collect();
    deviations.sort_by(|a, b| a.total_cmp(b));
    median(&deviations)
}

fn tukey_outlier_count(sorted_values: &[f64]) -> usize {
    if sorted_values.len() < 4 {
        return 0;
    }

    let q1 = percentile(sorted_values, 0.25);
    let q3 = percentile(sorted_values, 0.75);
    let iqr = q3 - q1;
    let lower = q1 - 1.5 * iqr;
    let upper = q3 + 1.5 * iqr;
    sorted_values
        .iter()
        .filter(|value| **value < lower || **value > upper)
        .count()
}

fn scale_multiplexed_count(raw: u64, enabled_ns: u64, running_ns: u64) -> u64 {
    if raw == 0 {
        return 0;
    }

    if enabled_ns == 0 || running_ns == 0 {
        return raw;
    }

    if running_ns >= enabled_ns {
        return raw;
    }

    ((raw as u128 * enabled_ns as u128) / running_ns as u128).min(u64::MAX as u128) as u64
}

fn pmu_active_percent(results: &Results) -> f64 {
    safe_ratio_f64(
        results.pmu_time_running_ns as f64,
        results.pmu_time_enabled_ns as f64,
    ) * 100.0
}

fn enforce_pmu_quality(name: &str, has_perf_counters: bool, results: &Results) {
    if !has_perf_counters || results.pmu_time_enabled_ns == 0 || results.pmu_time_running_ns == 0 {
        return;
    }

    let active_percent = pmu_active_percent(results);
    if active_percent < MIN_PMU_ACTIVE_PERCENT {
        // Log warning instead of panic by default, or maybe make it configurable later.
        // For now, let's keep it as a warning to avoid breaking CI unexpectedly during refactor.
        eprintln!(
            "⚠️ PMU counters were multiplexed too heavily for benchmark '{name}': active {active_percent:.1}% < {MIN_PMU_ACTIVE_PERCENT:.1}%"
        );
    }
}

#[cfg(target_os = "linux")]
fn perf_issues() -> &'static Mutex<Vec<String>> {
    static PERF_ISSUES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();
    PERF_ISSUES.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(target_os = "linux")]
fn record_perf_issue(message: impl Into<String>) {
    let message = message.into();
    let lock = perf_issues().lock();
    let mut issues = match lock {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };

    if issues.iter().any(|existing| existing == &message) {
        return;
    }
    if issues.len() >= 6 {
        return;
    }
    issues.push(message);
}

#[cfg(target_os = "linux")]
fn clear_perf_issues() {
    let lock = perf_issues().lock();
    let mut issues = match lock {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    issues.clear();
}

#[cfg(target_os = "linux")]
fn current_perf_issues() -> Vec<String> {
    let lock = perf_issues().lock();
    match lock {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

#[cfg(target_os = "linux")]
fn linux_perf_hint(has_perf_counters: bool, issues: &[String]) -> Option<String> {
    if has_perf_counters {
        return None;
    }

    let looks_like_perf_access_issue = issues.iter().any(|issue| {
        issue.contains("unusable timing window")
            || issue.contains("Operation not permitted")
            || issue.contains("Permission denied")
    });
    if !looks_like_perf_access_issue {
        return None;
    }

    let paranoid = std::fs::read_to_string("/proc/sys/kernel/perf_event_paranoid")
        .ok()
        .and_then(|value| value.trim().parse::<i32>().ok());

    match paranoid {
        Some(value) if value > 2 => Some(format!(
            "kernel.perf_event_paranoid={value}; lower it to 2 or less (or grant CAP_PERFMON/CAP_SYS_ADMIN) to enable PMU counters"
        )),
        Some(value) => Some(format!(
            "kernel.perf_event_paranoid={value}; PMU still unavailable, likely due to missing CAP_PERFMON/CAP_SYS_ADMIN or container perf_event restrictions"
        )),
        None => Some(
            "PMU still unavailable; check /proc/sys/kernel/perf_event_paranoid and container capabilities (CAP_PERFMON/CAP_SYS_ADMIN)".to_string(),
        ),
    }
}

fn warn_affinity_once(message: impl Into<String>) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        eprintln!("⚠️  {}", message.into());
    }
}

#[cfg(target_os = "linux")]
fn core_has_usable_pmu(core_id: usize) -> bool {
    if pin_current_thread_to_core(core_id).is_err() {
        return false;
    }

    let mut counter = match Builder::new(Hardware::INSTRUCTIONS).build() {
        Ok(counter) => counter,
        Err(_) => return false,
    };
    if counter.enable().is_err() {
        return false;
    }

    let mut acc = 0_u64;
    for i in 0..100_000 {
        acc = acc.wrapping_add(i);
    }
    black_box(acc);

    let _ = counter.disable();
    match counter.read_count_and_time() {
        Ok(cat) => cat.count > 0 || cat.time_running > 0,
        Err(_) => false,
    }
}

#[cfg(target_os = "linux")]
fn choose_default_pin_core(allowed_core_ids: &[usize]) -> Option<usize> {
    let mut candidates: Vec<usize> = Vec::new();
    if let Ok(DetectionResult::PerformanceCores(selection)) = detect_performance_cores() {
        for core_id in selection.logical_processor_ids {
            if allowed_core_ids.contains(&core_id) && !candidates.contains(&core_id) {
                candidates.push(core_id);
            }
        }
    }
    for core_id in allowed_core_ids {
        if !candidates.contains(core_id) {
            candidates.push(*core_id);
        }
    }

    for core_id in &candidates {
        if core_has_usable_pmu(*core_id) {
            return Some(*core_id);
        }
    }

    candidates.first().copied()
}

#[cfg(target_os = "linux")]
fn capture_current_thread_affinity() -> io::Result<libc::cpu_set_t> {
    // SAFETY: zeroed is valid initialization for cpu_set_t.
    let mut cpuset: libc::cpu_set_t = unsafe { std::mem::zeroed() };
    // SAFETY: pthread_self returns a valid handle for current thread; cpuset pointer is valid.
    let result = unsafe {
        libc::pthread_getaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            &mut cpuset,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    Ok(cpuset)
}

#[cfg(target_os = "linux")]
fn restore_current_thread_affinity(mask: &libc::cpu_set_t) -> io::Result<()> {
    // SAFETY: pthread_self returns a valid handle; mask pointer is valid.
    let result = unsafe {
        libc::pthread_setaffinity_np(
            libc::pthread_self(),
            std::mem::size_of::<libc::cpu_set_t>(),
            mask,
        )
    };
    if result != 0 {
        return Err(io::Error::from_raw_os_error(result));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn core_ids_from_mask(mask: &libc::cpu_set_t) -> Vec<usize> {
    let mut core_ids = Vec::new();
    for core_id in 0..(libc::CPU_SETSIZE as usize) {
        // SAFETY: core_id is in range and mask points to a valid cpu_set_t.
        let is_set = unsafe { libc::CPU_ISSET(core_id, mask) };
        if is_set {
            core_ids.push(core_id);
        }
    }
    core_ids
}

struct BenchAffinityGuard {
    #[cfg(target_os = "linux")]
    restore_mask: Option<libc::cpu_set_t>,
    #[cfg(target_os = "linux")]
    did_pin: bool,
}

impl BenchAffinityGuard {
    fn acquire() -> Self {
        #[cfg(target_os = "linux")]
        {
            let restore_mask = match capture_current_thread_affinity() {
                Ok(mask) => Some(mask),
                Err(error) => {
                    warn_affinity_once(format!(
                        "Could not capture existing benchmark thread affinity: {error}. Continuing with best effort pinning"
                    ));
                    None
                }
            };

            let allowed_core_ids = restore_mask
                .as_ref()
                .map(core_ids_from_mask)
                .filter(|core_ids| !core_ids.is_empty())
                .unwrap_or_else(|| {
                    let count = std::thread::available_parallelism()
                        .map(|p| p.get())
                        .unwrap_or(1);
                    (0..count).collect()
                });

            let requested_core = std::env::var("BENCH_UTILS_PIN_CORE")
                .ok()
                .and_then(|value| value.parse::<usize>().ok());
            let core_to_pin = requested_core
                .filter(|core_id| allowed_core_ids.contains(core_id))
                .or_else(|| choose_default_pin_core(&allowed_core_ids));

            let Some(core_id) = core_to_pin else {
                warn_affinity_once(
                    "No logical cores detected for pinning; benchmark will run without CPU pinning",
                );
                return Self {
                    restore_mask: None,
                    did_pin: false,
                };
            };

            if let Err(error) = pin_current_thread_to_core(core_id) {
                warn_affinity_once(format!(
                    "Could not pin benchmark thread to core {core_id}: {error}. Continuing without pinning"
                ));
                return Self {
                    restore_mask: None,
                    did_pin: false,
                };
            }

            Self {
                restore_mask,
                did_pin: true,
            }
        }

        #[cfg(not(target_os = "linux"))]
        {
            Self {}
        }
    }
}

impl Drop for BenchAffinityGuard {
    fn drop(&mut self) {
        #[cfg(target_os = "linux")]
        {
            if !self.did_pin {
                return;
            }

            let Some(mask) = &self.restore_mask else {
                return;
            };

            if let Err(error) = restore_current_thread_affinity(mask) {
                warn_affinity_once(format!(
                    "Could not restore benchmark thread affinity after run: {error}"
                ));
            }
        }
    }
}

#[cfg(target_os = "linux")]
struct PerfGroupCounters {
    group: Group,
    instructions: Option<perf_event::Counter>,
    branches: Option<perf_event::Counter>,
    branch_misses: Option<perf_event::Counter>,
    cache_misses: Option<perf_event::Counter>,
}

#[cfg(target_os = "linux")]
fn try_add_group_counter(
    group: &mut Group,
    event: Hardware,
    name: &str,
) -> Option<perf_event::Counter> {
    match group.add(&Builder::new(event)) {
        Ok(counter) => Some(counter),
        Err(error) => {
            record_perf_issue(format!("perf event '{name}' unavailable: {error}"));
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn build_perf_counter_group() -> Option<PerfGroupCounters> {
    let mut group = match Group::new() {
        Ok(group) => group,
        Err(error) => {
            record_perf_issue(format!("perf group unavailable: {error}"));
            return None;
        }
    };

    let instructions = try_add_group_counter(&mut group, Hardware::INSTRUCTIONS, "instructions");
    let branches = try_add_group_counter(&mut group, Hardware::BRANCH_INSTRUCTIONS, "branches");
    let branch_misses = try_add_group_counter(&mut group, Hardware::BRANCH_MISSES, "branch-misses");
    let cache_misses = try_add_group_counter(&mut group, Hardware::CACHE_MISSES, "cache-misses");

    if instructions.is_none()
        && branches.is_none()
        && branch_misses.is_none()
        && cache_misses.is_none()
    {
        record_perf_issue("no perf events could be added to perf group".to_string());
        return None;
    }

    Some(PerfGroupCounters {
        group,
        instructions,
        branches,
        branch_misses,
        cache_misses,
    })
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
        if divisor > 0 {
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
}

pub struct BenchmarkConfig {
    pub chunk_size: usize,
    pub target_samples: usize,
    pub estimated_ops_per_sec: f64,
}

/// Performance counter controls for fine-grained measurement
#[cfg(target_os = "linux")]
pub struct PerfCounters {
    pub instructions_counter: perf_event::Counter,
    pub cycles_counter: perf_event::Counter,
    pub branch_counter: perf_event::Counter,
    pub branch_misses: perf_event::Counter,
    pub cache_misses: perf_event::Counter,
    pub l1i_misses: perf_event::Counter,
    pub stalled_frontend: perf_event::Counter,
    pub stalled_backend: perf_event::Counter,
    pub start_time: Option<Instant>,
}

#[cfg(target_os = "linux")]
impl Default for PerfCounters {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl PerfCounters {
    pub fn try_new() -> io::Result<Self> {
        Ok(PerfCounters {
            instructions_counter: Builder::new(Hardware::INSTRUCTIONS).build()?,
            cycles_counter: Builder::new(Hardware::CPU_CYCLES).build()?,
            branch_counter: Builder::new(Hardware::BRANCH_INSTRUCTIONS).build()?,
            branch_misses: Builder::new(Hardware::BRANCH_MISSES).build()?,
            cache_misses: Builder::new(Hardware::CACHE_MISSES).build()?,
            l1i_misses: Builder::new(Hardware::CACHE_MISSES).build()?, // Use generic as fallback
            stalled_frontend: Builder::new(Hardware::CACHE_MISSES).build()?, // Use generic as fallback
            stalled_backend: Builder::new(Hardware::CACHE_MISSES).build()?, // Use generic as fallback
            start_time: None,
        })
    }

    pub fn new() -> Self {
        Self::try_new().expect("failed to initialize perf counters")
    }

    pub fn start(&mut self) {
        self.start_time = Some(Instant::now());
        let _ = self.instructions_counter.enable();
        let _ = self.cycles_counter.enable();
        let _ = self.branch_counter.enable();
        let _ = self.branch_misses.enable();
        let _ = self.cache_misses.enable();
        let _ = self.l1i_misses.enable();
        let _ = self.stalled_frontend.enable();
        let _ = self.stalled_backend.enable();
    }

    pub fn stop(&mut self) -> (Duration, u64, u64, u64, u64, u64, u64, u64, u64) {
        let _ = self.instructions_counter.disable();
        let _ = self.cycles_counter.disable();
        let _ = self.branch_counter.disable();
        let _ = self.branch_misses.disable();
        let _ = self.cache_misses.disable();
        let _ = self.l1i_misses.disable();
        let _ = self.stalled_frontend.disable();
        let _ = self.stalled_backend.disable();

        let duration = self
            .start_time
            .map_or(Duration::from_secs(0), |start| start.elapsed());
        let instructions = self.instructions_counter.read().unwrap_or(0);
        let cycles = self.cycles_counter.read().unwrap_or(0);
        let branches = self.branch_counter.read().unwrap_or(0);
        let branch_misses = self.branch_misses.read().unwrap_or(0);
        let cache_misses = self.cache_misses.read().unwrap_or(0);
        let l1i_misses = self.l1i_misses.read().unwrap_or(0);
        let stalled_frontend = self.stalled_frontend.read().unwrap_or(0);
        let stalled_backend = self.stalled_backend.read().unwrap_or(0);

        (
            duration,
            instructions,
            cycles,
            branches,
            branch_misses,
            cache_misses,
            l1i_misses,
            stalled_frontend,
            stalled_backend,
        )
    }
}

/// Generic benchmark context that can hold any preparation data
pub trait BenchContext {
    fn prepare(num_chunks: usize) -> Self;

    /// Optional preferred chunk size for this context
    /// If Some(size), skip calibration and use this size
    /// If None, use normal calibration
    fn chunk_size() -> Option<usize> {
        None
    }

    /// Optional: specify how many actual operations each chunk represents
    /// Used for calculating correct throughput metrics
    /// If None, assumes chunk_size == operations
    fn operations_per_chunk() -> Option<u64> {
        None
    }
}

/// Simple context for benchmarks that don't need preparation
pub struct NoContext;
impl BenchContext for NoContext {
    fn prepare(_num_chunks: usize) -> Self {
        NoContext
    }
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
            let new_chunk_size = ((chunk_size as f64) * scaling_factor) as usize;
            chunk_size = new_chunk_size.clamp(MIN_CHUNK_SIZE, MAX_CHUNK_SIZE);
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

fn execute_timing_only<T: BenchContext>(
    f: &BenchFunction<T>,
    prepared: &mut T,
    chunk_size: usize,
    chunk_num: usize,
    ops: u64,
) -> Results {
    let start_time = Instant::now();
    black_box(|| f(prepared, chunk_size, chunk_num))();
    let duration = start_time.elapsed();

    Results {
        duration,
        iterations: ops,
        chunks_executed: 1,
        ..Results::default()
    }
}

#[cfg(target_os = "linux")]
fn try_build_individual_counter(event: Hardware, name: &str) -> Option<perf_event::Counter> {
    match Builder::new(event).build() {
        Ok(counter) => Some(counter),
        Err(error) => {
            record_perf_issue(format!("perf event '{name}' unavailable: {error}"));
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn read_scaled_counter(counter: &mut Option<perf_event::Counter>, name: &str) -> (u64, u64, u64) {
    let Some(counter) = counter else {
        return (0, 0, 0);
    };

    match counter.read_count_and_time() {
        Ok(cat) => {
            if cat.count > 0 && (cat.time_enabled == 0 || cat.time_running == 0) {
                record_perf_issue(format!(
                    "perf event '{name}' missing timing metadata (enabled/running); using raw count"
                ));
            }
            (
                scale_multiplexed_count(cat.count, cat.time_enabled, cat.time_running),
                cat.time_enabled,
                cat.time_running,
            )
        }
        Err(error) => {
            record_perf_issue(format!("perf event '{name}' read failed: {error}"));
            (0, 0, 0)
        }
    }
}

#[cfg(target_os = "linux")]
fn enable_counter(counter: &mut Option<perf_event::Counter>, name: &str) {
    let Some(mut inner) = counter.take() else {
        return;
    };

    if let Err(error) = inner.enable() {
        record_perf_issue(format!("perf event '{name}' enable failed: {error}"));
        return;
    }

    *counter = Some(inner);
}

#[cfg(target_os = "linux")]
fn disable_counter(counter: &mut Option<perf_event::Counter>, name: &str) {
    let Some(counter) = counter.as_mut() else {
        return;
    };

    if let Err(error) = counter.disable() {
        record_perf_issue(format!("perf event '{name}' disable failed: {error}"));
    }
}

#[cfg(target_os = "linux")]
fn execute_with_individual_counters<T: BenchContext>(
    f: &BenchFunction<T>,
    prepared: &mut T,
    chunk_size: usize,
    chunk_num: usize,
    ops: u64,
) -> Results {
    let mut instructions_counter =
        try_build_individual_counter(Hardware::INSTRUCTIONS, "instructions");
    let mut branches_counter =
        try_build_individual_counter(Hardware::BRANCH_INSTRUCTIONS, "branches");
    let mut branch_misses_counter =
        try_build_individual_counter(Hardware::BRANCH_MISSES, "branch-misses");
    let mut cache_misses_counter =
        try_build_individual_counter(Hardware::CACHE_MISSES, "cache-misses");

    if instructions_counter.is_none()
        && branches_counter.is_none()
        && branch_misses_counter.is_none()
        && cache_misses_counter.is_none()
    {
        return execute_timing_only(f, prepared, chunk_size, chunk_num, ops);
    }

    record_perf_issue("using ungrouped perf counters fallback".to_string());

    enable_counter(&mut instructions_counter, "instructions");
    enable_counter(&mut branches_counter, "branches");
    enable_counter(&mut branch_misses_counter, "branch-misses");
    enable_counter(&mut cache_misses_counter, "cache-misses");

    let start_time = Instant::now();
    black_box(|| f(prepared, chunk_size, chunk_num))();
    let duration = start_time.elapsed();

    disable_counter(&mut instructions_counter, "instructions");
    disable_counter(&mut branches_counter, "branches");
    disable_counter(&mut branch_misses_counter, "branch-misses");
    disable_counter(&mut cache_misses_counter, "cache-misses");

    let (instructions, instructions_enabled, instructions_running) =
        read_scaled_counter(&mut instructions_counter, "instructions");
    let (branches, branches_enabled, branches_running) =
        read_scaled_counter(&mut branches_counter, "branches");
    let (branch_misses, branch_misses_enabled, branch_misses_running) =
        read_scaled_counter(&mut branch_misses_counter, "branch-misses");
    let (cache_misses, cache_misses_enabled, cache_misses_running) =
        read_scaled_counter(&mut cache_misses_counter, "cache-misses");

    let timing_candidates = [
        (instructions_enabled, instructions_running),
        (branches_enabled, branches_running),
        (branch_misses_enabled, branch_misses_running),
        (cache_misses_enabled, cache_misses_running),
    ];

    let (pmu_time_enabled_ns, pmu_time_running_ns) = timing_candidates
        .iter()
        .copied()
        .find(|(_, running)| *running > 0)
        .or_else(|| {
            timing_candidates
                .iter()
                .copied()
                .find(|(enabled, _)| *enabled > 0)
        })
        .unwrap_or((0, 0));

    Results {
        instructions,
        branches,
        branch_misses,
        cache_misses,
        has_instructions: instructions_counter.is_some(),
        has_branches: branches_counter.is_some(),
        has_branch_misses: branch_misses_counter.is_some(),
        has_cache_misses: cache_misses_counter.is_some(),
        pmu_time_enabled_ns,
        pmu_time_running_ns,
        duration,
        iterations: ops,
        chunks_executed: 1,
    }
}

#[cfg(target_os = "linux")]
fn execute_with_perf_group<T: BenchContext>(
    f: &BenchFunction<T>,
    prepared: &mut T,
    chunk_size: usize,
    chunk_num: usize,
    ops: u64,
) -> Results {
    let Some(mut perf) = build_perf_counter_group() else {
        return execute_with_individual_counters(f, prepared, chunk_size, chunk_num, ops);
    };

    if let Err(error) = perf.group.enable() {
        record_perf_issue(format!("perf group enable failed: {error}"));
        return execute_with_individual_counters(f, prepared, chunk_size, chunk_num, ops);
    }

    let start_time = Instant::now();
    black_box(|| f(prepared, chunk_size, chunk_num))();
    let duration = start_time.elapsed();
    if let Err(error) = perf.group.disable() {
        record_perf_issue(format!("perf group disable failed: {error}"));
    }

    let counts = match perf.group.read() {
        Ok(counts) => counts,
        Err(error) => {
            record_perf_issue(format!("perf group read failed: {error}"));
            return execute_with_individual_counters(f, prepared, chunk_size, chunk_num, ops);
        }
    };

    let enabled_ns = counts
        .time_enabled()
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0);
    let running_ns = counts
        .time_running()
        .map(|duration| duration.as_nanos().min(u64::MAX as u128) as u64)
        .unwrap_or(0);

    let instructions_raw = perf
        .instructions
        .as_ref()
        .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
        .unwrap_or(0);
    let branches_raw = perf
        .branches
        .as_ref()
        .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
        .unwrap_or(0);
    let branch_misses_raw = perf
        .branch_misses
        .as_ref()
        .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
        .unwrap_or(0);
    let cache_misses_raw = perf
        .cache_misses
        .as_ref()
        .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
        .unwrap_or(0);

    if enabled_ns == 0 || running_ns == 0 {
        record_perf_issue(
            "perf counters reported unusable timing window (enabled/running)".to_string(),
        );
        if instructions_raw == 0
            && branches_raw == 0
            && branch_misses_raw == 0
            && cache_misses_raw == 0
        {
            return execute_with_individual_counters(f, prepared, chunk_size, chunk_num, ops);
        }
    }

    Results {
        instructions: scale_multiplexed_count(instructions_raw, enabled_ns, running_ns),
        branches: scale_multiplexed_count(branches_raw, enabled_ns, running_ns),
        branch_misses: scale_multiplexed_count(branch_misses_raw, enabled_ns, running_ns),
        cache_misses: scale_multiplexed_count(cache_misses_raw, enabled_ns, running_ns),
        has_instructions: perf.instructions.is_some(),
        has_branches: perf.branches.is_some(),
        has_branch_misses: perf.branch_misses.is_some(),
        has_cache_misses: perf.cache_misses.is_some(),
        pmu_time_enabled_ns: enabled_ns,
        pmu_time_running_ns: running_ns,
        duration,
        iterations: ops,
        chunks_executed: 1,
    }
}

/// Progress bar with terminal-compatible characters
fn update_progress_bar(current: usize, total: usize, current_throughput: f64) {
    let width = 40;
    let filled = (current * width / total.max(1)).min(width);
    let empty = width - filled;

    let percentage = (current * 100 / total.max(1)).min(100);

    print!("\r\x1b[2K⚡ running [");

    // Progress bar with ASCII-compatible characters
    for i in 0..filled {
        if i == filled - 1 && current < total {
            print!(">"); // Current position
        } else {
            print!("="); // Completed
        }
    }

    for _ in 0..empty {
        print!(" "); // Empty
    }

    // Display throughput with proper bounds checking
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

    /// Override the persisted suite name used for saved results and comparisons.
    pub fn with_suite(mut self, suite: impl Into<String>) -> Self {
        self.session = std::sync::Arc::new(BenchmarkSession::new_with_suite(suite));
        self
    }

    pub fn with_filter(mut self, filter: Option<&str>) -> Self {
        self.filter = filter.map(|s| s.to_string());
        self
    }

    /// Run a group of benchmarks with a common context type
    pub fn group<T: BenchContext>(&self, name: &'static str, f: impl FnOnce(&BenchmarkGroup<T>)) {
        let group = BenchmarkGroup {
            runner: self,
            name,
            _marker: std::marker::PhantomData,
        };
        f(&group);
    }

    pub fn report(&self) -> BenchmarkReport {
        self.session.report()
    }

    /// Return the collected benchmark results for this runner.
    pub fn results(&self) -> Vec<BenchmarkResult> {
        self.session.get_results()
    }

    fn should_run(&self, name: &str, group: &str) -> bool {
        let Some(filter) = &self.filter else { return true };
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

        // Warm-up and calibration phase
        let config = calibrate_engine(&f, factory);
        let estimated_mops = if config.estimated_ops_per_sec > 0.0 {
            format!("{:.2} Mops/s", config.estimated_ops_per_sec / 1_000_000.0)
        } else {
            "n/a".to_string()
        };
        println!(
            "  calibrated: chunk={} samples={} estimate={estimated_mops}",
            config.chunk_size, config.target_samples
        );

        // Main benchmark phase
        rewrite_line(&format!("⚡ running 0/{} samples", config.target_samples));

        let mut all_results: Vec<Results> = Vec::new();
        let mut summed_results = Results::default();
        let mut running_throughput = if config.estimated_ops_per_sec > 0.0 {
            config.estimated_ops_per_sec / 1_000_000.0
        } else {
            0.0
        };

        for sample in 0..config.target_samples {
            let mut prepared = factory();
            let ops = T::operations_per_chunk().unwrap_or(config.chunk_size as u64);

            #[cfg(target_os = "linux")]
            let sample_result = execute_with_perf_group(&f, &mut prepared, config.chunk_size, sample, ops);
            #[cfg(not(target_os = "linux"))]
            let sample_result = execute_timing_only(&f, &mut prepared, config.chunk_size, sample, ops);

            let duration_secs = sample_result.duration.as_secs_f64();
            let sample_throughput_mops =
                safe_ratio_f64(sample_result.iterations as f64, duration_secs) / 1_000_000.0;
            if sample_throughput_mops > 0.0 {
                running_throughput = running_throughput * 0.9 + sample_throughput_mops * 0.1;
            }

            summed_results.add(&sample_result);
            all_results.push(sample_result);

            if sample % 2 == 0 || sample == config.target_samples - 1 {
                update_progress_bar(sample + 1, config.target_samples, running_throughput);
            }
        }

        clear_line();
        println!("  samples complete: {}", config.target_samples);

        // Calculate statistics
        let mut results = summed_results.clone();
        results.divide(config.target_samples as u64);

        let ops_per_sec = safe_ratio_f64(results.iterations as f64, results.duration.as_secs_f64());
        let ns_per_op = safe_ratio_f64(
            results.duration.as_nanos() as f64,
            results.iterations as f64,
        );
        let instructions_per_op =
            safe_ratio_f64(results.instructions as f64, results.iterations as f64);
        let branches_per_op = safe_ratio_f64(results.branches as f64, results.iterations as f64);
        let branch_miss_rate =
            safe_ratio_f64(results.branch_misses as f64, results.branches as f64) * 100.0;
        let branch_misses_per_op =
            safe_ratio_f64(results.branch_misses as f64, results.iterations as f64);
        let cache_miss_rate_per_op =
            safe_ratio_f64(results.cache_misses as f64, results.iterations as f64);
        let cv_percent = coefficient_of_variation_percent(&all_results);
        let mut throughput_samples = sample_mops_per_sec(&all_results);
        throughput_samples.sort_by(|a, b| a.total_cmp(b));
        let median_mops_per_sec = median(&throughput_samples);
        let mut latency_samples = sample_ns_per_op(&all_results);
        latency_samples.sort_by(|a, b| a.total_cmp(b));
        let median_ns_per_op = median(&latency_samples);
        let p95_ns_per_op = percentile(&latency_samples, 0.95);
        let mad_ns_per_op = median_absolute_deviation(&latency_samples, median_ns_per_op);
        let outlier_count = tukey_outlier_count(&latency_samples);

        println!("  results:");

        let has_perf_counters = results.has_instructions
            || results.has_branches
            || results.has_branch_misses
            || results.has_cache_misses;
        let has_full_perf_counters = results.has_instructions
            && results.has_branches
            && results.has_branch_misses
            && results.has_cache_misses;

        #[cfg(target_os = "linux")]
        {
            let issues = current_perf_issues();
            if !has_perf_counters {
                warn_perf_unavailable_once(&issues);
            } else if !has_full_perf_counters {
                warn_partial_perf_once();
            }
        }
        #[cfg(not(target_os = "linux"))]
        if !has_perf_counters {
            warn_perf_unavailable_once_non_linux();
        }

        enforce_pmu_quality(name, has_perf_counters, &results);

        let mut table = TableFormatter::new(
            vec!["Stat", "Value", "Stat", "Value"],
            vec![22, 18, 22, 18],
        )
        .with_alignments(vec![
            Alignment::Left,
            Alignment::Right,
            Alignment::Left,
            Alignment::Right,
        ])
        .with_group_split_after(1);

        table.add_row(vec![
            &colorize_label("Throughput"),
            &colorize_value(&format!("{:.2} Mops/s", ops_per_sec / 1_000_000.0)),
            &colorize_label("Median Throughput"),
            &colorize_value(&format!("{median_mops_per_sec:.2} Mops/s")),
        ]);

        table.add_row(vec![
            &colorize_label("Mean Latency"),
            &colorize_value(&format!("{ns_per_op:.2} ns/op")),
            &colorize_label("Median Latency"),
            &colorize_value(&format!("{median_ns_per_op:.2} ns/op")),
        ]);

        table.add_row(vec![
            &colorize_label("P95 Latency"),
            &colorize_value(&format!("{p95_ns_per_op:.2} ns/op")),
            &colorize_label("MAD Latency"),
            &colorize_value(&format!("{mad_ns_per_op:.2} ns/op")),
        ]);

        table.add_row(vec![
            &colorize_label("Samples"),
            &colorize_value(&format!("{}", config.target_samples)),
            &colorize_label("Outliers"),
            &colorize_value(&format!("{outlier_count}")),
        ]);

        table.add_row(vec![
            &colorize_label("Operations"),
            &colorize_value(&format!("{}", results.iterations)),
            &colorize_label("Total Duration"),
            &colorize_value(&format!("{:.3}s", summed_results.duration.as_secs_f64())),
        ]);

        table.add_row(vec![
            &colorize_label("Coefficient Var."),
            &colorize_value(&format!("{cv_percent:.2}%")),
            &colorize_label("Measurement"),
            &colorize_value(if has_perf_counters { "timing + PMU" } else { "timing only" }),
        ]);

        let mut pmu_byline = None;
        if has_perf_counters {
            let active_percent = pmu_active_percent(&results);
            let pmu_avg_running_sec = results.pmu_time_running_ns as f64 / 1_000_000_000.0;
            let pmu_avg_enabled_sec = results.pmu_time_enabled_ns as f64 / 1_000_000_000.0;
            let pmu_total_running_sec = summed_results.pmu_time_running_ns as f64 / 1_000_000_000.0;
            let pmu_total_enabled_sec = summed_results.pmu_time_enabled_ns as f64 / 1_000_000_000.0;
            if results.has_instructions {
                let branches_value = if results.has_branches {
                    format!("{branches_per_op:.1}")
                } else {
                    String::new()
                };
                table.add_row(vec![
                    &colorize_label("Instructions / op"),
                    &colorize_value(&format!("{instructions_per_op:.1}")),
                    &colorize_label("Branches / op"),
                    &colorize_value(&branches_value),
                ]);
            } else if results.has_branches {
                table.add_row(vec![
                    &colorize_label("Branches / op"),
                    &colorize_value(&format!("{branches_per_op:.1}")),
                    &colorize_label("Branch Count"),
                    &colorize_value(&format!("{:.1}M", results.branches as f64 / 1_000_000.0)),
                ]);
            }

            if results.has_branches && results.has_branch_misses {
                table.add_row(vec![
                    &colorize_label("Branch Miss Rate"),
                    &colorize_value(&format!("{branch_miss_rate:.4}%")),
                    &colorize_label("Branch Misses / op"),
                    &colorize_value(&format!("{branch_misses_per_op:.4}")),
                ]);
            }

            if results.has_cache_misses {
                table.add_row(vec![
                    &colorize_label("Cache Misses / op"),
                    &colorize_value(&format!("{cache_miss_rate_per_op:.4}")),
                    "",
                    "",
                ]);
            }

            pmu_byline = Some(format!(
                "  PMU: coverage={} avg_running={:.3}s avg_enabled={:.3}s total_running={:.3}s total_enabled={:.3}s",
                colorize_value(&format!("{active_percent:.1}%")),
                pmu_avg_running_sec,
                pmu_avg_enabled_sec,
                pmu_total_running_sec,
                pmu_total_enabled_sec,
            ));
        }

        table.print();
        if let Some(pmu_byline) = pmu_byline {
            println!("{pmu_byline}");
        }

        let benchmark_result = BenchmarkResult {
            name: name.to_string(),
            group: group.to_string(),
            kind: BenchmarkKind::Standard,
            mops_per_sec: ops_per_sec / 1_000_000.0,
            median_mops_per_sec,
            ns_per_op,
            median_ns_per_op,
            p95_ns_per_op,
            mad_ns_per_op,
            instructions_per_op,
            branches_per_op,
            branch_miss_rate,
            branch_misses_per_op,
            cache_miss_rate: cache_miss_rate_per_op,
            cv_percent,
            outlier_count,
            samples: config.target_samples,
            operations: results.iterations,
            total_duration_sec: summed_results.duration.as_secs_f64(),
            sample_throughput_mops_per_sec: throughput_samples,
            sample_latency_ns_per_op: latency_samples,
        };

        self.session.add_result(benchmark_result);
    }
}

pub struct BenchmarkGroup<'a, T: BenchContext> {
    runner: &'a BenchmarkRunner,
    name: &'static str,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: BenchContext> BenchmarkGroup<'a, T> {
    /// Add a benchmark to the group using default context preparation
    pub fn bench(&self, name: &str, f: BenchFunction<T>) {
        self.runner.run(name, self.name, f);
    }

    /// Add a benchmark with a custom context factory
    pub fn bench_with_factory<F: Fn() -> T + ?Sized>(&self, name: &str, factory: &F, f: BenchFunction<T>) {
        self.runner.run_with_factory(name, self.name, f, factory);
    }
}

#[cfg(target_os = "linux")]
fn warn_perf_unavailable_once(issues: &[String]) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }

    eprintln!("⚠️  PMU counters unavailable; continuing with timing-only results.");
    if let Some(hint) = linux_perf_hint(false, issues) {
        eprintln!("   {hint}");
    }
}

#[cfg(target_os = "linux")]
fn warn_partial_perf_once() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }

    eprintln!("⚠️  Some PMU counters are unavailable; omitted metrics will not be shown.");
}

#[cfg(not(target_os = "linux"))]
fn warn_perf_unavailable_once_non_linux() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }

    eprintln!("⚠️  PMU counters are unavailable on this platform; continuing with timing-only results.");
}

#[cfg(test)]
mod tests {
    use super::{median, median_absolute_deviation, percentile, tukey_outlier_count};

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
