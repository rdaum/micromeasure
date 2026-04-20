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
use std::{env, time::Duration};

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

    if options.save_results {
        match report.save_to_default_location() {
            Ok(path) => println!("\n💾 Results saved to: {}", path.display()),
            Err(error) => println!("\n⚠️  Failed to save results: {error}"),
        }
    }

    report
}
