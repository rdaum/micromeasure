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

use crate::{
    BenchmarkReport, BenchmarkRunner, BenchmarkRuntimeOptions, ComparisonOptions, ComparisonPolicy,
    ReportContext, ReportDocument, compare_reports,
};
use std::{env, ffi::OsString, path::PathBuf, time::Duration};

/// Environment variable selecting an explicit JSON report destination.
///
/// When set, this takes precedence over [`BenchmarkMainOptions::save_results`]
/// and the default target-directory destination. Failure to write the
/// requested artifact is fatal so automation cannot silently continue without
/// its expected evidence.
pub const OUTPUT_PATH_ENVIRONMENT: &str = "MICROMEASURE_OUTPUT";

/// Environment variable selecting an explicit report context document.
pub const CONTEXT_FILE_ENVIRONMENT: &str = "MICROMEASURE_CONTEXT_FILE";

/// Environment variable selecting an exact baseline report.
pub const BASELINE_PATH_ENVIRONMENT: &str = "MICROMEASURE_BASELINE";

#[derive(Debug, Eq, PartialEq)]
enum ReportDestination {
    Explicit(PathBuf),
    Default,
    Disabled,
}

fn report_destination(explicit_path: Option<OsString>, save_results: bool) -> ReportDestination {
    match explicit_path {
        Some(path) => ReportDestination::Explicit(PathBuf::from(path)),
        None if save_results => ReportDestination::Default,
        None => ReportDestination::Disabled,
    }
}

#[derive(Clone, Debug)]
pub struct BenchmarkMainOptions {
    pub suite: Option<String>,
    pub filter_help: Option<String>,
    pub comparison_policy: ComparisonPolicy,
    /// Programmatic context used when `MICROMEASURE_CONTEXT_FILE` is unset.
    pub report_context: Option<ReportContext>,
    /// Comparison behavior for an explicitly supplied baseline.
    pub explicit_comparison: ComparisonOptions,
    pub save_results: bool,
    pub runtime: BenchmarkRuntimeOptions,
}

impl Default for BenchmarkMainOptions {
    fn default() -> Self {
        Self {
            suite: None,
            filter_help: None,
            comparison_policy: ComparisonPolicy::LatestCompatible,
            report_context: None,
            explicit_comparison: ComparisonOptions::default().allow_partial_result_set(true),
            save_results: true,
            runtime: BenchmarkRuntimeOptions {
                warm_up_duration: Duration::from_secs(1),
                benchmark_duration: Duration::from_secs(5),
                min_samples: 20,
                max_samples: 100,
            },
        }
    }
}

#[doc(hidden)]
pub fn benchmark_options_with_default_suite(
    mut options: BenchmarkMainOptions,
    default_suite: &str,
) -> BenchmarkMainOptions {
    if options.suite.is_none() {
        options.suite = Some(default_suite.to_string());
    }
    options
}

pub fn benchmark_filter_from_args(args: &[String]) -> Option<String> {
    let separator_pos = args.iter().position(|arg| arg == "--");
    if let Some(separator_pos) = separator_pos {
        return args.get(separator_pos + 1).cloned();
    }

    args.iter()
        .skip(1)
        .find(|arg| !arg.starts_with("--") && !args[0].contains(arg.as_str()))
        .cloned()
}

pub fn benchmark_filter_from_env() -> Option<String> {
    let args: Vec<String> = env::args().collect();
    benchmark_filter_from_args(&args)
}

pub fn run_benchmark_main(
    options: BenchmarkMainOptions,
    register: impl FnOnce(&mut BenchmarkRunner),
) -> BenchmarkReport {
    let report_context = requested_context(
        env::var_os(CONTEXT_FILE_ENVIRONMENT),
        options.report_context.clone(),
    )
    .unwrap_or_else(|error| panic!("failed to load explicit benchmark context: {error}"));
    let explicit_baseline = requested_baseline(env::var_os(BASELINE_PATH_ENVIRONMENT))
        .unwrap_or_else(|error| panic!("failed to load explicit benchmark baseline: {error}"));
    let filter = benchmark_filter_from_env();

    if let Some(filter) = filter.as_deref() {
        eprintln!("Running benchmarks matching filter: '{filter}'");
        if let Some(help) = options.filter_help.as_deref() {
            eprintln!("Available filters: {help}");
        }
        eprintln!();
    }

    let mut runner = BenchmarkRunner::new().with_filter(filter.as_deref());
    if let Some(suite) = options.suite {
        runner = runner.with_suite(suite);
    }
    if let Some(context) = report_context {
        runner = runner
            .try_with_report_context(context)
            .unwrap_or_else(|error| panic!("invalid benchmark report context: {error}"));
    }
    runner = runner.with_runtime(options.runtime.clone());

    register(&mut runner);

    if filter.is_some() {
        eprintln!("\nBenchmark filtering complete.");
    }

    let report = runner.report();
    if let Some(baseline) = explicit_baseline {
        // The current evidence is persisted before comparison so a selected
        // but incompatible baseline cannot discard a valid measurement.
        persist_report(
            &report,
            env::var_os(OUTPUT_PATH_ENVIRONMENT),
            options.save_results,
        );
        let current = ReportDocument::from_native(report.clone())
            .unwrap_or_else(|error| panic!("failed to identify current benchmark report: {error}"));
        let comparison = compare_reports(&current, &baseline, &options.explicit_comparison)
            .unwrap_or_else(|error| {
                panic!(
                    "explicit benchmark baseline {} is incompatible: {error}",
                    baseline_display_path(&baseline)
                )
            });
        report.print_summary_against(baseline.as_native());
        println!(
            "\n📋 Structured comparison: {} matched, {} added, {} removed",
            comparison.summary.matched, comparison.summary.added, comparison.summary.removed
        );
    } else {
        report.print_summary_with(options.comparison_policy);
        persist_report(
            &report,
            env::var_os(OUTPUT_PATH_ENVIRONMENT),
            options.save_results,
        );
    }

    report
}

fn persist_report(report: &BenchmarkReport, explicit_path: Option<OsString>, save_results: bool) {
    match report_destination(explicit_path, save_results) {
        ReportDestination::Explicit(path) => {
            report.save_to_path(&path).unwrap_or_else(|error| {
                panic!(
                    "failed to save benchmark results to {}: {error}",
                    path.display()
                )
            });
            println!("\n💾 Results saved to: {}", path.display());
        }
        ReportDestination::Default => match report.save_to_default_location() {
            Ok(path) => println!("\n💾 Results saved to: {}", path.display()),
            Err(error) => println!("\n⚠️  Failed to save results: {error}"),
        },
        ReportDestination::Disabled => {}
    }
}

fn requested_context(
    explicit_path: Option<OsString>,
    configured: Option<ReportContext>,
) -> Result<Option<ReportContext>, crate::ContextError> {
    match explicit_path {
        Some(path) => ReportContext::load_from_path(PathBuf::from(path)).map(Some),
        None => {
            if let Some(context) = configured {
                context.validate()?;
                Ok(Some(context))
            } else {
                Ok(None)
            }
        }
    }
}

fn requested_baseline(
    explicit_path: Option<OsString>,
) -> Result<Option<ReportDocument>, crate::ReportError> {
    explicit_path
        .map(PathBuf::from)
        .map(ReportDocument::load_from_path)
        .transpose()
}

fn baseline_display_path(baseline: &ReportDocument) -> &str {
    baseline
        .reference()
        .display_path
        .as_deref()
        .unwrap_or("<in-memory>")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "micromeasure-launcher-{name}-{}-{}.json",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn explicit_output_takes_precedence_over_save_results() {
        let path = OsString::from("artifacts/report.json");
        assert_eq!(
            report_destination(Some(path), false),
            ReportDestination::Explicit(PathBuf::from("artifacts/report.json"))
        );
    }

    #[test]
    fn output_follows_save_results_without_an_explicit_path() {
        assert_eq!(report_destination(None, true), ReportDestination::Default);
        assert_eq!(report_destination(None, false), ReportDestination::Disabled);
    }

    #[test]
    fn call_site_suite_is_stable_and_preserves_explicit_override() {
        let options = benchmark_options_with_default_suite(BenchmarkMainOptions::default(), "gpu");
        assert_eq!(options.suite.as_deref(), Some("gpu"));

        let explicit = BenchmarkMainOptions {
            suite: Some("nightly".to_string()),
            ..BenchmarkMainOptions::default()
        };
        let options = benchmark_options_with_default_suite(explicit, "gpu");
        assert_eq!(options.suite.as_deref(), Some("nightly"));
    }

    #[test]
    fn explicit_context_file_takes_precedence_over_programmatic_context() {
        let path = temporary_path("context");
        fs::write(
            &path,
            r#"{"runner_id":"file-runner","environment":{"gpu":"GB300"},"provenance":{}}"#,
        )
        .unwrap();

        let context = requested_context(
            Some(path.clone().into_os_string()),
            Some(ReportContext::new("programmatic-runner")),
        )
        .unwrap()
        .unwrap();
        assert_eq!(context.runner_id, "file-runner");
        assert_eq!(context.environment["gpu"], "GB300");

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn invalid_programmatic_context_is_rejected_before_running() {
        let context = ReportContext::default().with_provenance("commit", " ");
        assert!(requested_context(None, Some(context)).is_err());
    }

    #[test]
    fn explicit_baseline_loads_the_exact_requested_report() {
        let path = temporary_path("baseline");
        let report = BenchmarkReport {
            schema_version: crate::REPORT_SCHEMA_VERSION,
            timestamp: "123".to_string(),
            hostname: "host-a".to_string(),
            suite: Some("suite-a".to_string()),
            git_commit: None,
            context: ReportContext::new("runner-a"),
            results: Vec::new(),
        };
        fs::write(&path, serde_json::to_vec_pretty(&report).unwrap()).unwrap();

        let baseline = requested_baseline(Some(path.clone().into_os_string()))
            .unwrap()
            .unwrap();
        assert_eq!(
            baseline.reference().display_path.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn missing_explicit_baseline_is_a_precise_error() {
        let path = temporary_path("missing-baseline");
        let error = requested_baseline(Some(path.clone().into_os_string())).unwrap_err();
        assert!(matches!(error, crate::ReportError::Io { .. }));
        assert!(error.to_string().contains(path.to_string_lossy().as_ref()));
    }
}
