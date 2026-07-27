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

use micromeasure::{
    BenchmarkReport, MeasurementDirection, MeasurementKind, ReportContext, SeriesReport,
    SeriesResult, SuiteComparisonAnalysis, Validity,
};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::atomic::{AtomicU64, Ordering},
};

static TEMPORARY_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new(name: &str) -> Self {
        let sequence = TEMPORARY_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "micromeasure-cli-{name}-{}-{sequence}",
            std::process::id()
        ));
        fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn context() -> ReportContext {
    ReportContext::new("fixture-runner").with_environment("hardware", "test-node")
}

fn series(suite: &str, timestamp: &str, samples: Vec<f64>) -> SeriesReport {
    SeriesReport::new(
        timestamp,
        suite,
        context(),
        vec![SeriesResult::new(
            "scenario",
            "latency",
            MeasurementKind::Latency,
            "ms",
            MeasurementDirection::Lower,
            samples,
        )],
    )
}

fn native(suite: &str, timestamp: &str) -> BenchmarkReport {
    let mut report: BenchmarkReport =
        serde_json::from_str(include_str!("../../tests/fixtures/native-report.json")).unwrap();
    report.timestamp = timestamp.to_string();
    report.suite = Some(suite.to_string());
    report
}

fn write_json(path: impl AsRef<Path>, value: &impl Serialize) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn command(arguments: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_micromeasure"));
    command.args(arguments).output().unwrap()
}

#[test]
fn help_and_version_are_successful_informational_exits() {
    assert_eq!(command(&["--help"]).status.code(), Some(0));
    assert_eq!(command(&["--version"]).status.code(), Some(0));
}

#[test]
fn validate_accepts_external_and_native_reports() {
    let root = TemporaryDirectory::new("validate");
    write_json(
        root.path().join("series.json"),
        &series("series-suite", "now", vec![1.0]),
    );
    write_json(
        root.path().join("native.json"),
        &native("native-suite", "now"),
    );

    let output = command(&["validate", root.path().to_str().unwrap()]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("validated 2 reports"));
    assert!(stdout.contains("native-suite [native_benchmark"));
    assert!(stdout.contains("series-suite [series"));
}

#[test]
fn file_comparison_writes_outputs_and_returns_gate_status() {
    let root = TemporaryDirectory::new("gating");
    let current = root.path().join("current.json");
    let baseline = root.path().join("baseline.json");
    let json_output = root.path().join("output/comparison.json");
    let markdown_output = root.path().join("output/comparison.md");
    write_json(
        &current,
        &series("suite", "current", vec![110.0, 120.0, 130.0]),
    );
    write_json(
        &baseline,
        &series("suite", "baseline", vec![90.0, 100.0, 110.0]),
    );

    let output = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--json-output",
        json_output.to_str().unwrap(),
        "--markdown-output",
        markdown_output.to_str().unwrap(),
        "--fail-on-regression",
    ]);

    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8(output.stdout)
            .unwrap()
            .contains("gate: FAIL")
    );
    let analysis: SuiteComparisonAnalysis =
        serde_json::from_slice(&fs::read(json_output).unwrap()).unwrap();
    assert_eq!(analysis.summary.blocking, 1);
    assert!(
        fs::read_to_string(markdown_output)
            .unwrap()
            .contains("# Benchmark suite comparison")
    );
}

#[test]
fn directory_comparison_handles_mixed_types_and_added_suites() {
    let root = TemporaryDirectory::new("directory");
    let current = root.path().join("current");
    let baseline = root.path().join("baseline");
    fs::create_dir(&current).unwrap();
    fs::create_dir(&baseline).unwrap();
    write_json(
        current.join("series.json"),
        &series("series-suite", "current", vec![90.0]),
    );
    write_json(
        baseline.join("series.json"),
        &series("series-suite", "baseline", vec![100.0]),
    );
    write_json(
        current.join("native.json"),
        &native("native-suite", "current"),
    );
    write_json(
        baseline.join("native.json"),
        &native("native-suite", "baseline"),
    );
    write_json(
        current.join("added.json"),
        &series("added-suite", "current", vec![1.0]),
    );

    let output = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
    ]);
    assert_eq!(output.status.code(), Some(0));
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("suites: 2 matched, 1 added, 0 removed"));
}

#[test]
fn malformed_reports_duplicate_suites_and_bad_invocations_return_error_status() {
    let root = TemporaryDirectory::new("errors");
    let malformed = root.path().join("malformed.json");
    fs::write(&malformed, "{not json").unwrap();
    let malformed_output = command(&["validate", malformed.to_str().unwrap()]);
    assert_eq!(malformed_output.status.code(), Some(2));

    let duplicate = root.path().join("duplicate");
    fs::create_dir(&duplicate).unwrap();
    write_json(duplicate.join("a.json"), &series("suite", "a", vec![1.0]));
    write_json(duplicate.join("b.json"), &series("suite", "b", vec![1.0]));
    let duplicate_output = command(&["validate", duplicate.to_str().unwrap()]);
    assert_eq!(duplicate_output.status.code(), Some(2));
    assert!(
        String::from_utf8(duplicate_output.stderr)
            .unwrap()
            .contains("duplicate suite")
    );

    let invocation_output = command(&["compare", "--current", "only-current"]);
    assert_eq!(invocation_output.status.code(), Some(2));
}

#[test]
fn missing_and_incompatible_baselines_return_error_status() {
    let root = TemporaryDirectory::new("baseline-errors");
    let current = root.path().join("current.json");
    let baseline = root.path().join("baseline.json");
    write_json(&current, &series("suite", "current", vec![1.0]));

    let missing_output = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
    ]);
    assert_eq!(missing_output.status.code(), Some(2));
    assert!(
        String::from_utf8(missing_output.stderr)
            .unwrap()
            .contains("failed to inspect")
    );

    let mut incompatible = series("suite", "baseline", vec![1.0]);
    incompatible.context =
        ReportContext::new("other-runner").with_environment("hardware", "other-node");
    write_json(&baseline, &incompatible);
    let incompatible_output = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
    ]);
    assert_eq!(incompatible_output.status.code(), Some(2));
    assert!(
        String::from_utf8(incompatible_output.stderr)
            .unwrap()
            .contains("runner mismatch")
    );
}

#[test]
fn strict_result_sets_reject_case_additions() {
    let root = TemporaryDirectory::new("strict");
    let current = root.path().join("current.json");
    let baseline = root.path().join("baseline.json");
    let mut current_report = series("suite", "current", vec![1.0]);
    current_report.results.push(SeriesResult::new(
        "scenario",
        "added",
        MeasurementKind::Latency,
        "ms",
        MeasurementDirection::Lower,
        vec![1.0],
    ));
    write_json(&current, &current_report);
    write_json(&baseline, &series("suite", "baseline", vec![1.0]));

    let partial_output = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
    ]);
    assert_eq!(partial_output.status.code(), Some(0));

    let strict_output = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--strict-result-set",
    ]);
    assert_eq!(strict_output.status.code(), Some(2));
    assert!(
        String::from_utf8(strict_output.stderr)
            .unwrap()
            .contains("result sets differ")
    );
}

#[test]
fn invalid_results_have_an_independent_gate() {
    let root = TemporaryDirectory::new("invalid-gate");
    let current = root.path().join("current.json");
    let baseline = root.path().join("baseline.json");
    let invalid = SeriesResult::new(
        "scenario",
        "latency",
        MeasurementKind::Latency,
        "ms",
        MeasurementDirection::Lower,
        Vec::new(),
    )
    .with_validity(Validity::invalid("checksum mismatch"));
    write_json(
        &current,
        &SeriesReport::new("current", "suite", context(), vec![invalid]),
    );
    write_json(&baseline, &series("suite", "baseline", vec![1.0]));

    let advisory = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--fail-on-regression",
    ]);
    assert_eq!(advisory.status.code(), Some(0));

    let gating = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--fail-on-invalid",
    ]);
    assert_eq!(gating.status.code(), Some(1));
    assert!(
        String::from_utf8(gating.stdout)
            .unwrap()
            .contains("1 invalid")
    );
}

#[test]
fn strict_suite_sets_reject_missing_suites() {
    let root = TemporaryDirectory::new("strict-suites");
    let current = root.path().join("current");
    let baseline = root.path().join("baseline");
    fs::create_dir(&current).unwrap();
    fs::create_dir(&baseline).unwrap();
    write_json(
        current.join("shared.json"),
        &series("shared", "current", vec![1.0]),
    );
    write_json(
        baseline.join("shared.json"),
        &series("shared", "baseline", vec![1.0]),
    );
    write_json(
        current.join("added.json"),
        &series("added", "current", vec![1.0]),
    );

    let output = command(&[
        "compare",
        "--current",
        current.to_str().unwrap(),
        "--baseline",
        baseline.to_str().unwrap(),
        "--strict-suite-set",
    ]);
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8(output.stderr)
            .unwrap()
            .contains("suite sets differ")
    );
}
