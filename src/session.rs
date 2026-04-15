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
    pub mops_per_sec: f64,
    pub median_mops_per_sec: f64,
    pub ns_per_op: f64,
    pub median_ns_per_op: f64,
    pub p95_ns_per_op: f64,
    pub mad_ns_per_op: f64,
    pub instructions_per_op: f64,
    pub branches_per_op: f64,
    pub branch_miss_rate: f64,     // percentage of branches mispredicted
    pub branch_misses_per_op: f64, // branch misses per operation
    pub cache_miss_rate: f64,
    pub cv_percent: f64,
    pub outlier_count: usize,
    pub samples: usize,
    pub operations: u64,
    pub total_duration_sec: f64,
    #[serde(default)]
    pub sample_throughput_mops_per_sec: Vec<f64>,
    #[serde(default)]
    pub sample_latency_ns_per_op: Vec<f64>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BenchmarkKind {
    Standard,
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
                            let change =
                                safe_percent_change(result.mops_per_sec, prev_result.mops_per_sec);
                            format_percent_change(change)
                        })
                        .unwrap_or_else(|| "NEW".to_string())
                } else {
                    "-".to_string()
                };

                table.add_row(vec![
                    &colorize_label(&result.name),
                    &colorize_value(&format!("{:.1}", result.mops_per_sec)),
                    &colorize_value(&format!("{:.2}", result.median_ns_per_op)),
                    &colorize_value(&format!("{:.2}", result.p95_ns_per_op)),
                    &change_info,
                ]);
            }

            table.print();
            println!();

            if let Some(ref prev) = previous_session {
                let comparable_results: Vec<_> = group_results
                    .iter()
                    .filter_map(|result| matching_previous_result(prev, result).map(|previous| (*result, previous)))
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

                    for (result, previous) in comparable_results {
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
                                result.cv_percent,
                                previous.cv_percent,
                                true,
                            )),
                        ]);
                    }

                    println!("   Per-stat comparison:");
                    comparison_table.print();
                    println!();
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
                    && let Some(change) =
                        safe_percent_change(result.mops_per_sec, prev_result.mops_per_sec)
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

fn matching_previous_result<'a>(
    previous_report: &'a BenchmarkReport,
    result: &BenchmarkResult,
) -> Option<&'a BenchmarkResult> {
    previous_report
        .results
        .iter()
        .find(|previous| previous.name == result.name)
}

fn result_mean_throughput(result: &BenchmarkResult) -> f64 {
    if result.sample_throughput_mops_per_sec.is_empty() {
        return result.mops_per_sec;
    }

    result.sample_throughput_mops_per_sec.iter().sum::<f64>()
        / result.sample_throughput_mops_per_sec.len() as f64
}

fn result_median_latency(result: &BenchmarkResult) -> f64 {
    if result.sample_latency_ns_per_op.is_empty() {
        return result.median_ns_per_op;
    }

    let mut samples = result.sample_latency_ns_per_op.clone();
    samples.sort_by(|a, b| a.total_cmp(b));
    percentile_latency(&samples, 0.5)
}

fn result_p95_latency(result: &BenchmarkResult) -> f64 {
    if result.sample_latency_ns_per_op.is_empty() {
        return result.p95_ns_per_op;
    }

    let mut samples = result.sample_latency_ns_per_op.clone();
    samples.sort_by(|a, b| a.total_cmp(b));
    percentile_latency(&samples, 0.95)
}

fn result_mad_latency(result: &BenchmarkResult) -> f64 {
    if result.sample_latency_ns_per_op.is_empty() {
        return result.mad_ns_per_op;
    }

    let median = result_median_latency(result);
    median_absolute_deviation_latency(&result.sample_latency_ns_per_op, median)
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

    let mut deviations: Vec<f64> = values.iter().map(|value| (value - median_value).abs()).collect();
    deviations.sort_by(|a, b| a.total_cmp(b));
    percentile_latency(&deviations, 0.5)
}

fn finite_mops(result: &BenchmarkResult) -> Option<f64> {
    let mops = result.mops_per_sec;
    if mops.is_finite() && mops >= 0.0 {
        Some(mops)
    } else {
        None
    }
}

fn default_suite_name() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.file_stem().map(|name| name.to_string_lossy().into_owned()))
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

    let current_names: BTreeSet<&str> = current_results.iter().map(|result| result.name.as_str()).collect();
    let previous_names: BTreeSet<&str> = session.results.iter().map(|result| result.name.as_str()).collect();
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
            mops_per_sec,
            median_mops_per_sec: mops_per_sec,
            ns_per_op: 1.0,
            median_ns_per_op: 1.0,
            p95_ns_per_op: 1.0,
            mad_ns_per_op: 0.0,
            instructions_per_op: 1.0,
            branches_per_op: 1.0,
            branch_miss_rate: 0.0,
            branch_misses_per_op: 0.0,
            cache_miss_rate: 0.0,
            cv_percent: 0.0,
            outlier_count: 0,
            samples: 1,
            operations: 1,
            total_duration_sec: 1.0,
            sample_throughput_mops_per_sec: vec![mops_per_sec],
            sample_latency_ns_per_op: vec![1.0],
        }
    }

    fn make_session(
        hostname: &str,
        suite: Option<&str>,
        result_names: &[&str],
    ) -> BenchmarkReport {
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
