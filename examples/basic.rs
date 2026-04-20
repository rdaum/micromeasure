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

use micromeasure::{BenchmarkRuntimeOptions, NoContext, Throughput, benchmark_main, black_box};
use std::time::Duration;

fn add_loop(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        acc = acc.wrapping_add(black_box(i));
    }
    black_box(acc);
}

benchmark_main!(|runner| {
    // Configure runtime options for the entire session
    let runtime = BenchmarkRuntimeOptions {
        warm_up_duration: Duration::from_millis(500),
        benchmark_duration: Duration::from_secs(2),
        ..BenchmarkRuntimeOptions::default()
    };

    runner
        .set_runtime(runtime)
        .group::<NoContext>("Arithmetic", |g| {
            g.throughput(Throughput::per_operation(8, "bytes"))
                .bench("add_loop", add_loop);
        });
});
