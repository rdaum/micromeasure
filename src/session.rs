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

use crate::{Alignment, TableFormatter};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{self, File},
    io::IsTerminal,
    path::PathBuf,
    sync::Mutex,
    time::{SystemTime, UNIX_EPOCH},
};

/// Collected benchmark result for session summary and JSON export
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkResult {
    pub name: String,
    pub group: String,
    pub kind: BenchmarkKind,
    #[serde(flatten)]
    pub stats: BenchmarkStats,
    #[serde(default)]
    pub worker_summaries: Vec<WorkerSummary>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct BenchmarkStats {
    pub mops_per_sec: f64,
    pub median_mops_per_sec: f64,
    pub ns_per_op: f64,
    pub median_ns_per_op: f64,
    pub p95_ns_per_op: f64,
    pub mad_ns_per_op: f64,
    pub cycles_per_op: f64,
    pub instructions_per_op: f64,
    pub ipc: f64,
    pub cache_references_per_op: f64,
    pub l1i_misses_per_op: f64,
    pub branches_per_op: f64,
    pub branch_miss_rate: f64,
    pub branch_misses_per_op: f64,
    pub cache_misses_per_op: f64,
    pub cache_miss_percent: f64,
    pub frontend_stall_cycles_per_op: f64,
    pub frontend_stall_percent: f64,
    pub backend_stall_cycles_per_op: f64,
    pub backend_stall_percent: f64,
    pub cv_percent: f64,
    pub outlier_count: usize,
    pub samples: usize,
    pub operations: u64,
    pub total_duration_sec: f64,
    #[serde(default)]
    pub sample_throughput_mops_per_sec: Vec<f64>,
    #[serde(default)]
    pub sample_latency_ns_per_op: Vec<f64>,
    #[serde(default)]
    pub has_cycles: bool,
    #[serde(default)]
    pub has_instructions: bool,
    #[serde(default)]
    pub has_cache_references: bool,
    #[serde(default)]
    pub has_l1i_misses: bool,
    #[serde(default)]
    pub has_branches: bool,
    #[serde(default)]
    pub has_branch_misses: bool,
    #[serde(default)]
    pub has_cache_misses: bool,
    #[serde(default)]
    pub has_stalled_cycles_frontend: bool,
    #[serde(default)]
    pub has_stalled_cycles_backend: bool,
    #[serde(default)]
    pub pmu_time_enabled_ns: u64,
    #[serde(default)]
    pub pmu_time_running_ns: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerCounterSummary {
    pub name: String,
    pub total: u64,
    pub per_op: f64,
    pub per_sec: f64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct WorkerSummary {
    pub name: String,
    pub threads: usize,
    #[serde(flatten)]
    pub stats: BenchmarkStats,
    #[serde(default)]
    pub counters: Vec<WorkerCounterSummary>,
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkKind {
    Standard,
    Concurrent,
}

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum ComparisonPolicy {
    #[default]
    None,
    LatestCompatible,
}

/// Persisted benchmark report for serialization and optional comparisons.
#[derive(Serialize, Deserialize)]
pub struct BenchmarkReport {
    pub timestamp: String,
    pub hostname: String,
    #[serde(default)]
    pub suite: Option<String>,
    pub git_commit: Option<String>,
    pub results: Vec<BenchmarkResult>,
}

/// A live benchmark session that collects results
pub(crate) struct BenchmarkSession {
    timestamp: String,
    hostname: String,
    suite: String,
    git_commit: Option<String>,
    results: Mutex<Vec<BenchmarkResult>>,
}

impl Default for BenchmarkSession {
    fn default() -> Self {
        Self::new()
    }
}

impl BenchmarkSession {
    pub(crate) fn new() -> Self {
        Self::new_with_suite(default_suite_name())
    }

    pub(crate) fn new_with_suite(suite: impl Into<String>) -> Self {
        let timestamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs()
            .to_string();

        let hostname = std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("COMPUTERNAME"))
            .unwrap_or_else(|_| "unknown".to_string());

        let git_commit = std::process::Command::new("git")
            .args(["rev-parse", "--short", "HEAD"])
            .output()
            .ok()
            .and_then(|output| {
                if output.status.success() {
                    String::from_utf8(output.stdout)
                        .ok()
                        .map(|s| s.trim().to_string())
                } else {
                    None
                }
            });

        let suite = suite.into();

        Self {
            timestamp,
            hostname,
            suite,
            git_commit,
            results: Mutex::new(Vec::new()),
        }
    }

    pub(crate) fn add_result(&self, result: BenchmarkResult) {
        if let Ok(mut results) = self.results.lock() {
            results.push(result);
        }
    }

    pub(crate) fn get_results(&self) -> Vec<BenchmarkResult> {
        self.results
            .lock()
            .map(|results| results.clone())
            .unwrap_or_default()
    }

    #[cfg(test)]
    pub(crate) fn clear(&self) {
        if let Ok(mut results) = self.results.lock() {
            results.clear();
        }
    }

    pub(crate) fn report(&self) -> BenchmarkReport {
        BenchmarkReport {
            timestamp: self.timestamp.clone(),
            hostname: self.hostname.clone(),
            suite: Some(self.suite.clone()),
            git_commit: self.git_commit.clone(),
            results: self.get_results(),
        }
    }
}

impl BenchmarkReport {
    pub fn print_summary(&self) {
        self.print_summary_with(ComparisonPolicy::None);
    }

    pub fn print_summary_with(&self, comparison_policy: ComparisonPolicy) {
        if self.results.is_empty() {
            return;
        }

        println!("\n🎯 BENCHMARK SESSION SUMMARY");
        println!("═══════════════════════════════════════════════════════════════════════");

        let previous_session = self.suite.as_deref().and_then(|suite| {
            load_comparison_session(comparison_policy, &self.hostname, suite, &self.results)
        });

        if let Some(ref prev) = previous_session {
            println!("📊 Comparing with previous run from {}", prev.timestamp);
            if let Some(ref suite) = prev.suite {
                println!("   Previous suite: {suite}");
            }
            if let Some(ref commit) = prev.git_commit {
                println!("   Previous commit: {commit}");
            }
            println!();
        }

        let mut groups: BTreeMap<String, Vec<&BenchmarkResult>> = BTreeMap::new();
        for result in &self.results {
            groups.entry(result.group.clone()).or_default().push(result);
        }

        for (group_name, group_results) in groups {
            println!(
                "📈 {} ({} benchmarks)",
                group_name.to_uppercase(),
                group_results.len()
            );

            let mut table = TableFormatter::new(
                vec!["Benchmark", "Mops/s", "Median ns/op", "P95 ns/op", "Change"],
                vec![25, 13, 14, 12, 16],
            )
            .with_alignments(vec![
                Alignment::Left,
                Alignment::Right,
                Alignment::Right,
                Alignment::Right,
                Alignment::Right,
            ]);

            for result in &group_results {
                let change_info = if let Some(ref prev) = previous_session {
                    matching_previous_result(prev, result)
                        .map(|prev_result| {
                            let change = safe_percent_change(
                                result.stats.mops_per_sec,
                                prev_result.stats.mops_per_sec,
                            );
                            format_percent_change(change)
                        })
                        .unwrap_or_else(|| "NEW".to_string())
                } else {
                    "-".to_string()
                };

                table.add_row(vec![
                    &colorize_label(&result.name),
                    &colorize_value(&format!("{:.1}", result.stats.mops_per_sec)),
                    &colorize_value(&format!("{:.2}", result.stats.median_ns_per_op)),
                    &colorize_value(&format!("{:.2}", result.stats.p95_ns_per_op)),
                    &change_info,
                ]);
            }

            table.print();
            println!();

            if let Some(ref prev) = previous_session {
                let comparable_results: Vec<_> = group_results
                    .iter()
                    .filter_map(|result| {
                        matching_previous_result(prev, result).map(|previous| (*result, previous))
                    })
                    .collect();

                if !comparable_results.is_empty() {
                    let mut comparison_table = TableFormatter::new(
                        vec!["Benchmark", "Thrpt", "Median", "P95", "MAD", "CV"],
                        vec![25, 10, 10, 10, 10, 10],
                    )
                    .with_alignments(vec![
                        Alignment::Left,
                        Alignment::Right,
                        Alignment::Right,
                        Alignment::Right,
                        Alignment::Right,
                        Alignment::Right,
                    ]);

                    for &(result, previous) in &comparable_results {
                        comparison_table.add_row(vec![
                            &colorize_label(&result.name),
                            &format_improvement(safe_improvement_percent(
                                result_mean_throughput(result),
                                result_mean_throughput(previous),
                                false,
                            )),
                            &format_improvement(safe_improvement_percent(
                                result_median_latency(result),
                                result_median_latency(previous),
                                true,
                            )),
                            &format_improvement(safe_improvement_percent(
                                result_p95_latency(result),
                                result_p95_latency(previous),
                                true,
                            )),
                            &format_improvement(safe_improvement_percent(
                                result_mad_latency(result),
                                result_mad_latency(previous),
                                true,
                            )),
                            &format_improvement(safe_improvement_percent(
                                result.stats.cv_percent,
                                previous.stats.cv_percent,
                                true,
                            )),
                        ]);
                    }

                    println!("   Per-stat comparison:");
                    comparison_table.print();
                    println!();

                    let mut pmu_comparison_table = TableFormatter::new(
                        vec![
                            "Benchmark",
                            "IPC",
                            "BrMiss",
                            "Cache",
                            "FE Stall",
                            "BE Stall",
                        ],
                        vec![25, 10, 10, 10, 10, 10],
                    )
                    .with_alignments(vec![
                        Alignment::Left,
                        Alignment::Right,
                        Alignment::Right,
                        Alignment::Right,
                        Alignment::Right,
                        Alignment::Right,
                    ]);

                    let mut any_pmu_deltas = false;
                    for &(result, previous) in &comparable_results {
                        let ipc = pmu_metric_improvement(result, previous, pmu_ipc, false);
                        let branch =
                            pmu_metric_improvement(result, previous, pmu_branch_miss_metric, true);
                        let cache =
                            pmu_metric_improvement(result, previous, pmu_cache_metric, true);
                        let frontend = pmu_metric_improvement(
                            result,
                            previous,
                            pmu_frontend_stall_metric,
                            true,
                        );
                        let backend = pmu_metric_improvement(
                            result,
                            previous,
                            pmu_backend_stall_metric,
                            true,
                        );

                        if ipc.is_some()
                            || branch.is_some()
                            || cache.is_some()
                            || frontend.is_some()
                            || backend.is_some()
                        {
                            any_pmu_deltas = true;
                        }

                        pmu_comparison_table.add_row(vec![
                            &colorize_label(&result.name),
                            &format_improvement(ipc),
                            &format_improvement(branch),
                            &format_improvement(cache),
                            &format_improvement(frontend),
                            &format_improvement(backend),
                        ]);
                    }

                    if any_pmu_deltas {
                        println!("   PMU comparison:");
                        pmu_comparison_table.print();
                        println!();
                    }

                    let comparative_diagnostics: Vec<_> = comparable_results
                        .iter()
                        .filter_map(|&(result, previous)| {
                            let diagnosis = comparative_diagnosis(result, previous);
                            (!diagnosis.is_empty()).then_some((result.name.as_str(), diagnosis))
                        })
                        .collect();

                    if !comparative_diagnostics.is_empty() {
                        println!("   Comparative diagnosis:");
                        for (name, diagnosis) in comparative_diagnostics {
                            println!("   - {}: {}", name, colorize_problem(&diagnosis));
                        }
                        println!();
                    }
                }
            }
        }

        println!("🔍 KEY INSIGHTS:");
        let fastest = self
            .results
            .iter()
            .filter_map(|result| finite_mops(result).map(|mops| (result, mops)))
            .max_by(|a, b| a.1.total_cmp(&b.1));
        let slowest = self
            .results
            .iter()
            .filter_map(|result| finite_mops(result).map(|mops| (result, mops)))
            .min_by(|a, b| a.1.total_cmp(&b.1));

        if let (Some((fast, fast_mops)), Some((slow, slow_mops))) = (fastest, slowest) {
            println!("   🏆 Fastest: {} ({:.1} Mops/s)", fast.name, fast_mops);
            println!("   🐌 Slowest: {} ({:.1} Mops/s)", slow.name, slow_mops);
            if slow_mops > f64::EPSILON {
                println!("   📊 Speed difference: {:.1}x", fast_mops / slow_mops);
            } else {
                println!("   📊 Speed difference: n/a");
            }
        } else {
            println!("   No finite throughput values available for insights.");
        }

        if let Some(ref prev) = previous_session {
            let mut improvements = 0;
            let mut regressions = 0;
            let mut total_change = 0.0;
            let mut comparable_count = 0;

            for result in &self.results {
                if let Some(prev_result) = matching_previous_result(prev, result)
                    && let Some(change) = safe_percent_change(
                        result.stats.mops_per_sec,
                        prev_result.stats.mops_per_sec,
                    )
                {
                    comparable_count += 1;
                    total_change += change;
                    if change > 1.0 {
                        improvements += 1;
                    } else if change < -1.0 {
                        regressions += 1;
                    }
                }
            }

            println!();
            println!("📊 REGRESSION ANALYSIS:");
            println!("   ✅ Improvements: {improvements} benchmarks");
            println!("   ❌ Regressions: {regressions} benchmarks");
            if comparable_count > 0 {
                println!(
                    "   📈 Average change: {:.1}%",
                    total_change / comparable_count as f64
                );
            } else {
                println!("   📈 Average change: n/a");
            }
        }

        println!("═══════════════════════════════════════════════════════════════════════");
    }

    pub fn save_to_default_location(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        if self.results.is_empty() {
            return Err("no benchmark results to save".into());
        }

        let target_dir = get_target_directory();
        fs::create_dir_all(&target_dir)?;

        let filename = target_dir.join(format!("benchmark_results_{}.json", self.timestamp));
        let file = File::create(&filename)?;
        serde_json::to_writer_pretty(file, self)?;
        Ok(filename)
    }
}

fn safe_percent_change(current: f64, previous: f64) -> Option<f64> {
    if !current.is_finite() || !previous.is_finite() || previous.abs() <= f64::EPSILON {
        return None;
    }

    Some(((current - previous) / previous) * 100.0)
}

fn format_percent_change(change: Option<f64>) -> String {
    let Some(change) = change else {
        return "n/a".to_string();
    };

    if change.abs() < 1.0 {
        "~0%".to_string()
    } else if change > 0.0 {
        colorize_change(&format!("+{change:.1}%"), true)
    } else {
        colorize_change(&format!("{change:.1}%"), false)
    }
}

fn safe_improvement_percent(current: f64, previous: f64, lower_is_better: bool) -> Option<f64> {
    if !current.is_finite() || !previous.is_finite() || previous.abs() <= f64::EPSILON {
        return None;
    }

    let raw_change = if lower_is_better {
        (previous - current) / previous
    } else {
        (current - previous) / previous
    };
    Some(raw_change * 100.0)
}

fn format_improvement(change: Option<f64>) -> String {
    let Some(change) = change else {
        return "n/a".to_string();
    };

    if change.abs() < 1.0 {
        "~0%".to_string()
    } else if change > 0.0 {
        colorize_change(&format!("+{change:.1}%"), true)
    } else {
        colorize_change(&format!("{change:.1}%"), false)
    }
}

fn colorize_change(text: &str, improved: bool) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }

    let color = if improved { "32" } else { "31" };
    format!("\x1b[{color}m{text}\x1b[0m")
}

fn colorize_label(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }

    format!("\x1b[36m{text}\x1b[0m")
}

fn colorize_value(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }

    format!("\x1b[97m{text}\x1b[0m")
}

fn colorize_problem(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }

    format!("\x1b[31m{text}\x1b[0m")
}

fn matching_previous_result<'a>(
    previous_report: &'a BenchmarkReport,
    result: &BenchmarkResult,
) -> Option<&'a BenchmarkResult> {
    previous_report
        .results
        .iter()
        .find(|previous| previous.name == result.name && previous.kind == result.kind)
}

fn result_mean_throughput(result: &BenchmarkResult) -> f64 {
    if result.stats.sample_throughput_mops_per_sec.is_empty() {
        return result.stats.mops_per_sec;
    }

    result
        .stats
        .sample_throughput_mops_per_sec
        .iter()
        .sum::<f64>()
        / result.stats.sample_throughput_mops_per_sec.len() as f64
}

fn result_median_latency(result: &BenchmarkResult) -> f64 {
    if result.stats.sample_latency_ns_per_op.is_empty() {
        return result.stats.median_ns_per_op;
    }

    let mut samples = result.stats.sample_latency_ns_per_op.clone();
    samples.sort_by(|a, b| a.total_cmp(b));
    percentile_latency(&samples, 0.5)
}

fn result_p95_latency(result: &BenchmarkResult) -> f64 {
    if result.stats.sample_latency_ns_per_op.is_empty() {
        return result.stats.p95_ns_per_op;
    }

    let mut samples = result.stats.sample_latency_ns_per_op.clone();
    samples.sort_by(|a, b| a.total_cmp(b));
    percentile_latency(&samples, 0.95)
}

fn result_mad_latency(result: &BenchmarkResult) -> f64 {
    if result.stats.sample_latency_ns_per_op.is_empty() {
        return result.stats.mad_ns_per_op;
    }

    let median = result_median_latency(result);
    median_absolute_deviation_latency(&result.stats.sample_latency_ns_per_op, median)
}

fn pmu_metric_improvement(
    current: &BenchmarkResult,
    previous: &BenchmarkResult,
    metric: impl Fn(&BenchmarkStats) -> Option<f64>,
    lower_is_better: bool,
) -> Option<f64> {
    let current = metric(&current.stats)?;
    let previous = metric(&previous.stats)?;
    safe_improvement_percent(current, previous, lower_is_better)
}

fn pmu_ipc(stats: &BenchmarkStats) -> Option<f64> {
    (stats.has_cycles && stats.has_instructions && stats.ipc.is_finite() && stats.ipc > 0.0)
        .then_some(stats.ipc)
}

fn pmu_branch_miss_metric(stats: &BenchmarkStats) -> Option<f64> {
    (stats.has_branches
        && stats.has_branch_misses
        && stats.branch_miss_rate.is_finite()
        && stats.branch_miss_rate >= 0.0)
        .then_some(stats.branch_miss_rate)
}

fn pmu_cache_metric(stats: &BenchmarkStats) -> Option<f64> {
    if stats.has_cache_references && stats.has_cache_misses {
        return (stats.cache_miss_percent.is_finite() && stats.cache_miss_percent >= 0.0)
            .then_some(stats.cache_miss_percent);
    }

    (stats.has_cache_misses
        && stats.cache_misses_per_op.is_finite()
        && stats.cache_misses_per_op >= 0.0)
        .then_some(stats.cache_misses_per_op)
}

fn pmu_frontend_stall_metric(stats: &BenchmarkStats) -> Option<f64> {
    (stats.has_cycles
        && stats.has_stalled_cycles_frontend
        && stats.frontend_stall_percent.is_finite()
        && stats.frontend_stall_percent >= 0.0)
        .then_some(stats.frontend_stall_percent)
}

fn pmu_backend_stall_metric(stats: &BenchmarkStats) -> Option<f64> {
    (stats.has_cycles
        && stats.has_stalled_cycles_backend
        && stats.backend_stall_percent.is_finite()
        && stats.backend_stall_percent >= 0.0)
        .then_some(stats.backend_stall_percent)
}

fn comparative_diagnosis(current: &BenchmarkResult, previous: &BenchmarkResult) -> String {
    let throughput_change =
        safe_percent_change(current.stats.mops_per_sec, previous.stats.mops_per_sec);
    let ipc_change = safe_percent_change(current.stats.ipc, previous.stats.ipc);
    let instructions_change = safe_percent_change(
        current.stats.instructions_per_op,
        previous.stats.instructions_per_op,
    );
    let frontend_change = safe_percent_change(
        current.stats.frontend_stall_percent,
        previous.stats.frontend_stall_percent,
    );
    let backend_change = safe_percent_change(
        current.stats.backend_stall_percent,
        previous.stats.backend_stall_percent,
    );
    let branch_change = safe_percent_change(
        current.stats.branch_miss_rate,
        previous.stats.branch_miss_rate,
    );
    let cache_percent_change = safe_percent_change(
        current.stats.cache_miss_percent,
        previous.stats.cache_miss_percent,
    );
    let cache_per_op_change = safe_percent_change(
        current.stats.cache_misses_per_op,
        previous.stats.cache_misses_per_op,
    );
    let l1i_change = safe_percent_change(
        current.stats.l1i_misses_per_op,
        previous.stats.l1i_misses_per_op,
    );
    let cv_change = safe_percent_change(current.stats.cv_percent, previous.stats.cv_percent);

    let mut notes = Vec::new();
    let is_regression = matches!(throughput_change, Some(change) if change <= -2.0);
    let is_improvement = matches!(throughput_change, Some(change) if change >= 2.0);

    if (is_regression || is_improvement)
        && matches!(instructions_change, Some(change) if change.abs() <= 5.0)
        && matches!(ipc_change, Some(change) if change.abs() >= 5.0)
    {
        let direction = if is_regression {
            "same work, worse utilization"
        } else {
            "same work, better utilization"
        };
        let ipc = ipc_change.unwrap_or(0.0);
        notes.push(format!(
            "{direction}: instructions/op stayed roughly flat while IPC moved {ipc:+.1}%"
        ));
    }

    if (is_regression || is_improvement)
        && matches!(backend_change, Some(change) if change.abs() >= 10.0)
        && (matches!(cache_percent_change, Some(change) if change.abs() >= 10.0)
            || matches!(cache_per_op_change, Some(change) if change.abs() >= 10.0))
    {
        let direction = if is_regression {
            "memory-latency signature"
        } else {
            "memory-latency relief"
        };
        let backend = backend_change.unwrap_or(0.0);
        let cache = cache_percent_change.or(cache_per_op_change).unwrap_or(0.0);
        notes.push(format!(
            "{direction}: backend stalls moved {backend:+.1}% and cache pressure moved {cache:+.1}%"
        ));
    }

    if (is_regression || is_improvement)
        && matches!(frontend_change, Some(change) if change.abs() >= 10.0)
        && matches!(branch_change, Some(change) if change.abs() >= 10.0)
    {
        let direction = if is_regression {
            "frontend/predictor regression"
        } else {
            "frontend/predictor improvement"
        };
        let frontend = frontend_change.unwrap_or(0.0);
        let branch = branch_change.unwrap_or(0.0);
        notes.push(format!("{direction}: frontend stalls moved {frontend:+.1}% and branch miss rate moved {branch:+.1}%"));
    }

    if (is_regression || is_improvement)
        && matches!(frontend_change, Some(change) if change.abs() >= 10.0)
        && matches!(l1i_change, Some(change) if change.abs() >= 10.0)
    {
        let direction = if is_regression {
            "instruction-cache regression"
        } else {
            "instruction-cache improvement"
        };
        let frontend = frontend_change.unwrap_or(0.0);
        let l1i = l1i_change.unwrap_or(0.0);
        notes.push(format!(
            "{direction}: frontend stalls moved {frontend:+.1}% and L1I misses/op moved {l1i:+.1}%"
        ));
    }

    if (is_regression || is_improvement)
        && matches!(instructions_change, Some(change) if change.abs() >= 5.0)
        && matches!(ipc_change, Some(change) if change.abs() <= 5.0)
    {
        let direction = if is_regression {
            "heavier code path"
        } else {
            "lighter code path"
        };
        let inst = instructions_change.unwrap_or(0.0);
        notes.push(format!(
            "{direction}: instructions/op moved {inst:+.1}% while IPC stayed roughly flat"
        ));
    }

    if matches!(cv_change, Some(change) if change.abs() >= 20.0) {
        let cv = cv_change.unwrap_or(0.0);
        let direction = if cv > 0.0 {
            "stability worsened"
        } else {
            "stability improved"
        };
        notes.push(format!(
            "{direction}: coefficient of variation moved {cv:+.1}%"
        ));
    }

    notes.join("; ")
}

fn percentile_latency(sorted_values: &[f64], percentile: f64) -> f64 {
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

fn median_absolute_deviation_latency(values: &[f64], median_value: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    let mut deviations: Vec<f64> = values
        .iter()
        .map(|value| (value - median_value).abs())
        .collect();
    deviations.sort_by(|a, b| a.total_cmp(b));
    percentile_latency(&deviations, 0.5)
}

fn finite_mops(result: &BenchmarkResult) -> Option<f64> {
    let mops = result.stats.mops_per_sec;
    if mops.is_finite() && mops >= 0.0 {
        Some(mops)
    } else {
        None
    }
}

fn default_suite_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| {
            path.file_stem()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "benchmark".to_string())
}

/// Get the target directory for saving benchmark results, following criterion's approach
fn get_target_directory() -> PathBuf {
    // Check CARGO_TARGET_DIR environment variable first
    if let Some(target_dir) = std::env::var_os("CARGO_TARGET_DIR") {
        return PathBuf::from(target_dir);
    }

    // Try cargo metadata to get target directory
    if let Ok(cargo) = std::env::var("CARGO")
        && let Ok(output) = std::process::Command::new(cargo)
            .args(["metadata", "--format-version", "1"])
            .output()
        && output.status.success()
        && let Ok(metadata_json) = serde_json::from_slice::<serde_json::Value>(&output.stdout)
        && let Some(target_dir) = metadata_json
            .get("target_directory")
            .and_then(serde_json::Value::as_str)
    {
        return PathBuf::from(target_dir);
    }

    // Fallback to ./target
    PathBuf::from("target")
}

fn session_is_compatible(
    session: &BenchmarkReport,
    hostname: &str,
    suite: &str,
    current_results: &[BenchmarkResult],
) -> bool {
    if session.hostname != hostname {
        return false;
    }

    if session.suite.as_deref() != Some(suite) {
        return false;
    }

    let current_names: BTreeSet<(&str, BenchmarkKind)> = current_results
        .iter()
        .map(|result| (result.name.as_str(), result.kind))
        .collect();
    let previous_names: BTreeSet<(&str, BenchmarkKind)> = session
        .results
        .iter()
        .map(|result| (result.name.as_str(), result.kind))
        .collect();
    current_names == previous_names
}

fn load_latest_session() -> Option<BenchmarkReport> {
    let target_dir = get_target_directory();
    if !target_dir.exists() {
        return None;
    }

    let mut json_files: Vec<_> = fs::read_dir(target_dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("benchmark_results_")
                && entry.file_name().to_string_lossy().ends_with(".json")
        })
        .collect();

    // Sort by modification time, newest first
    json_files.sort_by_key(|entry| {
        entry
            .metadata()
            .and_then(|metadata| metadata.modified())
            .unwrap_or(SystemTime::UNIX_EPOCH)
    });
    json_files.reverse();

    // Return the most recent parseable session.
    for entry in json_files {
        let Ok(file) = File::open(entry.path()) else {
            continue;
        };
        if let Ok(session) = serde_json::from_reader(file) {
            return Some(session);
        }
    }

    None
}

fn load_comparison_session(
    comparison_policy: ComparisonPolicy,
    hostname: &str,
    suite: &str,
    current_results: &[BenchmarkResult],
) -> Option<BenchmarkReport> {
    match comparison_policy {
        ComparisonPolicy::None => None,
        ComparisonPolicy::LatestCompatible => load_latest_session()
            .filter(|session| session_is_compatible(session, hostname, suite, current_results)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_result(name: &str, mops_per_sec: f64) -> BenchmarkResult {
        BenchmarkResult {
            name: name.to_string(),
            group: "test".to_string(),
            kind: BenchmarkKind::Standard,
            stats: BenchmarkStats {
                mops_per_sec,
                median_mops_per_sec: mops_per_sec,
                ns_per_op: 1.0,
                median_ns_per_op: 1.0,
                p95_ns_per_op: 1.0,
                mad_ns_per_op: 0.0,
                cycles_per_op: 1.0,
                instructions_per_op: 1.0,
                ipc: 1.0,
                cache_references_per_op: 1.0,
                l1i_misses_per_op: 0.0,
                branches_per_op: 1.0,
                branch_miss_rate: 0.0,
                branch_misses_per_op: 0.0,
                cache_misses_per_op: 0.0,
                cache_miss_percent: 0.0,
                frontend_stall_cycles_per_op: 0.0,
                frontend_stall_percent: 0.0,
                backend_stall_cycles_per_op: 0.0,
                backend_stall_percent: 0.0,
                cv_percent: 0.0,
                outlier_count: 0,
                samples: 1,
                operations: 1,
                total_duration_sec: 1.0,
                sample_throughput_mops_per_sec: vec![mops_per_sec],
                sample_latency_ns_per_op: vec![1.0],
                has_cycles: false,
                has_instructions: false,
                has_cache_references: false,
                has_l1i_misses: false,
                has_branches: false,
                has_branch_misses: false,
                has_cache_misses: false,
                has_stalled_cycles_frontend: false,
                has_stalled_cycles_backend: false,
                pmu_time_enabled_ns: 0,
                pmu_time_running_ns: 0,
            },
            worker_summaries: Vec::new(),
        }
    }

    fn make_session(hostname: &str, suite: Option<&str>, result_names: &[&str]) -> BenchmarkReport {
        BenchmarkReport {
            timestamp: "123".to_string(),
            hostname: hostname.to_string(),
            suite: suite.map(str::to_string),
            git_commit: Some("abc123".to_string()),
            results: result_names
                .iter()
                .map(|name| make_result(name, 1.0))
                .collect(),
        }
    }

    #[test]
    fn safe_percent_change_handles_invalid_input() {
        assert_eq!(safe_percent_change(10.0, 0.0), None);
        assert_eq!(safe_percent_change(f64::NAN, 10.0), None);
        assert_eq!(safe_percent_change(10.0, f64::INFINITY), None);
        assert_eq!(safe_percent_change(12.0, 10.0), Some(20.0));
    }

    #[test]
    fn format_percent_change_handles_none_and_threshold() {
        assert_eq!(format_percent_change(None), "n/a");
        assert_eq!(format_percent_change(Some(0.3)), "~0%");
        assert_eq!(format_percent_change(Some(3.2)), "+3.2%");
        assert_eq!(format_percent_change(Some(-3.2)), "-3.2%");
    }

    #[test]
    fn finite_mops_filters_out_non_finite_values() {
        assert_eq!(finite_mops(&make_result("ok", 1.0)), Some(1.0));
        assert_eq!(finite_mops(&make_result("nan", f64::NAN)), None);
        assert_eq!(finite_mops(&make_result("neg", -1.0)), None);
    }

    #[test]
    fn session_clear_removes_collected_results() {
        let session = BenchmarkSession::new();
        session.add_result(make_result("bench_a", 1.0));
        assert_eq!(session.get_results().len(), 1);

        session.clear();
        assert!(session.get_results().is_empty());
    }

    #[test]
    fn session_compatibility_requires_same_host_suite_and_benchmark_set() {
        let current_results = vec![make_result("bench_a", 1.0), make_result("bench_b", 2.0)];

        let compatible = make_session("host-a", Some("suite-a"), &["bench_b", "bench_a"]);
        assert!(session_is_compatible(
            &compatible,
            "host-a",
            "suite-a",
            &current_results
        ));

        let wrong_host = make_session("host-b", Some("suite-a"), &["bench_a", "bench_b"]);
        assert!(!session_is_compatible(
            &wrong_host,
            "host-a",
            "suite-a",
            &current_results
        ));

        let wrong_suite = make_session("host-a", Some("suite-b"), &["bench_a", "bench_b"]);
        assert!(!session_is_compatible(
            &wrong_suite,
            "host-a",
            "suite-a",
            &current_results
        ));

        let missing_suite = make_session("host-a", None, &["bench_a", "bench_b"]);
        assert!(!session_is_compatible(
            &missing_suite,
            "host-a",
            "suite-a",
            &current_results
        ));

        let different_results = make_session("host-a", Some("suite-a"), &["bench_a"]);
        assert!(!session_is_compatible(
            &different_results,
            "host-a",
            "suite-a",
            &current_results
        ));
    }
}
