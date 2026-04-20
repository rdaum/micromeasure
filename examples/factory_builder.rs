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

use micromeasure::{BenchContext, Throughput, benchmark_main, black_box};

const INPUT_BYTES: usize = 4096;

struct ParseContext {
    input: Vec<u8>,
}

impl BenchContext for ParseContext {
    fn prepare(_num_chunks: usize) -> Self {
        Self {
            input: vec![b'x'; INPUT_BYTES],
        }
    }
}

fn parse_config(ctx: &mut ParseContext, chunk_size: usize, _chunk_num: usize) {
    let mut checksum = black_box(0_u64);
    for _ in 0..chunk_size {
        for &byte in &ctx.input {
            checksum = checksum.wrapping_add(black_box(byte as u64));
        }
    }
    black_box(checksum);
}

benchmark_main!(|runner| {
    let factory = || ParseContext {
        input: vec![b'x'; INPUT_BYTES],
    };

    runner.group::<ParseContext>("Parser", |g| {
        g.throughput(Throughput::per_operation(INPUT_BYTES as u64, "bytes"))
            .factory(&factory)
            .bench("parse_config", parse_config);
    });
});
