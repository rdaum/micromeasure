//! Linux uncore memory-controller bandwidth measurement.
//!
//! The kernel exposes one dynamic PMU directory for each IMC channel number
//! and advertises one representative CPU per package in `cpumask`. Opening
//! every `(PMU directory, advertised CPU)` pair therefore covers every
//! channel on every package without opening duplicate counters on all CPUs.

use super::backend::{MemoryBandwidthScope, MetricValue};
use super::perf::{
    current_perf_issues, parse_cpu_list, record_perf_issue, scale_multiplexed_count,
};
use perf_event::Builder;
use perf_event::events::Dynamic;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

const SYSFS_PMU_ROOT: &str = "/sys/bus/event_source/devices";
const READ_EVENT: &str = "cas_count_read";
const WRITE_EVENT: &str = "cas_count_write";
const METRIC_SECTION: &str = "System memory bandwidth";
const MIN_RELIABLE_SCHEDULED_PERCENT: f64 = 25.0;
const MIN_RELIABLE_RUNNING_TIME: Duration = Duration::from_millis(10);

#[derive(Clone)]
struct ImcConfig {
    name: String,
    read_event: Dynamic,
    read_scale_bytes: f64,
    write_event: Dynamic,
    write_scale_bytes: f64,
    cpus: Vec<usize>,
}

#[derive(Clone, Default)]
pub(super) struct MemoryBandwidthDiscovery {
    configs: Vec<ImcConfig>,
    expected_targets: usize,
    complete: bool,
}

struct ImcCounterPair {
    name: String,
    cpu: usize,
    read_scale_bytes: f64,
    write_scale_bytes: f64,
    read: perf_event::Counter,
    write: perf_event::Counter,
}

pub(super) struct MemoryBandwidthMeasurement {
    counters: Vec<ImcCounterPair>,
    expected_targets: usize,
    complete: bool,
}

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct MemoryBandwidthSample {
    pub read_bytes: f64,
    pub write_bytes: f64,
    pub scheduled_percent: f64,
    pub least_running_ns: u64,
    pub coverage_percent: f64,
    pub scope: MemoryBandwidthScope,
}

fn byte_multiplier(unit: &str) -> Option<f64> {
    match unit.trim().to_ascii_lowercase().as_str() {
        "b" | "byte" | "bytes" => Some(1.0),
        "kb" => Some(1_000.0),
        "kib" => Some(1024.0),
        "mb" => Some(1_000_000.0),
        "mib" => Some(1024.0 * 1024.0),
        "gb" => Some(1_000_000_000.0),
        "gib" => Some(1024.0 * 1024.0 * 1024.0),
        _ => None,
    }
}

fn is_imc_pmu_name(name: &str) -> bool {
    name.strip_prefix("uncore_imc_").is_some_and(|suffix| {
        !suffix.is_empty() && suffix.bytes().all(|byte| byte.is_ascii_digit())
    })
}

fn configure_event(pmu_path: &Path, event_name: &str) -> Result<(Dynamic, f64), String> {
    let mut builder = Dynamic::builder(pmu_path).map_err(|error| error.to_string())?;
    builder
        .event(event_name)
        .map_err(|error| error.to_string())?;
    // perf's symbolic event syntax treats format fields omitted by the event
    // alias as zero. `perf-event2::DynamicBuilder` instead requires every
    // format field to be assigned before `build`, so supply those equivalent
    // zero defaults (for example `edge`, `inv`, and `thresh` on Intel IMCs).
    let unset_fields: Vec<String> = builder.params().map(str::to_owned).collect();
    for field in unset_fields {
        builder
            .field(&field, 0)
            .map_err(|error| error.to_string())?;
    }

    let scale = builder
        .scale()
        .map_err(|error| error.to_string())?
        .filter(|scale| scale.is_finite() && *scale > 0.0)
        .ok_or_else(|| "event has no usable scale".to_string())?;
    let unit = builder
        .unit()
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "event has no unit".to_string())?;
    let multiplier =
        byte_multiplier(&unit).ok_or_else(|| format!("unsupported event unit {unit:?}"))?;
    let scale_bytes = scale * multiplier;
    if !scale_bytes.is_finite() || scale_bytes <= 0.0 {
        return Err("event byte scale is not finite and positive".to_string());
    }

    let event = builder.build().map_err(|error| error.to_string())?;
    Ok((event, scale_bytes))
}

pub(super) fn discover_memory_bandwidth() -> MemoryBandwidthDiscovery {
    discover_memory_bandwidth_at(Path::new(SYSFS_PMU_ROOT))
}

fn discover_memory_bandwidth_at(root: &Path) -> MemoryBandwidthDiscovery {
    let mut discovery = MemoryBandwidthDiscovery {
        complete: true,
        ..MemoryBandwidthDiscovery::default()
    };
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) => {
            record_perf_issue(format!("IMC PMU discovery failed: {error}"));
            discovery.complete = false;
            return discovery;
        }
    };

    let mut devices = Vec::<(String, PathBuf)>::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if is_imc_pmu_name(&name) && entry.path().is_dir() {
            devices.push((name, entry.path()));
        }
    }
    devices.sort_by(|left, right| left.0.cmp(&right.0));
    if devices.is_empty() {
        discovery.complete = false;
        return discovery;
    }

    for (name, path) in devices {
        let cpus = match fs::read_to_string(path.join("cpumask"))
            .map_err(|error| error.to_string())
            .and_then(|value| parse_cpu_list(&value))
        {
            Ok(cpus) => cpus,
            Err(error) => {
                // Preserve a non-zero denominator so malformed advertised
                // scope can never be mistaken for complete coverage.
                discovery.expected_targets += 1;
                discovery.complete = false;
                record_perf_issue(format!("IMC PMU '{name}' CPU scope unavailable: {error}"));
                continue;
            }
        };
        discovery.expected_targets += cpus.len();

        let read = configure_event(&path, READ_EVENT);
        let write = configure_event(&path, WRITE_EVENT);
        match (read, write) {
            (Ok((read_event, read_scale_bytes)), Ok((write_event, write_scale_bytes))) => {
                discovery.configs.push(ImcConfig {
                    name,
                    read_event,
                    read_scale_bytes,
                    write_event,
                    write_scale_bytes,
                    cpus,
                });
            }
            (read, write) => {
                discovery.complete = false;
                if let Err(error) = read {
                    record_perf_issue(format!(
                        "IMC PMU '{name}/{READ_EVENT}' unavailable: {error}"
                    ));
                }
                if let Err(error) = write {
                    record_perf_issue(format!(
                        "IMC PMU '{name}/{WRITE_EVENT}' unavailable: {error}"
                    ));
                }
            }
        }
    }

    discovery
}

fn build_counter(event: Dynamic, cpu: usize) -> std::io::Result<perf_event::Counter> {
    let mut builder = Builder::new(event);
    builder
        .any_pid()
        .one_cpu(cpu)
        .exclude_kernel(false)
        .exclude_hv(false);
    builder.build()
}

pub(super) fn prepare_memory_bandwidth(
    discovery: &MemoryBandwidthDiscovery,
) -> MemoryBandwidthMeasurement {
    let mut measurement = MemoryBandwidthMeasurement {
        counters: Vec::new(),
        expected_targets: discovery.expected_targets,
        complete: discovery.complete,
    };
    for config in &discovery.configs {
        for &cpu in &config.cpus {
            let read = build_counter(config.read_event, cpu);
            let write = build_counter(config.write_event, cpu);
            match (read, write) {
                (Ok(read), Ok(write)) => measurement.counters.push(ImcCounterPair {
                    name: config.name.clone(),
                    cpu,
                    read_scale_bytes: config.read_scale_bytes,
                    write_scale_bytes: config.write_scale_bytes,
                    read,
                    write,
                }),
                (read, write) => {
                    measurement.complete = false;
                    if let Err(error) = read {
                        record_perf_issue(format!(
                            "IMC read counter '{}' on CPU {cpu} unavailable: {error}",
                            config.name
                        ));
                    }
                    if let Err(error) = write {
                        record_perf_issue(format!(
                            "IMC write counter '{}' on CPU {cpu} unavailable: {error}",
                            config.name
                        ));
                    }
                }
            }
        }
    }
    if measurement.counters.len() != measurement.expected_targets {
        measurement.complete = false;
    }
    measurement
}

impl MemoryBandwidthMeasurement {
    pub(super) fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }

    pub(super) fn is_complete(&self) -> bool {
        self.complete && !self.counters.is_empty()
    }

    pub(super) fn begin(&mut self) {
        self.counters.retain_mut(|pair| {
            let reset = pair.read.reset().and_then(|_| pair.write.reset());
            if let Err(error) = reset {
                record_perf_issue(format!(
                    "IMC counter '{}' on CPU {} reset failed: {error}",
                    pair.name, pair.cpu
                ));
                return false;
            }
            if let Err(error) = pair.read.enable() {
                record_perf_issue(format!(
                    "IMC read counter '{}' on CPU {} enable failed: {error}",
                    pair.name, pair.cpu
                ));
                return false;
            }
            if let Err(error) = pair.write.enable() {
                let _ = pair.read.disable();
                record_perf_issue(format!(
                    "IMC write counter '{}' on CPU {} enable failed: {error}",
                    pair.name, pair.cpu
                ));
                return false;
            }
            true
        });
        if self.counters.len() != self.expected_targets {
            self.complete = false;
        }
    }

    pub(super) fn end(&mut self) {
        for pair in &mut self.counters {
            if let Err(error) = pair.read.disable() {
                record_perf_issue(format!(
                    "IMC read counter '{}' on CPU {} disable failed: {error}",
                    pair.name, pair.cpu
                ));
            }
            if let Err(error) = pair.write.disable() {
                record_perf_issue(format!(
                    "IMC write counter '{}' on CPU {} disable failed: {error}",
                    pair.name, pair.cpu
                ));
            }
        }
    }

    pub(super) fn collect(&mut self) -> MemoryBandwidthSample {
        let mut sample = MemoryBandwidthSample::default();
        let mut usable_targets = 0usize;
        let mut least_scheduled: Option<(u64, u64)> = None;

        for pair in &mut self.counters {
            let read = pair.read.read_count_and_time();
            let write = pair.write.read_count_and_time();
            let (read, write) = match (read, write) {
                (Ok(read), Ok(write))
                    if read.time_enabled > 0
                        && read.time_running > 0
                        && write.time_enabled > 0
                        && write.time_running > 0 =>
                {
                    (read, write)
                }
                (Ok(_), Ok(_)) => {
                    record_perf_issue(format!(
                        "IMC counter pair '{}' on CPU {} has no usable scheduled window",
                        pair.name, pair.cpu
                    ));
                    continue;
                }
                (read, write) => {
                    if let Err(error) = read {
                        record_perf_issue(format!(
                            "IMC read counter '{}' on CPU {} read failed: {error}",
                            pair.name, pair.cpu
                        ));
                    }
                    if let Err(error) = write {
                        record_perf_issue(format!(
                            "IMC write counter '{}' on CPU {} read failed: {error}",
                            pair.name, pair.cpu
                        ));
                    }
                    continue;
                }
            };

            usable_targets += 1;
            sample.read_bytes += scaled_bytes(
                read.count,
                read.time_enabled,
                read.time_running,
                pair.read_scale_bytes,
            );
            sample.write_bytes += scaled_bytes(
                write.count,
                write.time_enabled,
                write.time_running,
                pair.write_scale_bytes,
            );
            for timing in [
                (read.time_enabled, read.time_running),
                (write.time_enabled, write.time_running),
            ] {
                if least_scheduled.is_none_or(|current| less_scheduled(timing, current)) {
                    least_scheduled = Some(timing);
                }
            }
        }

        sample.coverage_percent = if self.expected_targets == 0 {
            0.0
        } else {
            usable_targets as f64 / self.expected_targets as f64 * 100.0
        };
        sample.scheduled_percent = least_scheduled
            .map(|(enabled, running)| running as f64 / enabled as f64 * 100.0)
            .unwrap_or(0.0);
        sample.least_running_ns = least_scheduled.map(|(_, running)| running).unwrap_or(0);
        sample.scope = if usable_targets == 0 {
            MemoryBandwidthScope::SystemUnavailable
        } else if self.complete && usable_targets == self.expected_targets {
            MemoryBandwidthScope::SystemComplete
        } else {
            MemoryBandwidthScope::SystemPartial
        };
        sample
    }
}

fn less_scheduled(left: (u64, u64), right: (u64, u64)) -> bool {
    (left.1 as u128 * right.0 as u128) < (right.1 as u128 * left.0 as u128)
}

fn scaled_bytes(raw: u64, enabled: u64, running: u64, scale_bytes: f64) -> f64 {
    scale_multiplexed_count(raw, enabled, running) as f64 * scale_bytes
}

pub(super) fn push_memory_bandwidth_metrics(
    sample: MemoryBandwidthSample,
    operations: u64,
    elapsed: Duration,
    metrics: &mut Vec<MetricValue>,
) {
    if sample.scope == MemoryBandwidthScope::SystemUnavailable {
        return;
    }

    metrics.push(
        MetricValue::new("dram_imc_coverage_percent", sample.coverage_percent, "%")
            .with_display_name("IMC target coverage")
            .with_section(METRIC_SECTION),
    );
    metrics.push(
        MetricValue::new("dram_pmu_scheduled_percent", sample.scheduled_percent, "%")
            .with_display_name("IMC PMU scheduled")
            .with_section(METRIC_SECTION),
    );

    // Partial byte counts are not complete-system totals. Retain coverage and
    // scheduling evidence, but do not emit misleading bandwidth values.
    if sample.scope != MemoryBandwidthScope::SystemComplete {
        return;
    }
    if sample.scheduled_percent < MIN_RELIABLE_SCHEDULED_PERCENT
        || sample.least_running_ns < MIN_RELIABLE_RUNNING_TIME.as_nanos() as u64
    {
        warn_memory_bandwidth_quality_once(sample);
    }

    let total_bytes = sample.read_bytes + sample.write_bytes;
    if operations > 0 {
        for (name, display_name, bytes) in [
            (
                "dram_read_bytes_per_op",
                "DRAM read bytes/op",
                sample.read_bytes,
            ),
            (
                "dram_write_bytes_per_op",
                "DRAM write bytes/op",
                sample.write_bytes,
            ),
            (
                "dram_total_bytes_per_op",
                "DRAM total bytes/op",
                total_bytes,
            ),
        ] {
            metrics.push(
                MetricValue::new(name, bytes / operations as f64, "B/op")
                    .with_display_name(display_name)
                    .with_section(METRIC_SECTION),
            );
        }
    }
    if elapsed > Duration::ZERO {
        let gib = 1024.0 * 1024.0 * 1024.0;
        for (name, display_name, bytes) in [
            ("dram_read_gib_s", "DRAM read bandwidth", sample.read_bytes),
            (
                "dram_write_gib_s",
                "DRAM write bandwidth",
                sample.write_bytes,
            ),
            ("dram_total_gib_s", "DRAM total bandwidth", total_bytes),
        ] {
            metrics.push(
                MetricValue::new(name, bytes / gib / elapsed.as_secs_f64(), "GiB/s")
                    .with_display_name(display_name)
                    .with_section(METRIC_SECTION),
            );
        }
    }
}

pub(super) fn warn_memory_bandwidth_unavailable_once() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "⚠️  Uncore memory bandwidth counters unavailable; continuing without DRAM bandwidth metrics."
    );
    eprintln!(
        "   This requires Linux uncore_imc_* PMUs with cas_count_read/cas_count_write and system-wide perf access."
    );
    for issue in current_perf_issues()
        .into_iter()
        .filter(|issue| issue.starts_with("IMC "))
        .take(2)
    {
        eprintln!("   {issue}");
    }
}

pub(super) fn warn_memory_bandwidth_partial_once() {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "⚠️  Uncore memory-controller coverage is partial; whole-system DRAM bandwidth metrics will be omitted."
    );
}

fn warn_memory_bandwidth_quality_once(sample: MemoryBandwidthSample) {
    static WARNED: AtomicBool = AtomicBool::new(false);
    if WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    eprintln!(
        "⚠️  Uncore memory bandwidth scheduling quality is low: {:.1}% scheduled, {:.1} ms least-event running time; scaled bandwidth may be unreliable.",
        sample.scheduled_percent,
        sample.least_running_ns as f64 / 1_000_000.0,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn normalizes_kernel_units_to_bytes() {
        assert_eq!(byte_multiplier("bytes"), Some(1.0));
        assert_eq!(byte_multiplier("KiB"), Some(1024.0));
        assert_eq!(byte_multiplier("MiB"), Some(1024.0 * 1024.0));
        assert_eq!(byte_multiplier("GB"), Some(1_000_000_000.0));
        assert_eq!(byte_multiplier("transactions"), None);
    }

    #[test]
    fn recognizes_only_numbered_imc_pmus() {
        assert!(is_imc_pmu_name("uncore_imc_0"));
        assert!(is_imc_pmu_name("uncore_imc_12"));
        assert!(!is_imc_pmu_name("uncore_imc_free_running_0"));
        assert!(!is_imc_pmu_name("uncore_imc_"));
    }

    #[test]
    fn complete_samples_emit_bytes_and_bandwidth() {
        let sample = MemoryBandwidthSample {
            read_bytes: 3.0 * 1024.0 * 1024.0 * 1024.0,
            write_bytes: 1.0 * 1024.0 * 1024.0 * 1024.0,
            scheduled_percent: 80.0,
            least_running_ns: 20_000_000,
            coverage_percent: 100.0,
            scope: MemoryBandwidthScope::SystemComplete,
        };
        let mut metrics = Vec::new();
        push_memory_bandwidth_metrics(sample, 1024, Duration::from_secs(2), &mut metrics);
        let value = |name| {
            metrics
                .iter()
                .find(|metric| metric.name == name)
                .map(|metric| metric.value)
                .unwrap()
        };
        assert_eq!(value("dram_read_gib_s"), 1.5);
        assert_eq!(value("dram_write_gib_s"), 0.5);
        assert_eq!(value("dram_total_gib_s"), 2.0);
        assert_eq!(value("dram_imc_coverage_percent"), 100.0);
        assert!(
            metrics
                .iter()
                .all(|metric| metric.section == METRIC_SECTION)
        );
    }

    #[test]
    fn multiplex_scaling_aggregates_across_imc_targets() {
        // Two target readings represent the same event opened on two package
        // CPUs. The first ran continuously; the second ran for half its
        // enabled window and must be scaled by 2 before both are summed.
        let bytes = scaled_bytes(100, 1_000, 1_000, 64.0) + scaled_bytes(50, 1_000, 500, 64.0);
        assert_eq!(bytes, 12_800.0);
    }

    #[test]
    fn partial_samples_do_not_manufacture_whole_system_bandwidth() {
        let sample = MemoryBandwidthSample {
            read_bytes: 1024.0,
            write_bytes: 512.0,
            scheduled_percent: 75.0,
            least_running_ns: 20_000_000,
            coverage_percent: 50.0,
            scope: MemoryBandwidthScope::SystemPartial,
        };
        let mut metrics = Vec::new();
        push_memory_bandwidth_metrics(sample, 1, Duration::from_secs(1), &mut metrics);
        assert_eq!(metrics.len(), 2);
        assert!(
            metrics
                .iter()
                .all(|metric| !metric.name.ends_with("_gib_s") && !metric.name.contains("bytes"))
        );
    }

    #[test]
    fn unavailable_samples_emit_no_zero_bandwidth_metrics() {
        let mut metrics = Vec::new();
        push_memory_bandwidth_metrics(
            MemoryBandwidthSample {
                scope: MemoryBandwidthScope::SystemUnavailable,
                ..MemoryBandwidthSample::default()
            },
            1,
            Duration::from_secs(1),
            &mut metrics,
        );
        assert!(metrics.is_empty());
    }

    #[test]
    fn fixture_discovery_parses_event_scale_unit_and_cpumask() {
        let root = std::env::temp_dir().join(format!(
            "micromeasure-imc-fixture-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        let pmu = root.join("uncore_imc_0");
        fs::create_dir_all(pmu.join("format")).unwrap();
        fs::create_dir_all(pmu.join("events")).unwrap();
        fs::write(pmu.join("type"), "19\n").unwrap();
        fs::write(pmu.join("format/event"), "config:0-7\n").unwrap();
        fs::write(pmu.join("format/umask"), "config:8-15\n").unwrap();
        fs::write(pmu.join("format/edge"), "config:18\n").unwrap();
        fs::write(pmu.join("format/inv"), "config:23\n").unwrap();
        fs::write(pmu.join("format/thresh"), "config:24-31\n").unwrap();
        fs::write(pmu.join("cpumask"), "0,12\n").unwrap();
        fs::write(pmu.join("events/cas_count_read"), "event=0x04,umask=0x03\n").unwrap();
        fs::write(pmu.join("events/cas_count_read.scale"), "6.103515625e-5\n").unwrap();
        fs::write(pmu.join("events/cas_count_read.unit"), "MiB\n").unwrap();
        fs::write(
            pmu.join("events/cas_count_write"),
            "event=0x04,umask=0x0c\n",
        )
        .unwrap();
        fs::write(pmu.join("events/cas_count_write.scale"), "6.103515625e-5\n").unwrap();
        fs::write(pmu.join("events/cas_count_write.unit"), "MiB\n").unwrap();

        let discovery = discover_memory_bandwidth_at(&root);
        assert!(discovery.complete);
        assert_eq!(discovery.expected_targets, 2);
        assert_eq!(discovery.configs.len(), 1);
        assert_eq!(discovery.configs[0].cpus, vec![0, 12]);
        assert_eq!(discovery.configs[0].read_scale_bytes, 64.0);
        assert_eq!(discovery.configs[0].write_scale_bytes, 64.0);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fixture_discovery_marks_missing_event_as_partial() {
        let root = std::env::temp_dir().join(format!(
            "micromeasure-imc-missing-fixture-{}",
            std::process::id()
        ));
        let pmu = root.join("uncore_imc_0");
        fs::create_dir_all(pmu.join("format")).unwrap();
        fs::create_dir_all(pmu.join("events")).unwrap();
        fs::write(pmu.join("type"), "19\n").unwrap();
        fs::write(pmu.join("format/event"), "config:0-7\n").unwrap();
        fs::write(pmu.join("cpumask"), "0\n").unwrap();
        fs::write(pmu.join("events/cas_count_read"), "event=0x04\n").unwrap();
        fs::write(pmu.join("events/cas_count_read.scale"), "64\n").unwrap();
        fs::write(pmu.join("events/cas_count_read.unit"), "bytes\n").unwrap();

        let discovery = discover_memory_bandwidth_at(&root);
        assert!(!discovery.complete);
        assert_eq!(discovery.expected_targets, 1);
        assert!(discovery.configs.is_empty());

        fs::remove_dir_all(root).unwrap();
    }
}
