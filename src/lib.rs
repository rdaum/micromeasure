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
pub mod table;
mod session;
mod threading;

pub use bench::{
    BenchContext, BenchmarkRunner, NoContext,
};
pub use table::{Alignment, TableFormatter};

#[cfg(target_os = "linux")]
pub use bench::PerfCounters;
pub use session::{BenchmarkKind, BenchmarkReport, BenchmarkResult, ComparisonPolicy};

// Re-export key types for convenience
pub use std::time::Instant;
pub use std::hint::black_box;

#[cfg(target_os = "linux")]
pub use perf_event;
