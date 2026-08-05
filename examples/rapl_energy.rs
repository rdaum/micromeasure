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

#[cfg(target_os = "linux")]
use micromeasure::{
    BenchmarkRuntimeOptions, ConcurrentBenchControl, ConcurrentWorker, ConcurrentWorkerResult,
    LinuxPerfBackend, NoContext, benchmark_main, black_box,
};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
fn arithmetic(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut value = black_box(1_u64);
    for index in 0..chunk_size as u64 {
        value = value
            .wrapping_mul(black_box(6364136223846793005))
            .wrapping_add(black_box(index));
    }
    black_box(value);
}

#[cfg(target_os = "linux")]
fn parallel_arithmetic(
    _ctx: &NoContext,
    control: &ConcurrentBenchControl,
) -> ConcurrentWorkerResult {
    let mut operations = 0_u64;
    let mut value = black_box(control.thread_index() as u64 + 1);
    while !control.should_stop() {
        value = value
            .wrapping_mul(black_box(6364136223846793005))
            .wrapping_add(black_box(operations));
        operations = operations.wrapping_add(1);
    }
    black_box(value);
    ConcurrentWorkerResult::operations(operations)
}

#[cfg(target_os = "linux")]
benchmark_main!(|runner| {
    runner.set_runtime(BenchmarkRuntimeOptions {
        warm_up_duration: Duration::from_millis(250),
        benchmark_duration: Duration::from_secs(1),
        ..BenchmarkRuntimeOptions::default()
    });
    runner.group::<NoContext>("RAPL energy", |g| {
        g.backend(|| Box::new(LinuxPerfBackend::new().with_rapl_energy()))
            .bench("single-thread arithmetic", arithmetic);
    });
    runner.concurrent_group::<NoContext>("RAPL energy", |g| {
        let workers = [ConcurrentWorker {
            name: "arithmetic",
            threads: 2,
            run: parallel_arithmetic,
        }];
        g.backend(|| Box::new(LinuxPerfBackend::new().with_rapl_energy()))
            .sample_duration(Duration::from_millis(50))
            .bench("two-worker arithmetic", &workers);
    });
});

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the RAPL energy example requires Linux");
}
