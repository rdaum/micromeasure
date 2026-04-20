use micromeasure::{
    ConcurrentBenchContext, ConcurrentBenchControl, ConcurrentWorker, ConcurrentWorkerResult,
    benchmark_main, black_box,
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

fn optimistic_reader(
    ctx: &CounterLatch,
    control: &ConcurrentBenchControl,
) -> ConcurrentWorkerResult {
    let mut operations = 0_u64;
    let mut read_misses = 0_u64;

    while !control.should_stop() {
        if let Ok(guard) = ctx.value.try_read() {
            black_box(*guard ^ control.thread_index() as u64 ^ control.role_thread_index() as u64);
            operations = operations.wrapping_add(1);
        } else {
            read_misses = read_misses.wrapping_add(1);
        }
    }

    ConcurrentWorkerResult::operations(operations).with_counter("read_misses", read_misses)
}

fn exclusive_writer(
    ctx: &CounterLatch,
    control: &ConcurrentBenchControl,
) -> ConcurrentWorkerResult {
    let mut operations = 0_u64;

    while !control.should_stop() {
        let mut guard = ctx.value.write().expect("rwlock poisoned");
        *guard = guard.wrapping_add(control.thread_index() as u64 + 1);
        black_box(*guard);
        operations = operations.wrapping_add(1);
    }

    ConcurrentWorkerResult::operations(operations)
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
        g.sample_duration(Duration::from_millis(50))
            .bench("rwlock_readers_vs_writer_with_counters", &workers);
    });
});
