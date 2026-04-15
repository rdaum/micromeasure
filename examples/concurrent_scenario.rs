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

use micromeasure::{
    ConcurrentBenchContext, ConcurrentBenchControl, ConcurrentWorker, benchmark_main, black_box,
};
use std::sync::RwLock;
use std::time::Duration;

#[derive(Default)]
struct CounterLatch {
    value: RwLock<u64>,
}

impl ConcurrentBenchContext for CounterLatch {
    fn prepare(_num_threads: usize) -> Self {
        Self::default()
    }
}

fn optimistic_reader(ctx: &CounterLatch, control: &ConcurrentBenchControl) -> u64 {
    let mut operations = 0_u64;
    while !control.should_stop() {
        if let Ok(guard) = ctx.value.try_read() {
            black_box(*guard ^ control.thread_index() as u64 ^ control.role_thread_index() as u64);
            operations = operations.wrapping_add(1);
        }
    }
    operations
}

fn exclusive_writer(ctx: &CounterLatch, control: &ConcurrentBenchControl) -> u64 {
    let mut operations = 0_u64;
    while !control.should_stop() {
        let mut guard = ctx.value.write().expect("rwlock poisoned");
        *guard = guard.wrapping_add(control.thread_index() as u64 + 1);
        black_box(*guard);
        operations = operations.wrapping_add(1);
    }
    operations
}

benchmark_main!(|runner| {
    let workers = [
        ConcurrentWorker {
            name: "optimistic_reader",
            threads: 3,
            run: optimistic_reader,
        },
        ConcurrentWorker {
            name: "exclusive_writer",
            threads: 1,
            run: exclusive_writer,
        },
    ];

    runner.concurrent_group::<CounterLatch>("Contention", |g| {
        g.bench(
            "rwlock_readers_vs_writer",
            Duration::from_millis(50),
            &workers,
        );
    });
});
