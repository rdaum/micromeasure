use super::{
    ConcurrentBenchContext, ConcurrentBenchControl, ConcurrentWorkerMeasurement,
    ConcurrentWorkerResult, Results, safe_ratio_f64,
};
use std::{
    io,
    sync::atomic::{AtomicBool, Ordering},
    time::Instant,
};

#[cfg(target_os = "linux")]
use perf_event::{Builder, Group, events::Hardware};
#[cfg(target_os = "linux")]
use std::sync::{Mutex, OnceLock};

const MIN_PMU_ACTIVE_PERCENT: f64 = 90.0;

#[cfg(target_os = "linux")]
struct PerfGroupCounters {
    group: Group,
    instructions: Option<perf_event::Counter>,
    branches: Option<perf_event::Counter>,
    branch_misses: Option<perf_event::Counter>,
    cache_misses: Option<perf_event::Counter>,
}

pub(super) fn pmu_active_percent(results: &Results) -> f64 {
    safe_ratio_f64(
        results.pmu_time_running_ns as f64,
        results.pmu_time_enabled_ns as f64,
    ) * 100.0
}

pub(super) fn enforce_pmu_quality(name: &str, has_perf_counters: bool, results: &Results) {
    if !has_perf_counters || results.pmu_time_enabled_ns == 0 || results.pmu_time_running_ns == 0 {
        return;
    }

    let active_percent = pmu_active_percent(results);
    if active_percent < MIN_PMU_ACTIVE_PERCENT {
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
fn timing_window(timing_candidates: [(u64, u64); 4]) -> (u64, u64) {

    timing_candidates
        .iter()
        .copied()
        .find(|(_, running)| *running > 0)
        .or_else(|| {
            timing_candidates
                .iter()
                .copied()
                .find(|(enabled, _)| *enabled > 0)
        })
        .unwrap_or((0, 0))
}

#[cfg(target_os = "linux")]
fn run_with_individual_counters(run: &mut impl FnMut() -> u64) -> Results {
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
        let start_time = Instant::now();
        let iterations = run();
        return Results {
            duration: start_time.elapsed(),
            iterations,
            chunks_executed: 1,
            ..Results::default()
        };
    }

    record_perf_issue("using ungrouped perf counters fallback".to_string());

    enable_counter(&mut instructions_counter, "instructions");
    enable_counter(&mut branches_counter, "branches");
    enable_counter(&mut branch_misses_counter, "branch-misses");
    enable_counter(&mut cache_misses_counter, "cache-misses");

    let start_time = Instant::now();
    let iterations = run();
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

    let (pmu_time_enabled_ns, pmu_time_running_ns) = timing_window([
        (instructions_enabled, instructions_running),
        (branches_enabled, branches_running),
        (branch_misses_enabled, branch_misses_running),
        (cache_misses_enabled, cache_misses_running),
    ]);

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
        iterations,
        chunks_executed: 1,
    }
}

#[cfg(target_os = "linux")]
fn run_with_perf_group(run: &mut impl FnMut() -> u64) -> Option<Results> {
    let mut perf = build_perf_counter_group()?;
    if let Err(error) = perf.group.enable() {
        record_perf_issue(format!("perf group enable failed: {error}"));
        return None;
    }

    let start_time = Instant::now();
    let iterations = run();
    let duration = start_time.elapsed();
    if let Err(error) = perf.group.disable() {
        record_perf_issue(format!("perf group disable failed: {error}"));
    }

    let counts = match perf.group.read() {
        Ok(counts) => counts,
        Err(error) => {
            record_perf_issue(format!("perf group read failed: {error}"));
            return None;
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

    if (enabled_ns == 0 || running_ns == 0)
        && instructions_raw == 0
        && branches_raw == 0
        && branch_misses_raw == 0
        && cache_misses_raw == 0
    {
        record_perf_issue(
            "perf counters reported unusable timing window (enabled/running)".to_string(),
        );
        return None;
    }

    if enabled_ns == 0 || running_ns == 0 {
        record_perf_issue(
            "perf counters reported unusable timing window (enabled/running)".to_string(),
        );
    }

    Some(Results {
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
        iterations,
        chunks_executed: 1,
    })
}

#[cfg(target_os = "linux")]
pub(super) fn execute_standard(mut run: impl FnMut() -> u64) -> Results {
    run_with_perf_group(&mut run).unwrap_or_else(|| run_with_individual_counters(&mut run))
}

#[cfg(target_os = "linux")]
pub(super) fn execute_concurrent_worker<T: ConcurrentBenchContext>(
    prepared: &T,
    control: &ConcurrentBenchControl,
    run: fn(&T, &ConcurrentBenchControl) -> ConcurrentWorkerResult,
) -> ConcurrentWorkerMeasurement {
    let mut maybe_counters = None;
    let results = execute_standard(|| {
        let worker_result = run(prepared, control);
        maybe_counters = Some(worker_result.counters);
        worker_result.operations
    });
    ConcurrentWorkerMeasurement {
        results,
        counters: maybe_counters.unwrap_or_default(),
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
            l1i_misses: Builder::new(Hardware::CACHE_MISSES).build()?,
            stalled_frontend: Builder::new(Hardware::CACHE_MISSES).build()?,
            stalled_backend: Builder::new(Hardware::CACHE_MISSES).build()?,
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
