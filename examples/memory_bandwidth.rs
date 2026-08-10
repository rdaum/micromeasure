// Copyright 2026 Ryan Daum
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
// http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

#[cfg(target_os = "linux")]
use micromeasure::{
    BenchContext, BenchmarkRuntimeOptions, LinuxPerfBackend, Throughput, benchmark_main, black_box,
};
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
const BUFFER_BYTES: usize = 64 * 1024 * 1024;

#[cfg(target_os = "linux")]
struct StreamingContext {
    words: Vec<u64>,
}

#[cfg(target_os = "linux")]
impl BenchContext for StreamingContext {
    fn prepare(_chunk_size: usize) -> Self {
        // Non-zero initialization faults in and backs the pages before the
        // measured sample begins.
        Self {
            words: vec![1; BUFFER_BYTES / size_of::<u64>()],
        }
    }
}

#[cfg(target_os = "linux")]
fn streaming_update(ctx: &mut StreamingContext, chunk_size: usize, _chunk_num: usize) {
    for _ in 0..chunk_size {
        for word in black_box(&mut ctx.words) {
            *word = word.wrapping_mul(3).wrapping_add(1);
        }
    }
    black_box(&ctx.words);
}

#[cfg(target_os = "linux")]
benchmark_main!(|runner| {
    runner.set_runtime(BenchmarkRuntimeOptions {
        warm_up_duration: Duration::from_millis(250),
        benchmark_duration: Duration::from_secs(1),
        ..BenchmarkRuntimeOptions::default()
    });
    runner.group::<StreamingContext>("System memory bandwidth", |g| {
        g.throughput(Throughput::per_operation(BUFFER_BYTES as u64, "bytes"))
            .backend(|| Box::new(LinuxPerfBackend::new().without_cpu_counters()))
            .bench("64 MiB streaming update", streaming_update);
    });
});

#[cfg(not(target_os = "linux"))]
fn main() {
    eprintln!("the memory bandwidth example requires Linux");
}
