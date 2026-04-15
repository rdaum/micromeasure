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

//! Microbenchmark harness for tiny operations where PMU behaviour matters.
//!
//! This crate provides a reusable microbenchmark framework with:
//! - Performance counter integration (Linux only)
//! - Console output with Unicode tables
//! - Explicit report rendering and JSON persistence
//! - Warm-up and calibration phases
//! - Progress indicators
//! - Generic table formatting

pub mod bench;
mod launcher;
mod session;
pub mod table;
mod threading;

pub use bench::{
    BenchContext, BenchmarkRunner, ConcurrentBenchContext, ConcurrentBenchControl,
    ConcurrentBenchmarkGroup, ConcurrentWorker, NoContext,
};
pub use launcher::{BenchmarkMainOptions, benchmark_filter_from_args, benchmark_filter_from_env, run_benchmark_main};
pub use table::{Alignment, BorderColor, TableFormatter};

#[cfg(target_os = "linux")]
pub use bench::PerfCounters;
pub use session::{
    BenchmarkKind, BenchmarkReport, BenchmarkResult, BenchmarkStats, ComparisonPolicy,
    WorkerSummary,
};

// Re-export key types for convenience
pub use std::hint::black_box;
pub use std::time::Instant;

#[cfg(target_os = "linux")]
pub use perf_event;

#[macro_export]
macro_rules! benchmark_main {
    (|$runner:ident| $body:block) => {
        fn main() {
            let _ = $crate::run_benchmark_main($crate::BenchmarkMainOptions::default(), |$runner| $body);
        }
    };
    ($options:expr, |$runner:ident| $body:block) => {
        fn main() {
            let _ = $crate::run_benchmark_main($options, |$runner| $body);
        }
    };
}
