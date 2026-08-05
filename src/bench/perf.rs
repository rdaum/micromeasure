use super::{
    ConcurrentBenchContext, ConcurrentBenchControl, ConcurrentWorkerMeasurement,
    ConcurrentWorkerResult, Results, safe_ratio_f64,
};
use crate::bench::backend::{MeasurementBackend, MetricValue, PmuCounterProfile};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs, io,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(target_os = "linux")]
use perf_event::events::{Cache, CacheId, CacheOp, CacheResult, Dynamic, Hardware};
#[cfg(target_os = "linux")]
use perf_event::{Builder, Group};
#[cfg(target_os = "linux")]
use std::sync::{Mutex, OnceLock};

const DIRECT_PMU_SCHEDULED_PERCENT: f64 = 90.0;
const MIN_RELIABLE_PMU_SCHEDULED_PERCENT: f64 = 25.0;
const MIN_RELIABLE_PMU_RUNNING_TIME: Duration = Duration::from_millis(10);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PmuQuality {
    Direct,
    Multiplexed,
    Unreliable,
}

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
fn parse_cpu_list(value: &str) -> Result<Vec<usize>, String> {
    let mut cpus = BTreeSet::new();
    for part in value.trim().split(',').filter(|part| !part.is_empty()) {
        let mut bounds = part.split('-');
        let start = bounds
            .next()
            .ok_or_else(|| format!("invalid CPU list segment '{part}'"))?
            .parse::<usize>()
            .map_err(|error| format!("invalid CPU in '{part}': {error}"))?;
        let end = match bounds.next() {
            Some(end) => end
                .parse::<usize>()
                .map_err(|error| format!("invalid CPU in '{part}': {error}"))?,
            None => start,
        };
        if bounds.next().is_some() || end < start {
            return Err(format!("invalid CPU range '{part}'"));
        }
        cpus.extend(start..=end);
    }
    if cpus.is_empty() {
        return Err("CPU list is empty".to_string());
    }
    Ok(cpus.into_iter().collect())
}

#[cfg(target_os = "linux")]
fn discover_rapl_event(spec: RaplEventSpec) -> Option<RaplEventConfig> {
    let pmu_path = format!("/sys/bus/event_source/devices/{}", spec.pmu);
    let event_path = format!("{pmu_path}/events/{}", spec.event);
    if !std::path::Path::new(&event_path).is_file() {
        return None;
    }

    let cpus = match fs::read_to_string(format!("{pmu_path}/cpumask"))
        .map_err(|error| error.to_string())
        .and_then(|value| parse_cpu_list(&value))
    {
        Ok(cpus) => cpus,
        Err(error) => {
            record_perf_issue(format!(
                "RAPL event '{}/{}' CPU scope unavailable: {error}",
                spec.pmu, spec.event
            ));
            return None;
        }
    };

    let mut dynamic = match Dynamic::builder(spec.pmu) {
        Ok(dynamic) => dynamic,
        Err(error) => {
            record_perf_issue(format!(
                "RAPL PMU '{}' configuration unavailable: {error}",
                spec.pmu
            ));
            return None;
        }
    };
    if let Err(error) = dynamic.event(spec.event) {
        record_perf_issue(format!(
            "RAPL event '{}/{}' configuration unavailable: {error}",
            spec.pmu, spec.event
        ));
        return None;
    }

    let scale_joules = match dynamic.scale() {
        Ok(Some(scale)) if scale.is_finite() && scale > 0.0 => scale,
        Ok(_) => {
            record_perf_issue(format!(
                "RAPL event '{}/{}' has no usable Joule scale",
                spec.pmu, spec.event
            ));
            return None;
        }
        Err(error) => {
            record_perf_issue(format!(
                "RAPL event '{}/{}' scale unavailable: {error}",
                spec.pmu, spec.event
            ));
            return None;
        }
    };
    match dynamic.unit() {
        Ok(Some(unit)) if unit.eq_ignore_ascii_case("joules") => {}
        Ok(unit) => {
            record_perf_issue(format!(
                "RAPL event '{}/{}' reported unexpected unit {unit:?}",
                spec.pmu, spec.event
            ));
            return None;
        }
        Err(error) => {
            record_perf_issue(format!(
                "RAPL event '{}/{}' unit unavailable: {error}",
                spec.pmu, spec.event
            ));
            return None;
        }
    }

    let event = match dynamic.build() {
        Ok(event) => event,
        Err(error) => {
            record_perf_issue(format!(
                "RAPL event '{}/{}' could not be configured: {error}",
                spec.pmu, spec.event
            ));
            return None;
        }
    };
    Some(RaplEventConfig {
        domain: spec.domain,
        event,
        scale_joules,
        cpus,
    })
}

#[cfg(target_os = "linux")]
fn discover_rapl_events(scope: crate::EnergyScope) -> Vec<RaplEventConfig> {
    let mut configs: Vec<_> = RAPL_PACKAGE_EVENTS
        .iter()
        .filter_map(|spec| discover_rapl_event(*spec))
        .collect();
    if scope == crate::EnergyScope::RaplPackageAndCore {
        configs.extend(
            RAPL_CORE_EVENTS
                .iter()
                .filter_map(|spec| discover_rapl_event(*spec)),
        );
    }
    configs
}

#[cfg(target_os = "linux")]
fn prepare_rapl_measurement(configs: &[RaplEventConfig]) -> RaplMeasurement {
    let mut measurement = RaplMeasurement::default();
    for config in configs {
        for &cpu in &config.cpus {
            let mut builder = Builder::new(config.event);
            // RAPL is an uncore/system-wide PMU and advertises
            // PERF_PMU_CAP_NO_EXCLUDE. perf-event2 defaults to excluding
            // kernel and hypervisor activity for ordinary CPU events, which
            // makes RAPL reject the event configuration on affected kernels.
            builder
                .any_pid()
                .one_cpu(cpu)
                .exclude_kernel(false)
                .exclude_hv(false);
            match builder.build() {
                Ok(counter) => measurement.counters.push(RaplCounter {
                    domain: config.domain,
                    scale_joules: config.scale_joules,
                    counter,
                }),
                Err(error) => {
                    record_perf_issue(format!("RAPL event on CPU {cpu} unavailable: {error}"))
                }
            }
        }
    }
    measurement
}

#[cfg(target_os = "linux")]
impl RaplMeasurement {
    fn begin(&mut self) {
        self.counters.retain_mut(|measurement| {
            if let Err(error) = measurement.counter.reset() {
                record_perf_issue(format!("RAPL counter reset failed: {error}"));
                return false;
            }
            if let Err(error) = measurement.counter.enable() {
                record_perf_issue(format!("RAPL counter enable failed: {error}"));
                return false;
            }
            true
        });
    }

    fn end(&mut self) {
        for measurement in &mut self.counters {
            if let Err(error) = measurement.counter.disable() {
                record_perf_issue(format!("RAPL counter disable failed: {error}"));
            }
        }
    }

    fn collect(&mut self) -> BTreeMap<RaplDomain, f64> {
        let mut joules = BTreeMap::<RaplDomain, f64>::new();
        for measurement in &mut self.counters {
            match measurement.counter.read_count_and_time() {
                Ok(reading) if reading.time_enabled > 0 && reading.time_running > 0 => {
                    let count = scale_multiplexed_count(
                        reading.count,
                        reading.time_enabled,
                        reading.time_running,
                    );
                    *joules.entry(measurement.domain).or_default() +=
                        count as f64 * measurement.scale_joules;
                }
                Ok(_) => record_perf_issue(
                    "RAPL counter has no usable scheduled window; omitting it".to_string(),
                ),
                Err(error) => {
                    record_perf_issue(format!("RAPL counter read failed: {error}"));
                }
            }
        }
        joules
    }
}

#[cfg(target_os = "linux")]
fn push_rapl_metrics(
    joules: &BTreeMap<RaplDomain, f64>,
    operations: u64,
    elapsed: Duration,
    metrics: &mut Vec<MetricValue>,
) {
    for (&domain, &energy_joules) in joules {
        let (per_op_name, joules_name, watts_name) = domain.metric_names();
        if operations > 0 {
            metrics.push(
                MetricValue::new(
                    per_op_name,
                    energy_joules * 1_000_000.0 / operations as f64,
                    "µJ/op",
                )
                .with_display_name(match domain {
                    RaplDomain::Package => "Package energy/op",
                    RaplDomain::Cores => "CPU cores energy/op",
                    RaplDomain::Dram => "DRAM energy/op",
                    RaplDomain::Gpu => "Integrated GPU energy/op",
                    RaplDomain::Platform => "Platform energy/op",
                    RaplDomain::Core => "Per-core total energy/op",
                })
                .with_section("RAPL energy"),
            );
        }
        metrics.push(
            MetricValue::new(joules_name, energy_joules, "J")
                .with_display_name(match domain {
                    RaplDomain::Package => "Package energy/sample",
                    RaplDomain::Cores => "CPU cores energy/sample",
                    RaplDomain::Dram => "DRAM energy/sample",
                    RaplDomain::Gpu => "Integrated GPU energy/sample",
                    RaplDomain::Platform => "Platform energy/sample",
                    RaplDomain::Core => "Per-core total energy/sample",
                })
                .with_section("RAPL energy"),
        );
        if elapsed > Duration::ZERO {
            metrics.push(
                MetricValue::new(watts_name, energy_joules / elapsed.as_secs_f64(), "W")
                    .with_display_name(match domain {
                        RaplDomain::Package => "Package power",
                        RaplDomain::Cores => "CPU cores power",
                        RaplDomain::Dram => "DRAM power",
                        RaplDomain::Gpu => "Integrated GPU power",
                        RaplDomain::Platform => "Platform power",
                        RaplDomain::Core => "Per-core total power",
                    })
                    .with_section("RAPL energy"),
            );
        }
    }
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
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum RaplDomain {
    Package,
    Cores,
    Dram,
    Gpu,
    Platform,
    Core,
}

#[cfg(target_os = "linux")]
impl RaplDomain {
    fn metric_names(self) -> (&'static str, &'static str, &'static str) {
        match self {
            Self::Package => (
                "rapl_package_uj_per_op",
                "rapl_package_joules",
                "rapl_package_watts",
            ),
            Self::Cores => (
                "rapl_cores_uj_per_op",
                "rapl_cores_joules",
                "rapl_cores_watts",
            ),
            Self::Dram => ("rapl_dram_uj_per_op", "rapl_dram_joules", "rapl_dram_watts"),
            Self::Gpu => ("rapl_gpu_uj_per_op", "rapl_gpu_joules", "rapl_gpu_watts"),
            Self::Platform => (
                "rapl_platform_uj_per_op",
                "rapl_platform_joules",
                "rapl_platform_watts",
            ),
            Self::Core => ("rapl_core_uj_per_op", "rapl_core_joules", "rapl_core_watts"),
        }
    }
}

#[cfg(target_os = "linux")]
#[derive(Clone, Copy)]
struct RaplEventSpec {
    pmu: &'static str,
    event: &'static str,
    domain: RaplDomain,
}

#[cfg(target_os = "linux")]
const RAPL_PACKAGE_EVENTS: &[RaplEventSpec] = &[
    RaplEventSpec {
        pmu: "power",
        event: "energy-pkg",
        domain: RaplDomain::Package,
    },
    RaplEventSpec {
        pmu: "power",
        event: "energy-cores",
        domain: RaplDomain::Cores,
    },
    RaplEventSpec {
        pmu: "power",
        event: "energy-ram",
        domain: RaplDomain::Dram,
    },
    RaplEventSpec {
        pmu: "power",
        event: "energy-gpu",
        domain: RaplDomain::Gpu,
    },
    RaplEventSpec {
        pmu: "power",
        event: "energy-psys",
        domain: RaplDomain::Platform,
    },
];

#[cfg(target_os = "linux")]
const RAPL_CORE_EVENTS: &[RaplEventSpec] = &[RaplEventSpec {
    pmu: "power_core",
    event: "energy-core",
    domain: RaplDomain::Core,
}];

#[cfg(target_os = "linux")]
#[derive(Clone)]
struct RaplEventConfig {
    domain: RaplDomain,
    event: Dynamic,
    scale_joules: f64,
    cpus: Vec<usize>,
}

#[cfg(target_os = "linux")]
struct RaplCounter {
    domain: RaplDomain,
    scale_joules: f64,
    counter: perf_event::Counter,
}

#[cfg(target_os = "linux")]
#[derive(Default)]
struct RaplMeasurement {
    counters: Vec<RaplCounter>,
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

impl PmuCounterProfile {
    fn collects_cpu_counters(self) -> bool {
        self != Self::None
    }

    fn collects_extended_counters(self) -> bool {
        self == Self::Full
    }
}

pub(super) fn pmu_scheduled_percent(results: &Results) -> f64 {
    safe_ratio_f64(
        results.pmu_time_running_ns as f64,
        results.pmu_time_enabled_ns as f64,
    ) * 100.0
}

fn pmu_quality(results: &Results) -> PmuQuality {
    let scheduled_percent = pmu_scheduled_percent(results);
    if scheduled_percent >= DIRECT_PMU_SCHEDULED_PERCENT {
        PmuQuality::Direct
    } else if scheduled_percent < MIN_RELIABLE_PMU_SCHEDULED_PERCENT
        || results.pmu_time_running_ns < MIN_RELIABLE_PMU_RUNNING_TIME.as_nanos() as u64
    {
        PmuQuality::Unreliable
    } else {
        PmuQuality::Multiplexed
    }
}

pub(super) fn enforce_pmu_quality(name: &str, has_perf_counters: bool, results: &Results) {
    if !has_perf_counters || results.pmu_time_enabled_ns == 0 || results.pmu_time_running_ns == 0 {
        return;
    }

    let scheduled_percent = pmu_scheduled_percent(results);
    let running_ms = results.pmu_time_running_ns as f64 / 1_000_000.0;
    match pmu_quality(results) {
        PmuQuality::Direct => {}
        PmuQuality::Multiplexed => eprintln!(
            "ℹ️ PMU counters were multiplexed for benchmark '{name}': {scheduled_percent:.1}% scheduled ({running_ms:.1} ms least-counter running time per sample); values were scaled"
        ),
        PmuQuality::Unreliable => eprintln!(
            "⚠️ PMU counter coverage was too low for benchmark '{name}': {scheduled_percent:.1}% scheduled ({running_ms:.1} ms least-counter running time per sample); scaled values may be unreliable"
        ),
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

#[cfg(target_os = "linux")]
fn warn_rapl_unavailable_once() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }

    eprintln!("⚠️  RAPL energy counters unavailable; continuing without system energy metrics.");
    eprintln!(
        "   Check for /sys/bus/event_source/devices/power and system-wide perf access (CAP_PERFMON/CAP_SYS_ADMIN or kernel.perf_event_paranoid < 1)."
    );
}

#[cfg(target_os = "linux")]
fn warn_rapl_core_unavailable_once() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }

    eprintln!(
        "⚠️  Per-core RAPL counters unavailable; package/die energy domains will still be measured."
    );
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
fn build_perf_counter_group(
    target: PerfTarget,
    profile: PmuCounterProfile,
) -> Option<PerfGroupCounters> {
    if !profile.collects_cpu_counters() {
        return None;
    }
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
    let cache_references = if profile.collects_extended_counters() {
        try_add_group_counter(
            &mut group,
            Hardware::CACHE_REFERENCES,
            "cache-references",
            target,
        )
    } else {
        None
    };
    let l1i_misses = if profile.collects_extended_counters() {
        try_add_l1i_group_counter(&mut group, target)
    } else {
        None
    };
    let branches = try_add_group_counter(
        &mut group,
        Hardware::BRANCH_INSTRUCTIONS,
        "branches",
        target,
    );
    let branch_misses =
        try_add_group_counter(&mut group, Hardware::BRANCH_MISSES, "branch-misses", target);
    let cache_misses = if profile.collects_extended_counters() {
        try_add_group_counter(&mut group, Hardware::CACHE_MISSES, "cache-misses", target)
    } else {
        None
    };
    let stalled_cycles_frontend = if profile.collects_extended_counters() {
        try_add_group_counter(
            &mut group,
            Hardware::STALLED_CYCLES_FRONTEND,
            "stalled-cycles-frontend",
            target,
        )
    } else {
        None
    };
    let stalled_cycles_backend = if profile.collects_extended_counters() {
        try_add_group_counter(
            &mut group,
            Hardware::STALLED_CYCLES_BACKEND,
            "stalled-cycles-backend",
            target,
        )
    } else {
        None
    };

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
pub(super) fn prepare_concurrent_worker_measurement(
    profile: PmuCounterProfile,
) -> LinuxPerfBackend {
    let mut backend = LinuxPerfBackend::new().with_counter_profile(profile);
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
    counter_profile: PmuCounterProfile,
    measurements: Vec<TargetMeasurement>,
    prefer_individual: bool,
    energy_scope: crate::EnergyScope,
    rapl_configs: Option<Vec<RaplEventConfig>>,
    rapl_measurement: Option<RaplMeasurement>,
    rapl_observed: bool,
    rapl_core_observed: bool,
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
            counter_profile: PmuCounterProfile::Full,
            measurements: Vec::new(),
            prefer_individual: false,
            energy_scope: crate::EnergyScope::None,
            rapl_configs: None,
            rapl_measurement: None,
            rapl_observed: false,
            rapl_core_observed: false,
        }
    }

    /// Measure every thread that exists in the calling process when a sample
    /// begins. Threads created during the sample are not inherited; initialize
    /// long-lived worker pools before entering the benchmark window.
    pub fn process_threads() -> Self {
        Self {
            scope: PerfScope::ProcessThreads,
            counter_profile: PmuCounterProfile::Full,
            measurements: Vec::new(),
            prefer_individual: true,
            energy_scope: crate::EnergyScope::None,
            rapl_configs: None,
            rapl_measurement: None,
            rapl_observed: false,
            rapl_core_observed: false,
        }
    }

    /// Measure only threads contained in `threads`.
    pub fn registered_threads(threads: LinuxPerfThreadSet) -> Self {
        Self {
            scope: PerfScope::RegisteredThreads(threads),
            counter_profile: PmuCounterProfile::Full,
            measurements: Vec::new(),
            prefer_individual: true,
            energy_scope: crate::EnergyScope::None,
            rapl_configs: None,
            rapl_measurement: None,
            rapl_observed: false,
            rapl_core_observed: false,
        }
    }

    /// Select the CPU performance-counter set independently of RAPL energy.
    ///
    /// [`PmuCounterProfile::Compact`] requests cycles, instructions, branches,
    /// and branch misses so the set can fit common four-counter PMUs without
    /// permanent multiplexing. [`PmuCounterProfile::None`] provides a
    /// timing/RAPL-only backend.
    #[must_use]
    pub fn with_counter_profile(mut self, profile: PmuCounterProfile) -> Self {
        self.counter_profile = profile;
        self.measurements.clear();
        self
    }

    /// Select the four-event compact CPU-counter profile.
    #[must_use]
    pub fn with_compact_counters(self) -> Self {
        self.with_counter_profile(PmuCounterProfile::Compact)
    }

    /// Disable CPU counters while retaining timing and any configured RAPL
    /// energy measurement.
    #[must_use]
    pub fn without_cpu_counters(self) -> Self {
        self.with_counter_profile(PmuCounterProfile::None)
    }

    /// Add system-wide Linux RAPL energy measurement for every package/die
    /// domain exposed by the `power` PMU. This commonly includes whole-package
    /// energy and may also include cores, DRAM, integrated GPU, or platform
    /// energy depending on the processor.
    ///
    /// Energy is measured around each complete sample and reported as gross
    /// microjoules per operation, Joules per sample, and average Watts. The
    /// runner also sums sample energy, operations, and active measurement time
    /// before calculating a higher-signal aggregate. It is not attributable
    /// to the benchmark process: unrelated activity on the measured package
    /// is included. Because the scope is system-wide, work dispatched to an
    /// existing Rayon or other external worker pool is included without
    /// registering those threads.
    #[must_use]
    pub fn with_rapl_energy(mut self) -> Self {
        self.energy_scope = crate::EnergyScope::RaplPackageDomains;
        self.rapl_configs = None;
        self
    }

    /// Add package/die RAPL domains plus the per-core `power_core` counters
    /// available on some AMD processors.
    ///
    /// This opens one additional event for every CPU listed by the
    /// `power_core` PMU and sums them into a `Per-core total` metric. Prefer
    /// [`with_rapl_energy`](Self::with_rapl_energy) unless that extra
    /// attribution is useful, especially on high-core-count systems.
    #[must_use]
    pub fn with_rapl_core_energy(mut self) -> Self {
        self.energy_scope = crate::EnergyScope::RaplPackageAndCore;
        self.rapl_configs = None;
        self
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
    fn try_prepare_group(target: PerfTarget, profile: PmuCounterProfile) -> Option<PerfMode> {
        build_perf_counter_group(target, profile).map(PerfMode::Group)
    }

    /// Build individual counters without activating them.
    fn prepare_individual(target: PerfTarget, profile: PmuCounterProfile) -> PerfMode {
        if !profile.collects_cpu_counters() {
            return PerfMode::None;
        }
        let ind = IndividualCounters {
            cycles: try_build_individual_counter(Hardware::CPU_CYCLES, "cycles", target),
            instructions: try_build_individual_counter(
                Hardware::INSTRUCTIONS,
                "instructions",
                target,
            ),
            cache_references: if profile.collects_extended_counters() {
                try_build_individual_counter(Hardware::CACHE_REFERENCES, "cache-references", target)
            } else {
                None
            },
            l1i_misses: if profile.collects_extended_counters() {
                try_build_l1i_counter(target)
            } else {
                None
            },
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
            cache_misses: if profile.collects_extended_counters() {
                try_build_individual_counter(Hardware::CACHE_MISSES, "cache-misses", target)
            } else {
                None
            },
            stalled_cycles_frontend: if profile.collects_extended_counters() {
                try_build_individual_counter(
                    Hardware::STALLED_CYCLES_FRONTEND,
                    "stalled-cycles-frontend",
                    target,
                )
            } else {
                None
            },
            stalled_cycles_backend: if profile.collects_extended_counters() {
                try_build_individual_counter(
                    Hardware::STALLED_CYCLES_BACKEND,
                    "stalled-cycles-backend",
                    target,
                )
            } else {
                None
            },
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
        self.rapl_measurement = None;
        if self.counter_profile.collects_cpu_counters() {
            let targets = self.targets();
            if targets.is_empty() {
                record_perf_issue("PMU scope contains no live threads".to_string());
            }
            for target in targets {
                let mode = if self.prefer_individual {
                    Self::prepare_individual(target, self.counter_profile)
                } else {
                    Self::try_prepare_group(target, self.counter_profile)
                        .unwrap_or_else(|| Self::prepare_individual(target, self.counter_profile))
                };
                self.measurements.push(TargetMeasurement { target, mode });
            }
        }

        if self.energy_scope != crate::EnergyScope::None {
            let configs = self
                .rapl_configs
                .get_or_insert_with(|| discover_rapl_events(self.energy_scope));
            if configs.is_empty() {
                warn_rapl_unavailable_once();
            } else {
                if self.energy_scope == crate::EnergyScope::RaplPackageAndCore
                    && !configs
                        .iter()
                        .any(|config| config.domain == RaplDomain::Core)
                {
                    warn_rapl_core_unavailable_once();
                }
                let measurement = prepare_rapl_measurement(configs);
                if measurement.counters.is_empty() {
                    warn_rapl_unavailable_once();
                }
                self.rapl_measurement = Some(measurement);
            }
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
                measurement.mode =
                    Self::prepare_individual(measurement.target, self.counter_profile);
                let _ = Self::enable_prepared(&mut measurement.mode);
            }
        }
        if let Some(rapl) = &mut self.rapl_measurement {
            rapl.begin();
            if rapl.counters.is_empty() {
                warn_rapl_unavailable_once();
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
                < DIRECT_PMU_SCHEDULED_PERCENT
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
        if let Some(rapl) = &mut self.rapl_measurement {
            rapl.end();
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

        if let Some(rapl) = &mut self.rapl_measurement {
            let joules = rapl.collect();
            if !joules.is_empty() {
                self.rapl_observed = true;
                self.rapl_core_observed |= joules.contains_key(&RaplDomain::Core);
                push_rapl_metrics(&joules, ops, host_elapsed, metrics);
            }
        }

        // Reset for the next sample window.
        self.measurements.clear();
        self.rapl_measurement = None;
    }

    fn measurement_label(&self) -> &'static str {
        match (&self.scope, self.counter_profile, self.rapl_observed) {
            (_, PmuCounterProfile::None, false) => "timing only",
            (_, PmuCounterProfile::None, true) => "timing + RAPL energy",
            (PerfScope::CurrentThread, PmuCounterProfile::Full, false) => "timing + PMU",
            (PerfScope::ProcessThreads, PmuCounterProfile::Full, false) => {
                "timing + process-thread PMU"
            }
            (PerfScope::RegisteredThreads(_), PmuCounterProfile::Full, false) => {
                "timing + registered-thread PMU"
            }
            (PerfScope::CurrentThread, PmuCounterProfile::Compact, false) => "timing + compact PMU",
            (PerfScope::ProcessThreads, PmuCounterProfile::Compact, false) => {
                "timing + compact process-thread PMU"
            }
            (PerfScope::RegisteredThreads(_), PmuCounterProfile::Compact, false) => {
                "timing + compact registered-thread PMU"
            }
            (PerfScope::CurrentThread, PmuCounterProfile::Full, true) => {
                "timing + PMU + RAPL energy"
            }
            (PerfScope::ProcessThreads, PmuCounterProfile::Full, true) => {
                "timing + process-thread PMU + RAPL energy"
            }
            (PerfScope::RegisteredThreads(_), PmuCounterProfile::Full, true) => {
                "timing + registered-thread PMU + RAPL energy"
            }
            (PerfScope::CurrentThread, PmuCounterProfile::Compact, true) => {
                "timing + compact PMU + RAPL energy"
            }
            (PerfScope::ProcessThreads, PmuCounterProfile::Compact, true) => {
                "timing + compact process-thread PMU + RAPL energy"
            }
            (PerfScope::RegisteredThreads(_), PmuCounterProfile::Compact, true) => {
                "timing + compact registered-thread PMU + RAPL energy"
            }
        }
    }

    fn pmu_scope(&self) -> crate::PmuScope {
        match self.scope {
            PerfScope::CurrentThread => crate::PmuScope::CallingThread,
            PerfScope::ProcessThreads => crate::PmuScope::ProcessThreads,
            PerfScope::RegisteredThreads(_) => crate::PmuScope::RegisteredThreads,
        }
    }

    fn pmu_counter_profile(&self) -> PmuCounterProfile {
        self.counter_profile
    }

    fn energy_scope(&self) -> crate::EnergyScope {
        if self.rapl_observed {
            if self.energy_scope == crate::EnergyScope::RaplPackageAndCore
                && !self.rapl_core_observed
            {
                crate::EnergyScope::RaplPackageDomains
            } else {
                self.energy_scope
            }
        } else {
            crate::EnergyScope::None
        }
    }

    fn emits_cpu_diagnostics(&self) -> bool {
        self.counter_profile != PmuCounterProfile::None
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::{
        LinuxPerfBackend, LinuxPerfThreadSet, PerfMode, PerfTarget, PmuQuality, RaplDomain,
        TargetMeasurement, current_thread_id, parse_cpu_list, pmu_quality, process_thread_ids,
        push_rapl_metrics, timing_window,
    };
    use crate::{EnergyScope, MeasurementBackend, PmuCounterProfile};
    use std::{collections::BTreeMap, time::Duration};

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
    fn pmu_quality_distinguishes_direct_scaled_and_unreliable_windows() {
        let result = |enabled_ms, running_ms| crate::bench::Results {
            pmu_time_enabled_ns: Duration::from_millis(enabled_ms).as_nanos() as u64,
            pmu_time_running_ns: Duration::from_millis(running_ms).as_nanos() as u64,
            ..crate::bench::Results::default()
        };

        assert_eq!(pmu_quality(&result(100, 95)), PmuQuality::Direct);
        assert_eq!(pmu_quality(&result(100, 59)), PmuQuality::Multiplexed);
        assert_eq!(pmu_quality(&result(100, 20)), PmuQuality::Unreliable);
        assert_eq!(pmu_quality(&result(10, 6)), PmuQuality::Unreliable);
    }

    #[test]
    fn compact_and_none_profiles_select_the_intended_event_classes() {
        assert!(PmuCounterProfile::Compact.collects_cpu_counters());
        assert!(!PmuCounterProfile::Compact.collects_extended_counters());
        assert!(PmuCounterProfile::Full.collects_cpu_counters());
        assert!(PmuCounterProfile::Full.collects_extended_counters());
        assert!(!PmuCounterProfile::None.collects_cpu_counters());
        assert!(!PmuCounterProfile::None.collects_extended_counters());
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
    fn parses_rapl_cpu_lists() {
        assert_eq!(parse_cpu_list("0").unwrap(), vec![0]);
        assert_eq!(
            parse_cpu_list("0-2,4,8-9\n").unwrap(),
            vec![0, 1, 2, 4, 8, 9]
        );
        assert!(parse_cpu_list("").is_err());
        assert!(parse_cpu_list("3-1").is_err());
        assert!(parse_cpu_list("0-1-2").is_err());
    }

    #[test]
    fn rapl_metrics_are_normalized_per_operation_and_time() {
        let mut joules = BTreeMap::new();
        joules.insert(RaplDomain::Package, 0.25);
        let mut metrics = Vec::new();
        push_rapl_metrics(&joules, 100_000, Duration::from_millis(50), &mut metrics);

        let value = |name| {
            metrics
                .iter()
                .find(|metric| metric.name == name)
                .map(|metric| metric.value)
                .unwrap()
        };
        assert!((value("rapl_package_uj_per_op") - 2.5).abs() < f64::EPSILON);
        assert!((value("rapl_package_joules") - 0.25).abs() < f64::EPSILON);
        assert!((value("rapl_package_watts") - 5.0).abs() < f64::EPSILON);
        assert!(metrics.iter().all(|metric| metric.section == "RAPL energy"));
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

    #[test]
    fn public_counter_profile_builders_are_composable_with_rapl() {
        let compact = LinuxPerfBackend::new().with_compact_counters();
        let mut rapl_only = LinuxPerfBackend::new()
            .without_cpu_counters()
            .with_rapl_energy();

        assert_eq!(compact.pmu_counter_profile(), PmuCounterProfile::Compact);
        assert_eq!(compact.measurement_label(), "timing + compact PMU");
        assert_eq!(rapl_only.pmu_counter_profile(), PmuCounterProfile::None);
        rapl_only.rapl_observed = true;
        assert_eq!(rapl_only.measurement_label(), "timing + RAPL energy");
        assert!(!rapl_only.emits_cpu_diagnostics());
    }

    #[test]
    fn public_rapl_builders_have_distinct_persisted_identity() {
        let package = LinuxPerfBackend::new().with_rapl_energy();
        let mut core = LinuxPerfBackend::new().with_rapl_core_energy();

        assert_eq!(package.energy_scope, EnergyScope::RaplPackageDomains);
        assert_eq!(core.energy_scope, EnergyScope::RaplPackageAndCore);
        assert_eq!(package.energy_scope(), EnergyScope::None);
        assert_eq!(core.energy_scope(), EnergyScope::None);
        core.rapl_observed = true;
        assert_eq!(core.energy_scope(), EnergyScope::RaplPackageDomains);
        core.rapl_core_observed = true;
        assert_eq!(core.energy_scope(), EnergyScope::RaplPackageAndCore);
    }
}
