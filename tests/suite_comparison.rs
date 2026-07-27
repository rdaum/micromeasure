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
    BenchmarkReport, ComparisonError, ComparisonOptions, MeasurementDirection, MeasurementKind,
    REPORT_SCHEMA_VERSION, RegressionPolicy, ReportContext, SeriesReport, SeriesResult,
    SuiteComparisonAnalysis, SuiteComparisonError, compare_report_inputs, validate_report_input,
};
use serde::Serialize;
use std::{
    fs,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
};

static TEMPORARY_DIRECTORY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TemporaryDirectory(PathBuf);

impl TemporaryDirectory {
    fn new(name: &str) -> Self {
        let sequence = TEMPORARY_DIRECTORY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "micromeasure-suite-{name}-{}-{sequence}",
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
        serde_json::from_str(include_str!("fixtures/native-report.json")).unwrap();
    report.timestamp = timestamp.to_string();
    report.suite = Some(suite.to_string());
    report
}

fn write_json(path: impl AsRef<Path>, value: &impl Serialize) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

#[test]
fn directory_comparison_combines_mixed_types_and_unmatched_suites() {
    let root = TemporaryDirectory::new("mixed");
    let current = root.path().join("current");
    let baseline = root.path().join("baseline");
    fs::create_dir_all(current.join("nested")).unwrap();
    fs::create_dir_all(&baseline).unwrap();

    write_json(
        current.join("nested/series.json"),
        &series("series-suite", "current", vec![80.0, 90.0, 100.0]),
    );
    write_json(
        baseline.join("series.json"),
        &series("series-suite", "baseline", vec![90.0, 100.0, 110.0]),
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
    write_json(
        baseline.join("removed.json"),
        &series("removed-suite", "baseline", vec![1.0]),
    );
    fs::write(current.join("README.txt"), "ignored").unwrap();

    let analysis = compare_report_inputs(
        &current,
        &baseline,
        &ComparisonOptions::default().allow_partial_result_set(true),
        &RegressionPolicy::gating(),
    )
    .unwrap();

    assert_eq!(analysis.summary.matched_suites, 2);
    assert_eq!(analysis.summary.added_suites, 1);
    assert_eq!(analysis.summary.removed_suites, 1);
    assert_eq!(analysis.summary.matched_cases, 2);
    assert_eq!(analysis.summary.improvements, 1);
    assert_eq!(analysis.summary.blocking, 0);
    assert_eq!(
        analysis.added_suites[0].suite.as_deref(),
        Some("added-suite")
    );
    assert_eq!(
        analysis.removed_suites[0].suite.as_deref(),
        Some("removed-suite")
    );
    assert!(analysis.render_markdown().contains("## Added suites"));
    assert!(analysis.render_terminal().contains("suites: 2 matched"));

    let json = analysis.render_json_pretty().unwrap();
    let restored: SuiteComparisonAnalysis = serde_json::from_str(&json).unwrap();
    assert_eq!(restored, analysis);
}

#[test]
fn duplicate_suites_are_rejected_in_lexical_path_order() {
    let root = TemporaryDirectory::new("duplicate-suite");
    write_json(
        root.path().join("z-report.json"),
        &series("duplicate", "z", vec![1.0]),
    );
    write_json(
        root.path().join("a-report.json"),
        &series("duplicate", "a", vec![1.0]),
    );

    let error = validate_report_input(root.path()).unwrap_err();
    match error {
        SuiteComparisonError::DuplicateSuite { first, second, .. } => {
            assert!(first.ends_with("a-report.json"));
            assert!(second.ends_with("z-report.json"));
        }
        other => panic!("unexpected error: {other}"),
    }
}

#[test]
fn duplicate_cases_are_rejected_even_without_a_matching_suite() {
    let root = TemporaryDirectory::new("duplicate-case");
    let current = root.path().join("current");
    let baseline = root.path().join("baseline");
    fs::create_dir(&current).unwrap();
    fs::create_dir(&baseline).unwrap();
    let duplicate = SeriesResult::new(
        "group",
        "case",
        MeasurementKind::Throughput,
        "items/s",
        MeasurementDirection::Higher,
        vec![1.0],
    );
    let report = SeriesReport::new(
        "current",
        "current-only",
        context(),
        vec![duplicate.clone(), duplicate],
    );
    write_json(current.join("duplicate.json"), &report);
    write_json(
        baseline.join("other.json"),
        &series("baseline-only", "baseline", vec![1.0]),
    );

    let error = compare_report_inputs(
        &current,
        &baseline,
        &ComparisonOptions::default(),
        &RegressionPolicy::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        SuiteComparisonError::InvalidDocument { source, .. }
            if matches!(*source, ComparisonError::DuplicateIdentity { .. })
    ));
}

#[test]
fn one_suite_cannot_change_document_type() {
    let root = TemporaryDirectory::new("type-mismatch");
    let current = root.path().join("current.json");
    let baseline = root.path().join("baseline.json");
    write_json(&current, &series("suite", "current", vec![1.0]));
    write_json(&baseline, &native("suite", "baseline"));

    assert!(matches!(
        compare_report_inputs(
            current,
            baseline,
            &ComparisonOptions::default(),
            &RegressionPolicy::default(),
        ),
        Err(SuiteComparisonError::DocumentTypeMismatch { .. })
    ));
}

#[test]
fn comparison_validation_rejects_empty_native_reports() {
    let root = TemporaryDirectory::new("empty-native");
    let report = BenchmarkReport {
        schema_version: REPORT_SCHEMA_VERSION,
        timestamp: "now".to_string(),
        hostname: "fixture-runner".to_string(),
        suite: Some("empty".to_string()),
        git_commit: None,
        context: context(),
        results: Vec::new(),
    };
    let path = root.path().join("empty.json");
    write_json(&path, &report);

    assert!(matches!(
        validate_report_input(path),
        Err(SuiteComparisonError::InvalidDocument { source, .. })
            if matches!(*source, ComparisonError::EmptyResultSet { .. })
    ));
}

#[test]
fn strict_suite_sets_reject_added_and_removed_suites() {
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
    write_json(
        baseline.join("removed.json"),
        &series("removed", "baseline", vec![1.0]),
    );

    let error = compare_report_inputs(
        current,
        baseline,
        &ComparisonOptions::default().require_same_suite_set(true),
        &RegressionPolicy::default(),
    )
    .unwrap_err();
    assert!(matches!(
        error,
        SuiteComparisonError::SuiteSetMismatch { added, removed }
            if added == vec!["added"] && removed == vec!["removed"]
    ));
}
