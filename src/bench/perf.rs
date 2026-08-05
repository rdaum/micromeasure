use super::{
    ConcurrentBenchContext, ConcurrentBenchControl, ConcurrentWorkerMeasurement,
    ConcurrentWorkerResult, Results, safe_ratio_f64,
};
use crate::bench::backend::{MeasurementBackend, MetricValue};
use std::{
    collections::BTreeSet,
    fs, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use perf_event::events::{Cache, CacheId, CacheOp, CacheResult, Hardware};
#[cfg(target_os = "linux")]
use perf_event::{Builder, Group};
#[cfg(target_os = "linux")]
use std::sync::{Mutex, OnceLock};

const MIN_PMU_SCHEDULED_PERCENT: f64 = 90.0;

/// A reusable set of Linux thread IDs for targeted PMU measurement.
///
/// Register externally managed workers once (for example with
/// `rayon::ThreadPool::broadcast`) and pass the set to
/// [`LinuxPerfBackend::registered_threads`]. Stale thread IDs are ignored
/// when a sample opens its counters.
#[cfg(target_os = "linux")]
#[derive(Clone, Default)]
pub struct LinuxPerfThreadSet {
    tids: Arc<Mutex<BTreeSet<libc::pid_t>>>,
}

#[cfg(target_os = "linux")]
impl LinuxPerfThreadSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add the calling thread to this measurement set.
    pub fn register_current(&self) -> libc::pid_t {
        let tid = current_thread_id();
        let lock = self.tids.lock();
        let mut tids = match lock {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        tids.insert(tid);
        tid
    }

    /// Remove the calling thread from this measurement set.
    pub fn unregister_current(&self) -> bool {
        let tid = current_thread_id();
        let lock = self.tids.lock();
        let mut tids = match lock {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        tids.remove(&tid)
    }

    pub fn len(&self) -> usize {
        let lock = self.tids.lock();
        match lock {
            Ok(guard) => guard.len(),
            Err(poisoned) => poisoned.into_inner().len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn snapshot(&self) -> Vec<libc::pid_t> {
        let lock = self.tids.lock();
        match lock {
            Ok(guard) => guard.iter().copied().collect(),
            Err(poisoned) => poisoned.into_inner().iter().copied().collect(),
        }
    }
}

#[cfg(target_os = "linux")]
fn current_thread_id() -> libc::pid_t {
    // Linux gettid has no libc wrapper on all supported libc versions.
    unsafe { libc::syscall(libc::SYS_gettid) as libc::pid_t }
}

#[cfg(target_os = "linux")]
fn process_thread_ids() -> io::Result<Vec<libc::pid_t>> {
    let mut tids = Vec::new();
    for entry in fs::read_dir("/proc/self/task")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if let Ok(tid) = name.parse::<libc::pid_t>() {
            tids.push(tid);
        }
    }
    tids.sort_unstable();
    tids.dedup();
    Ok(tids)
}

#[cfg(target_os = "linux")]
#[derive(Clone)]
enum PerfScope {
    CurrentThread,
    ProcessThreads,
    RegisteredThreads(LinuxPerfThreadSet),
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
enum PerfTarget {
    CurrentThread,
    Thread(libc::pid_t),
}

#[cfg(target_os = "linux")]
impl PerfTarget {
    fn configure(self, builder: &mut Builder<'_>) {
        if let Self::Thread(tid) = self {
            builder.observe_pid(tid);
        }
    }
}

#[cfg(target_os = "linux")]
struct PerfGroupCounters {
    group: Group,
    cycles: Option<perf_event::Counter>,
    instructions: Option<perf_event::Counter>,
    cache_references: Option<perf_event::Counter>,
    l1i_misses: Option<perf_event::Counter>,
    branches: Option<perf_event::Counter>,
    branch_misses: Option<perf_event::Counter>,
    cache_misses: Option<perf_event::Counter>,
    stalled_cycles_frontend: Option<perf_event::Counter>,
    stalled_cycles_backend: Option<perf_event::Counter>,
}

pub(super) fn pmu_scheduled_percent(results: &Results) -> f64 {
    safe_ratio_f64(
        results.pmu_time_running_ns as f64,
        results.pmu_time_enabled_ns as f64,
    ) * 100.0
}

pub(super) fn enforce_pmu_quality(name: &str, has_perf_counters: bool, results: &Results) {
    if !has_perf_counters || results.pmu_time_enabled_ns == 0 || results.pmu_time_running_ns == 0 {
        return;
    }

    let scheduled_percent = pmu_scheduled_percent(results);
    if scheduled_percent < MIN_PMU_SCHEDULED_PERCENT {
        eprintln!(
            "⚠️ PMU counters were scheduled too little for benchmark '{name}': {scheduled_percent:.1}% < {MIN_PMU_SCHEDULED_PERCENT:.1}%; scaled values may be unreliable"
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

    if issues.iter().any(|existing| existing == &message) || issues.len() >= 6 {
        return;
    }
    issues.push(message);
}

#[cfg(target_os = "linux")]
pub(super) fn clear_perf_issues() {
    let lock = perf_issues().lock();
    let mut issues = match lock {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    issues.clear();
}

#[cfg(target_os = "linux")]
pub(super) fn current_perf_issues() -> Vec<String> {
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

pub(super) fn warn_perf_status(has_perf_counters: bool, has_full_perf_counters: bool) {
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

    eprintln!(
        "⚠️  PMU counters are unavailable on this platform; continuing with timing-only results."
    );
}

pub(super) fn measurement_label(has_perf_counters: bool) -> &'static str {
    if has_perf_counters {
        "timing + PMU"
    } else {
        "timing only"
    }
}

fn scale_multiplexed_count(raw: u64, enabled_ns: u64, running_ns: u64) -> u64 {
    if raw == 0 {
        return 0;
    }
    if enabled_ns == 0 || running_ns == 0 || running_ns >= enabled_ns {
        return raw;
    }

    ((raw as u128 * enabled_ns as u128) / running_ns as u128).min(u64::MAX as u128) as u64
}

#[cfg(target_os = "linux")]
fn try_add_group_counter(
    group: &mut Group,
    event: Hardware,
    name: &str,
    target: PerfTarget,
) -> Option<perf_event::Counter> {
    let mut builder = Builder::new(event);
    target.configure(&mut builder);
    match group.add(&builder) {
        Ok(counter) => Some(counter),
        Err(error) => {
            record_perf_issue(format!("perf event '{name}' unavailable: {error}"));
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn try_add_l1i_group_counter(group: &mut Group, target: PerfTarget) -> Option<perf_event::Counter> {
    let mut builder = Builder::new(Cache {
        which: CacheId::L1I,
        operation: CacheOp::READ,
        result: CacheResult::MISS,
    });
    target.configure(&mut builder);
    match group.add(&builder) {
        Ok(counter) => Some(counter),
        Err(error) => {
            record_perf_issue(format!("perf event 'l1i-misses' unavailable: {error}"));
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn try_build_l1i_counter(target: PerfTarget) -> Option<perf_event::Counter> {
    let mut builder = Builder::new(Cache {
        which: CacheId::L1I,
        operation: CacheOp::READ,
        result: CacheResult::MISS,
    });
    target.configure(&mut builder);
    match builder.build() {
        Ok(counter) => Some(counter),
        Err(error) => {
            record_perf_issue(format!("perf event 'l1i-misses' unavailable: {error}"));
            None
        }
    }
}

#[cfg(target_os = "linux")]
fn build_perf_counter_group(target: PerfTarget) -> Option<PerfGroupCounters> {
    let mut group_builder = Group::builder();
    target.configure(&mut group_builder);
    let mut group = match group_builder.build_group() {
        Ok(group) => group,
        Err(error) => {
            record_perf_issue(format!("perf group unavailable: {error}"));
            return None;
        }
    };

    let cycles = try_add_group_counter(&mut group, Hardware::CPU_CYCLES, "cycles", target);
    let instructions =
        try_add_group_counter(&mut group, Hardware::INSTRUCTIONS, "instructions", target);
    let cache_references = try_add_group_counter(
        &mut group,
        Hardware::CACHE_REFERENCES,
        "cache-references",
        target,
    );
    let l1i_misses = try_add_l1i_group_counter(&mut group, target);
    let branches = try_add_group_counter(
        &mut group,
        Hardware::BRANCH_INSTRUCTIONS,
        "branches",
        target,
    );
    let branch_misses =
        try_add_group_counter(&mut group, Hardware::BRANCH_MISSES, "branch-misses", target);
    let cache_misses =
        try_add_group_counter(&mut group, Hardware::CACHE_MISSES, "cache-misses", target);
    let stalled_cycles_frontend = try_add_group_counter(
        &mut group,
        Hardware::STALLED_CYCLES_FRONTEND,
        "stalled-cycles-frontend",
        target,
    );
    let stalled_cycles_backend = try_add_group_counter(
        &mut group,
        Hardware::STALLED_CYCLES_BACKEND,
        "stalled-cycles-backend",
        target,
    );

    if cycles.is_none()
        && instructions.is_none()
        && cache_references.is_none()
        && l1i_misses.is_none()
        && branches.is_none()
        && branch_misses.is_none()
        && cache_misses.is_none()
        && stalled_cycles_frontend.is_none()
        && stalled_cycles_backend.is_none()
    {
        record_perf_issue("no perf events could be added to perf group".to_string());
        return None;
    }

    Some(PerfGroupCounters {
        group,
        cycles,
        instructions,
        cache_references,
        l1i_misses,
        branches,
        branch_misses,
        cache_misses,
        stalled_cycles_frontend,
        stalled_cycles_backend,
    })
}

#[cfg(target_os = "linux")]
fn try_build_individual_counter(
    event: Hardware,
    name: &str,
    target: PerfTarget,
) -> Option<perf_event::Counter> {
    let mut builder = Builder::new(event);
    target.configure(&mut builder);
    match builder.build() {
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
            if cat.time_enabled == 0 || cat.time_running == 0 {
                record_perf_issue(format!(
                    "perf event '{name}' has no usable scheduled window; omitting it"
                ));
                return (0, cat.time_enabled, cat.time_running);
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
fn timing_window(timing_candidates: &[(u64, u64)]) -> (u64, u64) {
    timing_candidates
        .iter()
        .copied()
        .filter(|(enabled, _)| *enabled > 0)
        .min_by(
            |(left_enabled, left_running), (right_enabled, right_running)| {
                // Compare running/enabled without losing precision to floating
                // point. The least-scheduled event is the conservative quality
                // indicator for independently multiplexed counters.
                ((*left_running as u128) * (*right_enabled as u128))
                    .cmp(&((*right_running as u128) * (*left_enabled as u128)))
            },
        )
        .unwrap_or((0, 0))
}

#[cfg(target_os = "linux")]
pub(super) fn prepare_concurrent_worker_measurement() -> LinuxPerfBackend {
    let mut backend = LinuxPerfBackend::new();
    // Concurrent workers are recreated for every sample, so they cannot
    // remember that an oversized group was unschedulable on a prior sample.
    // Start directly with independently multiplexed counters.
    backend.prefer_individual = true;
    backend.prepare();
    backend
}

#[cfg(target_os = "linux")]
pub(super) fn execute_concurrent_worker<T: ConcurrentBenchContext>(
    backend: &mut LinuxPerfBackend,
    prepared: &T,
    control: &ConcurrentBenchControl,
    run: fn(&T, &ConcurrentBenchControl) -> ConcurrentWorkerResult,
) -> ConcurrentWorkerMeasurement {
    let started = Instant::now();
    let worker_result = run(prepared, control);
    let host_elapsed = started.elapsed();
    backend.end();

    let mut results = Results::default();
    let mut metrics = Vec::new();
    backend.collect(
        host_elapsed,
        worker_result.operations,
        0,
        &mut results,
        &mut metrics,
    );
    ConcurrentWorkerMeasurement {
        results,
        counters: worker_result.counters,
    }
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
            l1i_misses: Builder::new(Cache {
                which: CacheId::L1I,
                operation: CacheOp::READ,
                result: CacheResult::MISS,
            })
            .build()?,
            stalled_frontend: Builder::new(Hardware::STALLED_CYCLES_FRONTEND).build()?,
            stalled_backend: Builder::new(Hardware::STALLED_CYCLES_BACKEND).build()?,
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

    pub fn stop(&mut self) -> (std::time::Duration, u64, u64, u64, u64, u64, u64, u64, u64) {
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
            .map_or(std::time::Duration::from_secs(0), |start| start.elapsed());
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

// ---------------------------------------------------------------------------
// MeasurementBackend implementation
// ---------------------------------------------------------------------------

/// Active perf measurement strategy for the current sample window.
///
/// Prepared before a measurement window by trying the perf group first and
/// falling back to individual counters. Activation happens separately so
/// concurrent workers can open counters before their ready barrier.
#[cfg(target_os = "linux")]
enum PerfMode {
    /// Not yet initialised for the current sample; no measurement is
    /// active. Also the state after [`LinuxPerfBackend::collect`] resets
    /// for the next sample.
    Idle,
    /// Perf event group is open; it may be prepared or active.
    Group(PerfGroupCounters),
    /// Individual (ungrouped) counters are open; they may be prepared or
    /// active. This is the fallback when the perf group cannot be created.
    Individual(IndividualCounters),
    /// No perf counters could be opened at all — timing-only mode for
    /// this sample. `collect` will leave all `has_*` flags false.
    None,
}

#[cfg(target_os = "linux")]
struct TargetMeasurement {
    target: PerfTarget,
    mode: PerfMode,
}

/// Standalone (ungrouped) perf counters, used when the grouped path is
/// unavailable. Extracted from the historic `run_with_individual_counters`
/// function so the counter handles can live across `begin` / `end` /
/// `collect`.
#[cfg(target_os = "linux")]
struct IndividualCounters {
    cycles: Option<perf_event::Counter>,
    instructions: Option<perf_event::Counter>,
    cache_references: Option<perf_event::Counter>,
    l1i_misses: Option<perf_event::Counter>,
    branches: Option<perf_event::Counter>,
    branch_misses: Option<perf_event::Counter>,
    cache_misses: Option<perf_event::Counter>,
    stalled_cycles_frontend: Option<perf_event::Counter>,
    stalled_cycles_backend: Option<perf_event::Counter>,
}

/// Performance counter measurement backend for Linux.
///
/// Implements [`MeasurementBackend`] by wrapping the existing perf-event
/// group + individual-counter fallback chain. For ordinary samples,
/// [`begin`](MeasurementBackend::begin) opens and enables fresh counters,
/// [`end`](MeasurementBackend::end) disables them, and
/// [`collect`](MeasurementBackend::collect) reads and scales them into
/// [`Results`]. Concurrent workers use the internal prepare/activate split.
///
/// This backend is the default on Linux; on other platforms
/// [`crate::WallClockBackend`] is the default.
///
/// By default only the calling thread is measured. [`process_threads`](Self::process_threads)
/// snapshots all existing threads before every sample, while
/// [`registered_threads`](Self::registered_threads) targets a caller-managed
/// set such as the workers of an existing Rayon pool.
#[cfg(target_os = "linux")]
pub struct LinuxPerfBackend {
    scope: PerfScope,
    measurements: Vec<TargetMeasurement>,
    prefer_individual: bool,
}

#[cfg(target_os = "linux")]
impl Default for LinuxPerfBackend {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(target_os = "linux")]
impl LinuxPerfBackend {
    pub fn new() -> Self {
        Self {
            scope: PerfScope::CurrentThread,
            measurements: Vec::new(),
            prefer_individual: false,
        }
    }

    /// Measure every thread that exists in the calling process when a sample
    /// begins. Threads created during the sample are not inherited; initialize
    /// long-lived worker pools before entering the benchmark window.
    pub fn process_threads() -> Self {
        Self {
            scope: PerfScope::ProcessThreads,
            measurements: Vec::new(),
            prefer_individual: true,
        }
    }

    /// Measure only threads contained in `threads`.
    pub fn registered_threads(threads: LinuxPerfThreadSet) -> Self {
        Self {
            scope: PerfScope::RegisteredThreads(threads),
            measurements: Vec::new(),
            prefer_individual: true,
        }
    }

    fn targets(&self) -> Vec<PerfTarget> {
        match &self.scope {
            PerfScope::CurrentThread => vec![PerfTarget::CurrentThread],
            PerfScope::ProcessThreads => match process_thread_ids() {
                Ok(tids) => tids.into_iter().map(PerfTarget::Thread).collect(),
                Err(error) => {
                    record_perf_issue(format!("could not enumerate process threads: {error}"));
                    Vec::new()
                }
            },
            PerfScope::RegisteredThreads(threads) => threads
                .snapshot()
                .into_iter()
                .map(PerfTarget::Thread)
                .collect(),
        }
    }

    /// Try to build grouped perf counters without activating them.
    fn try_prepare_group(target: PerfTarget) -> Option<PerfMode> {
        build_perf_counter_group(target).map(PerfMode::Group)
    }

    /// Build individual counters without activating them.
    fn prepare_individual(target: PerfTarget) -> PerfMode {
        let ind = IndividualCounters {
            cycles: try_build_individual_counter(Hardware::CPU_CYCLES, "cycles", target),
            instructions: try_build_individual_counter(
                Hardware::INSTRUCTIONS,
                "instructions",
                target,
            ),
            cache_references: try_build_individual_counter(
                Hardware::CACHE_REFERENCES,
                "cache-references",
                target,
            ),
            l1i_misses: try_build_l1i_counter(target),
            branches: try_build_individual_counter(
                Hardware::BRANCH_INSTRUCTIONS,
                "branches",
                target,
            ),
            branch_misses: try_build_individual_counter(
                Hardware::BRANCH_MISSES,
                "branch-misses",
                target,
            ),
            cache_misses: try_build_individual_counter(
                Hardware::CACHE_MISSES,
                "cache-misses",
                target,
            ),
            stalled_cycles_frontend: try_build_individual_counter(
                Hardware::STALLED_CYCLES_FRONTEND,
                "stalled-cycles-frontend",
                target,
            ),
            stalled_cycles_backend: try_build_individual_counter(
                Hardware::STALLED_CYCLES_BACKEND,
                "stalled-cycles-backend",
                target,
            ),
        };

        let all_none = ind.cycles.is_none()
            && ind.instructions.is_none()
            && ind.cache_references.is_none()
            && ind.l1i_misses.is_none()
            && ind.branches.is_none()
            && ind.branch_misses.is_none()
            && ind.cache_misses.is_none()
            && ind.stalled_cycles_frontend.is_none()
            && ind.stalled_cycles_backend.is_none();

        if all_none {
            return PerfMode::None;
        }

        record_perf_issue("using ungrouped perf counters fallback".to_string());
        PerfMode::Individual(ind)
    }

    /// Open all perf-event handles for a future sample without enabling them.
    pub(super) fn prepare(&mut self) {
        self.measurements.clear();
        let targets = self.targets();
        if targets.is_empty() {
            record_perf_issue("PMU scope contains no live threads".to_string());
        }
        for target in targets {
            let mode = if self.prefer_individual {
                Self::prepare_individual(target)
            } else {
                Self::try_prepare_group(target).unwrap_or_else(|| Self::prepare_individual(target))
            };
            self.measurements.push(TargetMeasurement { target, mode });
        }
    }

    /// Activate handles previously opened by [`Self::prepare`]. Returns false
    /// only when a prepared group cannot be enabled or no state was prepared.
    fn enable_prepared(mode: &mut PerfMode) -> bool {
        match mode {
            PerfMode::Group(perf) => {
                if let Err(error) = perf.group.enable() {
                    record_perf_issue(format!("perf group enable failed: {error}"));
                    *mode = PerfMode::Idle;
                    false
                } else {
                    true
                }
            }
            PerfMode::Individual(ind) => {
                enable_counter(&mut ind.cycles, "cycles");
                enable_counter(&mut ind.instructions, "instructions");
                enable_counter(&mut ind.cache_references, "cache-references");
                enable_counter(&mut ind.l1i_misses, "l1i-misses");
                enable_counter(&mut ind.branches, "branches");
                enable_counter(&mut ind.branch_misses, "branch-misses");
                enable_counter(&mut ind.cache_misses, "cache-misses");
                enable_counter(&mut ind.stalled_cycles_frontend, "stalled-cycles-frontend");
                enable_counter(&mut ind.stalled_cycles_backend, "stalled-cycles-backend");
                true
            }
            PerfMode::None => true,
            PerfMode::Idle => false,
        }
    }

    /// Activate prepared counters, constructing the individual-counter
    /// fallback if a grouped handle cannot be enabled. Concurrent workers call
    /// this before their measurement-ready barrier, so the fallback remains
    /// outside the workload deadline.
    pub(super) fn begin_prepared(&mut self) {
        for measurement in &mut self.measurements {
            if !Self::enable_prepared(&mut measurement.mode) {
                self.prefer_individual = true;
                measurement.mode = Self::prepare_individual(measurement.target);
                let _ = Self::enable_prepared(&mut measurement.mode);
            }
        }
    }

    fn disable_group(perf: &mut PerfGroupCounters) {
        if let Err(error) = perf.group.disable() {
            record_perf_issue(format!("perf group disable failed: {error}"));
        }
    }

    fn disable_individual(ind: &mut IndividualCounters) {
        disable_counter(&mut ind.cycles, "cycles");
        disable_counter(&mut ind.instructions, "instructions");
        disable_counter(&mut ind.cache_references, "cache-references");
        disable_counter(&mut ind.l1i_misses, "l1i-misses");
        disable_counter(&mut ind.branches, "branches");
        disable_counter(&mut ind.branch_misses, "branch-misses");
        disable_counter(&mut ind.cache_misses, "cache-misses");
        disable_counter(&mut ind.stalled_cycles_frontend, "stalled-cycles-frontend");
        disable_counter(&mut ind.stalled_cycles_backend, "stalled-cycles-backend");
    }

    fn collect_group(
        perf: &mut PerfGroupCounters,
        host_elapsed: Duration,
        ops: u64,
        results: &mut Results,
    ) -> bool {
        let counts = match perf.group.read() {
            Ok(counts) => counts,
            Err(error) => {
                record_perf_issue(format!("perf group read failed: {error}"));
                results.duration = host_elapsed;
                results.iterations = ops;
                results.chunks_executed = 1;
                return true;
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

        let cycles_raw = perf
            .cycles
            .as_ref()
            .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
            .unwrap_or(0);
        let instructions_raw = perf
            .instructions
            .as_ref()
            .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
            .unwrap_or(0);
        let cache_references_raw = perf
            .cache_references
            .as_ref()
            .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
            .unwrap_or(0);
        let l1i_misses_raw = perf
            .l1i_misses
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
        let stalled_cycles_frontend_raw = perf
            .stalled_cycles_frontend
            .as_ref()
            .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
            .unwrap_or(0);
        let stalled_cycles_backend_raw = perf
            .stalled_cycles_backend
            .as_ref()
            .and_then(|counter| counts.get(counter).map(|entry| entry.value()))
            .unwrap_or(0);

        let usable_timing = enabled_ns > 0 && running_ns > 0;
        if !usable_timing {
            record_perf_issue(
                "perf group was opened but never scheduled; switching to ungrouped counters"
                    .to_string(),
            );
        }

        if usable_timing {
            results.cycles = scale_multiplexed_count(cycles_raw, enabled_ns, running_ns);
            results.instructions =
                scale_multiplexed_count(instructions_raw, enabled_ns, running_ns);
            results.cache_references =
                scale_multiplexed_count(cache_references_raw, enabled_ns, running_ns);
            results.l1i_misses = scale_multiplexed_count(l1i_misses_raw, enabled_ns, running_ns);
            results.branches = scale_multiplexed_count(branches_raw, enabled_ns, running_ns);
            results.branch_misses =
                scale_multiplexed_count(branch_misses_raw, enabled_ns, running_ns);
            results.cache_misses =
                scale_multiplexed_count(cache_misses_raw, enabled_ns, running_ns);
            results.stalled_cycles_frontend =
                scale_multiplexed_count(stalled_cycles_frontend_raw, enabled_ns, running_ns);
            results.stalled_cycles_backend =
                scale_multiplexed_count(stalled_cycles_backend_raw, enabled_ns, running_ns);
            results.has_cycles = perf.cycles.is_some();
            results.has_instructions = perf.instructions.is_some();
            results.has_cache_references = perf.cache_references.is_some();
            results.has_l1i_misses = perf.l1i_misses.is_some();
            results.has_branches = perf.branches.is_some();
            results.has_branch_misses = perf.branch_misses.is_some();
            results.has_cache_misses = perf.cache_misses.is_some();
            results.has_stalled_cycles_frontend = perf.stalled_cycles_frontend.is_some();
            results.has_stalled_cycles_backend = perf.stalled_cycles_backend.is_some();
        }
        results.pmu_time_enabled_ns = enabled_ns;
        results.pmu_time_running_ns = running_ns;
        results.duration = host_elapsed;
        results.iterations = ops;
        results.chunks_executed = 1;

        !usable_timing
            || safe_ratio_f64(running_ns as f64, enabled_ns as f64) * 100.0
                < MIN_PMU_SCHEDULED_PERCENT
    }

    fn collect_individual(
        ind: &mut IndividualCounters,
        host_elapsed: Duration,
        ops: u64,
        results: &mut Results,
    ) {
        let (cycles, cycles_enabled, cycles_running) =
            read_scaled_counter(&mut ind.cycles, "cycles");
        let (instructions, instructions_enabled, instructions_running) =
            read_scaled_counter(&mut ind.instructions, "instructions");
        let (cache_references, cache_references_enabled, cache_references_running) =
            read_scaled_counter(&mut ind.cache_references, "cache-references");
        let (l1i_misses, l1i_misses_enabled, l1i_misses_running) =
            read_scaled_counter(&mut ind.l1i_misses, "l1i-misses");
        let (branches, branches_enabled, branches_running) =
            read_scaled_counter(&mut ind.branches, "branches");
        let (branch_misses, branch_misses_enabled, branch_misses_running) =
            read_scaled_counter(&mut ind.branch_misses, "branch-misses");
        let (cache_misses, cache_misses_enabled, cache_misses_running) =
            read_scaled_counter(&mut ind.cache_misses, "cache-misses");
        let (
            stalled_cycles_frontend,
            stalled_cycles_frontend_enabled,
            stalled_cycles_frontend_running,
        ) = read_scaled_counter(&mut ind.stalled_cycles_frontend, "stalled-cycles-frontend");
        let (
            stalled_cycles_backend,
            stalled_cycles_backend_enabled,
            stalled_cycles_backend_running,
        ) = read_scaled_counter(&mut ind.stalled_cycles_backend, "stalled-cycles-backend");

        let (pmu_time_enabled_ns, pmu_time_running_ns) = timing_window(&[
            (cycles_enabled, cycles_running),
            (instructions_enabled, instructions_running),
            (cache_references_enabled, cache_references_running),
            (l1i_misses_enabled, l1i_misses_running),
            (branches_enabled, branches_running),
            (branch_misses_enabled, branch_misses_running),
            (cache_misses_enabled, cache_misses_running),
            (
                stalled_cycles_frontend_enabled,
                stalled_cycles_frontend_running,
            ),
            (
                stalled_cycles_backend_enabled,
                stalled_cycles_backend_running,
            ),
        ]);

        results.cycles = cycles;
        results.instructions = instructions;
        results.cache_references = cache_references;
        results.l1i_misses = l1i_misses;
        results.branches = branches;
        results.branch_misses = branch_misses;
        results.cache_misses = cache_misses;
        results.stalled_cycles_frontend = stalled_cycles_frontend;
        results.stalled_cycles_backend = stalled_cycles_backend;
        results.has_cycles = ind.cycles.is_some() && cycles_enabled > 0 && cycles_running > 0;
        results.has_instructions =
            ind.instructions.is_some() && instructions_enabled > 0 && instructions_running > 0;
        results.has_cache_references = ind.cache_references.is_some()
            && cache_references_enabled > 0
            && cache_references_running > 0;
        results.has_l1i_misses =
            ind.l1i_misses.is_some() && l1i_misses_enabled > 0 && l1i_misses_running > 0;
        results.has_branches =
            ind.branches.is_some() && branches_enabled > 0 && branches_running > 0;
        results.has_branch_misses =
            ind.branch_misses.is_some() && branch_misses_enabled > 0 && branch_misses_running > 0;
        results.has_cache_misses =
            ind.cache_misses.is_some() && cache_misses_enabled > 0 && cache_misses_running > 0;
        results.has_stalled_cycles_frontend = ind.stalled_cycles_frontend.is_some()
            && stalled_cycles_frontend_enabled > 0
            && stalled_cycles_frontend_running > 0;
        results.has_stalled_cycles_backend = ind.stalled_cycles_backend.is_some()
            && stalled_cycles_backend_enabled > 0
            && stalled_cycles_backend_running > 0;
        results.pmu_time_enabled_ns = pmu_time_enabled_ns;
        results.pmu_time_running_ns = pmu_time_running_ns;
        results.duration = host_elapsed;
        results.iterations = ops;
        results.chunks_executed = 1;
    }
}

#[cfg(target_os = "linux")]
impl MeasurementBackend for LinuxPerfBackend {
    fn begin(&mut self) {
        self.prepare();
        self.begin_prepared();
    }

    fn end(&mut self) {
        for measurement in &mut self.measurements {
            match &mut measurement.mode {
                PerfMode::Group(perf) => Self::disable_group(perf),
                PerfMode::Individual(ind) => Self::disable_individual(ind),
                PerfMode::None | PerfMode::Idle => {}
            }
        }
    }

    fn collect(
        &mut self,
        host_elapsed: Duration,
        ops: u64,
        _chunk_index: usize,
        results: &mut Results,
        _metrics: &mut Vec<MetricValue>,
    ) {
        let mut aggregate = Results::default();
        let mut prefer_individual = self.prefer_individual;
        for measurement in &mut self.measurements {
            let mut thread_results = Results::default();
            match &mut measurement.mode {
                PerfMode::Group(perf) => {
                    prefer_individual |=
                        Self::collect_group(perf, Duration::ZERO, 0, &mut thread_results);
                }
                PerfMode::Individual(ind) => {
                    Self::collect_individual(ind, Duration::ZERO, 0, &mut thread_results)
                }
                PerfMode::None | PerfMode::Idle => {}
            }
            aggregate.add(&thread_results);
        }
        aggregate.duration = host_elapsed;
        aggregate.iterations = ops;
        aggregate.chunks_executed = 1;
        *results = aggregate;
        self.prefer_individual = prefer_individual;

        // Reset for the next sample window.
        self.measurements.clear();
    }

    fn measurement_label(&self) -> &'static str {
        match self.scope {
            PerfScope::CurrentThread => "timing + PMU",
            PerfScope::ProcessThreads => "timing + process-thread PMU",
            PerfScope::RegisteredThreads(_) => "timing + registered-thread PMU",
        }
    }

    fn pmu_scope(&self) -> crate::PmuScope {
        match self.scope {
            PerfScope::CurrentThread => crate::PmuScope::CallingThread,
            PerfScope::ProcessThreads => crate::PmuScope::ProcessThreads,
            PerfScope::RegisteredThreads(_) => crate::PmuScope::RegisteredThreads,
        }
    }

    fn emits_cpu_diagnostics(&self) -> bool {
        true
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{
        LinuxPerfBackend, LinuxPerfThreadSet, PerfMode, PerfTarget, TargetMeasurement,
        current_thread_id, process_thread_ids, timing_window,
    };
    use crate::MeasurementBackend;

    #[test]
    fn prepared_activation_failure_attempts_individual_fallback() {
        let mut backend = LinuxPerfBackend::new();
        backend.measurements.push(TargetMeasurement {
            target: PerfTarget::CurrentThread,
            mode: PerfMode::Idle,
        });

        // Idle is the state left by a grouped-counter activation failure.
        backend.begin_prepared();
        assert!(matches!(
            backend.measurements[0].mode,
            PerfMode::Individual(_) | PerfMode::None
        ));
        backend.end();
    }

    #[test]
    fn timing_window_reports_least_scheduled_counter() {
        assert_eq!(
            timing_window(&[(1_000, 800), (1_000, 250), (1_000, 600)]),
            (1_000, 250)
        );
        assert_eq!(timing_window(&[(0, 0), (500, 0)]), (500, 0));
    }

    #[test]
    fn registered_thread_set_is_idempotent() {
        let threads = LinuxPerfThreadSet::new();
        assert!(threads.is_empty());
        let tid = threads.register_current();
        assert_eq!(tid, current_thread_id());
        threads.register_current();
        assert_eq!(threads.len(), 1);
        assert!(threads.unregister_current());
        assert!(threads.is_empty());
    }

    #[test]
    fn process_thread_snapshot_contains_caller() {
        let tids = process_thread_ids().expect("/proc/self/task should be readable on Linux");
        assert!(tids.contains(&current_thread_id()));
    }

    #[test]
    fn public_scopes_have_distinct_persisted_identity() {
        let current = LinuxPerfBackend::new();
        let process = LinuxPerfBackend::process_threads();
        let registered = LinuxPerfBackend::registered_threads(LinuxPerfThreadSet::new());

        assert_eq!(current.pmu_scope(), crate::PmuScope::CallingThread);
        assert_eq!(process.pmu_scope(), crate::PmuScope::ProcessThreads);
        assert_eq!(registered.pmu_scope(), crate::PmuScope::RegisteredThreads);
        assert_ne!(current.measurement_label(), process.measurement_label());
        assert_ne!(process.measurement_label(), registered.measurement_label());
    }
}
