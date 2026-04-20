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

use micromeasure::{NoContext, Throughput, benchmark_main, black_box};

const LINES_PER_COMPILATION: usize = 1_000;

fn compile_lines(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut checksum = black_box(0_u64);
    for chunk in 0..chunk_size {
        for line in 0..LINES_PER_COMPILATION {
            checksum = checksum.wrapping_add(black_box((chunk ^ line) as u64));
        }
    }
    black_box(checksum);
}

benchmark_main!(|runner| {
    runner.group::<NoContext>("Compiler", |g| {
        g.throughput(Throughput::per_operation(
            LINES_PER_COMPILATION as u64,
            "lines",
        ))
        .bench("compile_lines", compile_lines);
    });
});
