# Single-Threaded Benchmarks

This is the historic core of `micromeasure`: one function on one thread, measured with PMU counters when available.

## The minimal shape

```rust,ignore
use micromeasure::{NoContext, Throughput, benchmark_main, black_box};

fn add_loop(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        acc = acc.wrapping_add(black_box(i));
    }
    black_box(acc);
}

benchmark_main!(|runner| {
    runner.group::<NoContext>("Arithmetic", |g| {
        g.throughput(Throughput::per_operation(8, "bytes"))
            .bench("add_loop", add_loop);
    });
});
```

Recap from [Concepts](./concepts.md): the runner warms up, calibrates a chunk size that targets ~50 ms per sample, runs `min_samples..max_samples` samples, and aggregates.

## A context with setup

When your bench needs pre-allocated input, implement `BenchContext`:

```rust,ignore
struct ParseContext { input: Vec<u8> }

impl BenchContext for ParseContext {
    fn prepare(_num_chunks: usize) -> Self {
        Self { input: vec![b'x'; 4096] }
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
```

`prepare` is called once per sample (and during warm-up), so each sample starts from a fresh context. To override the default `T::prepare(...)` constructor without changing the `BenchContext` impl — for example to close over external setup state, or to construct via a different code path — supply a `factory`:

```rust,ignore
let factory = || ParseContext { input: vec![b'x'; 4096] };

runner.group::<ParseContext>("Parser", |g| {
    g.throughput(Throughput::per_operation(4096, "bytes"))
        .factory(&factory)
        .bench("parse_config", parse_config);
});
```

The `factory_builder` example demonstrates this end to end — see [factory_builder](./examples/factory-builder.md).

Note: the factory is called once per sample (and during warm-up), same lifecycle as `prepare` — it does not reuse one instance across samples. Its purpose is to let you supply the constructor inline, close over external setup state, or use a different construction path than the trait default. If per-sample construction is the cost you are trying to avoid, that is a real limitation of the current API, not something `factory` solves.

## Throughput with the right unit

If one measured operation represents N logical units, declare it so the report renders e.g. `lines/s`:

```rust,ignore
const LINES_PER_COMPILATION: usize = 1_000;

runner.group::<NoContext>("Compiler", |g| {
    g.throughput(Throughput::per_operation(
        LINES_PER_COMPILATION as u64,
        "lines",
    ))
    .bench("compile_lines", compile_lines);
});
```

See [throughput_units](./examples/throughput-units.md) for a runnable version.

## `operations_per_chunk` when chunk size != operation count

Calibration sets `chunk_size` to land at ~50 ms of work. If your inner loop does `chunk_size` iterations but each iteration covers multiple logical operations, implement `operations_per_chunk()` so throughput uses the real count:

```rust,ignore
impl BenchContext for MyCtx {
    fn prepare(_n: usize) -> Self { /* ... */ }

    fn operations_per_chunk() -> Option<u64> {
        // each chunk iterates chunk_size rows, each row is 4096 bytes
        Some(/* chunk_size * 4096 — but you can't see chunk_size here,
               so this is for fixed-shape work; for shape-dependent
               counts return None and rely on Throughput::per_operation */)
    }
}
```

In practice `Throughput::per_operation(amount, unit)` is the simpler lever: it multiplies `iterations` by `amount` before dividing by duration, so `rate = iterations * amount / duration`. Use `operations_per_chunk` only when the per-chunk operation count genuinely differs from `chunk_size` and you can express it statically.

## Reading the output

For each benchmark the runner prints:

```text
bench: add_loop
  results:
    ┌────────────┬─────────────┬───────────────┬ ... ┐
    │ Metric     │ Value       │ Per Op        │ ... │
    ├────────────┼─────────────┼───────────────┼ ... ┤
    │ samples    │ 100         │               │ ... │
    │ throughput │ 14.20 M/s   │               │ ... │  (bytes/s)
    │ latency    │             │ 70.4 ns/op    │ ... │
    │ ...        │             │               │ ... │
    │ instructions │           │ 2.00 /op      │ ... │
    │ branches   │             │ 1.00 /op      │ ... │
    │ cache misses │           │ 0.0000 /op    │ ... │
    └────────────┴─────────────┴───────────────┴ ... ┘
  host PMU (perf event group): coverage=100.0%
```

The PMU coverage line reports `time_running / time_enabled` from the perf event group — below 100% means the counters were multiplexed by the kernel and the PMU numbers were scaled. If coverage is low the runner emits a warning; if PMU is unavailable it falls back to timing-only and says so.

After the table, a `possible bottlenecks:` block may appear, derived from the PMU counters (data-side memory latency, branch predictor disruption, instruction-cache pressure, low IPC). These are heuristics, not proofs — see the note at the bottom of every report: *"I am not a professional statistician. It's possible my code is lying to you."*

## `bench` vs `bench_sample`

- `g.bench(name, f)` — `f: fn(&mut C, usize, usize)`. The framework derives operation count from `chunk_size` (or `operations_per_chunk()`). Use this for tight CPU loops.
- `g.bench_sample(name, f)` — `f: fn(&mut C, usize, usize) -> BenchSampleResult`. The bench explicitly returns its operation count *and* any custom metrics. Use this when the bench knows facts only available after execution (selected algorithm, device time, TFLOP/s). The runner routes `operations` into the normal throughput/latency path and aggregates `metrics` into a `custom metrics:` table. See [GPU Benchmarks](./gpu.md#per-sample-custom-metrics).

Both paths share the same calibration, warm-up, sample count, PMU, and persistence pipeline. `bench_sample` is strictly additive.

## What's next

- For lock/latch implementations and reader/writer contention, go to [Concurrent Benchmarks](./concurrent.md).
- For GPU work or custom device backends, go to [GPU Benchmarks](./gpu.md).
- For PMU setup and access troubleshooting, go to [Linux PMU Setup](./linux-pmu.md).