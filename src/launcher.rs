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

use crate::{BenchmarkReport, BenchmarkRunner, BenchmarkRuntimeOptions, ComparisonPolicy};
use std::{env, ffi::OsString, path::PathBuf, time::Duration};

/// Environment variable selecting an explicit JSON report destination.
///
/// When set, this takes precedence over [`BenchmarkMainOptions::save_results`]
/// and the default target-directory destination. Failure to write the
/// requested artifact is fatal so automation cannot silently continue without
/// its expected evidence.
pub const OUTPUT_PATH_ENVIRONMENT: &str = "MICROMEASURE_OUTPUT";

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
    pub save_results: bool,
    pub runtime: BenchmarkRuntimeOptions,
}

impl Default for BenchmarkMainOptions {
    fn default() -> Self {
        Self {
            suite: None,
            filter_help: None,
            comparison_policy: ComparisonPolicy::LatestCompatible,
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
    runner = runner.with_runtime(options.runtime.clone());

    register(&mut runner);

    if filter.is_some() {
        eprintln!("\nBenchmark filtering complete.");
    }

    let report = runner.report();
    report.print_summary_with(options.comparison_policy);

    match report_destination(env::var_os(OUTPUT_PATH_ENVIRONMENT), options.save_results) {
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

    report
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
