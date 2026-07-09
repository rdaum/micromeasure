# Concepts

This chapter covers the handful of types and ideas that every `micromeasure` benchmark is built on. Read it once; later chapters assume it.

## The bench function signature

Every single-threaded benchmark is a plain function:

```rust,ignore
fn bench(ctx: &mut C, chunk_size: usize, chunk_num: usize)
```

- `ctx: &mut C` — a context your benchmark owns across one chunk. Use it to hold pre-allocated inputs, GPU buffers, parsed config, etc. `NoContext` is the zero-sized context for benches that need nothing.
- `chunk_size: usize` — how many operations to perform in this sample. The runner calibrates this so one chunk takes roughly 50 ms. Do **not** ignore it and run a fixed number of iterations: the calibration is what makes throughput numbers comparable across runs and machines.
- `chunk_num: usize` — zero-based sample index. Rarely needed in the hot loop; useful for metrics that vary per-sample (e.g. cuBLASLt algo id, see [custom_metrics](./examples/custom-metrics.md)).

The richer variant returns a `BenchSampleResult` so the bench can report custom metrics:

```rust,ignore
fn bench(ctx: &mut C, chunk_size: usize, chunk_num: usize) -> BenchSampleResult
```

Use the plain `bench(...)` when the framework-derived numbers (latency, throughput, PMU counters) tell the whole story. Use `bench_sample(...)` when the bench knows facts only available after execution — selected algorithm, CUDA event time, TFLOP/s. See [GPU Benchmarks](./gpu.md).

## `BenchContext` and `NoContext`

`BenchContext` is the trait your context type implements:

```rust,ignore
pub trait BenchContext {
    fn prepare(num_chunks: usize) -> Self;

    fn chunk_size() -> Option<usize> {
        None
    }

    fn operations_per_chunk() -> Option<u64> {
        None
    }
}
```

- `prepare(num_chunks)` constructs the context. The runner calls it once per sample (and during warm-up). `num_chunks` is a hint; most benches ignore it.
- `chunk_size()` — return `Some(n)` to **bypass calibration** and use a fixed chunk size. This is the standard pattern for GPU work, where launch overhead dominates small chunks and algorithm selection can change with shape. When you return `None`, the runner calibrates automatically (the CPU default). See [GPU Benchmarking Sharp Edges](./gpu-sharp-edges.md#calibration-assumes-cpu-like-chunk-scaling) for why this matters.
- `operations_per_chunk()` — when one chunk represents a number of logical operations different from `chunk_size` (e.g. each chunk iterates over `chunk_size` rows but does `chunk_size * 4096` byte reads), return it here so throughput is computed against the real operation count. Defaults to `chunk_size` when `None`.

`NoContext` is the pre-supplied zero-sized context. It implements both `BenchContext` and `ConcurrentBenchContext`, so it works for single-threaded and concurrent benches alike.

## Groups

All registration happens inside a named group:

```rust,ignore
runner.group::<NoContext>("Arithmetic", |g| {
    g.throughput(Throughput::per_operation(8, "bytes"))
        .bench("add_loop", add_bench);
});
```

The closure receives a `BenchmarkGroup<C>`, which is a **fluent** builder: each configurator method takes `&self` and returns `Self` **by value** (so chains work because each call produces a new configured value). `bench`/`bench_sample` are the terminal registrations and return `()`. Group-level config applies to every `bench` / `bench_sample` registered beneath it:

| Method | Purpose |
|---|---|
| `throughput(t)` | What one operation represents, for `X/s` output |
| `factory(\|\| C)` | Override the default `C::prepare(...)` constructor. Returns a different type (`BenchmarkGroupWithFactory`), not `Self` — it's a type-state transition. The factory is called per sample, same lifecycle as `prepare`. |
| `measurement_domain(d)` | `Cpu` / `Gpu` / `Mixed` — controls CPU-PMU diagnostics |
| `backend(\|\| Box<dyn MeasurementBackend>)` | Replace the platform default measurement path |
| `bench(name, f)` | Register a `fn(&mut C, usize, usize)` benchmark |
| `bench_sample(name, f)` | Register a bench that returns `BenchSampleResult` |
| `diagnostic_pass(f)` | Register a post-timing diagnostic pass (see [GPU](./gpu.md#diagnostic-replay-pass)) |
| `diagnostic_samples(n)` | How many times to run the diagnostic pass |

A concurrent group is a different entry point, `runner.concurrent_group::<C>(...)`, with its own fluent builder (notably `sample_duration(...)`). See [Concurrent Benchmarks](./concurrent.md).

## Throughput

`Throughput` says how many "things" one measured operation represents, so the report can render throughput with the right unit (`bytes/s`, `lines/s`, `rows/s`), not just generic `ops/s`.

```rust,ignore
Throughput::ops()                                  // 1 op/op -> "ops/s"
Throughput::bytes(8)                                // 8 bytes/op -> "bytes/s"
Throughput::per_operation(1_000, "lines")          // 1000 lines/op -> "lines/s"
```

Throughput is aggregated *across operations in a sample*, not across samples: `rate = (operations * amount_per_op) / duration`. The aggregation across samples is what produces median/p95/etc.

Asserts: amount per operation must be > 0, unit must be non-empty. The formatting helper auto-scales with SI prefixes (`k`, `M`, `G`) and prints `n/a` for non-finite or non-positive values.

The `Throughput` value flows into the report's throughput column. If you want bytes/s but the bench loop only does `chunk_size` iterations, set `Throughput::bytes(bytes_per_iteration)` — or, if one iteration covers multiple bytes, use `BenchContext::operations_per_chunk()` to report the true byte count per chunk.

## `black_box`

`std::hint::black_box` (re-exported as `micromeasure::black_box`) tells the optimizer "treat this value as if it could be anything", which forces the surrounding computation to be emitted. Without it, a tight bench loop can be folded to a constant or removed entirely, and you measure nothing.

Rule of thumb:

- `black_box` every input you read from a parameter or shared state inside the hot loop.
- `black_box` the final accumulator / return value at the end.
- Do **not** `black_box` inside the inner loop body more than necessary — it can itself affect instruction count and branch behaviour, which is what you are trying to measure.

The `basic` example shows the canonical pattern:

```rust,ignore
fn add_loop(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut acc = black_box(0_u64);
    let limit = black_box(chunk_size as u64);
    for i in 0..limit {
        acc = acc.wrapping_add(black_box(i));
    }
    black_box(acc);
}
```

## Runtime options

`BenchmarkRuntimeOptions` controls the measurement budget:

```rust,ignore
pub struct BenchmarkRuntimeOptions {
    pub warm_up_duration: Duration,   // default 1s
    pub benchmark_duration: Duration, // default 5s
    pub min_samples: usize,          // default 20
    pub max_samples: usize,          // default 100
}
```

Set it on the runner before registering groups:

```rust,ignore
runner.set_runtime(BenchmarkRuntimeOptions {
    warm_up_duration: Duration::from_millis(500),
    benchmark_duration: Duration::from_secs(2),
    ..BenchmarkRuntimeOptions::default()
});
```

`benchmark_duration` is the *target* total sample time; the actual number of samples is clamped to `[min_samples, max_samples]` based on the calibrated chunk duration. A 5s budget with 50ms chunks yields up to 100 samples; a 2s budget yields ~40.

All four fields must be > 0 and `min_samples <= max_samples`; the runner asserts this at startup.

## `benchmark_main!` and `run_benchmark_main`

The macro is the standard entry point:

```rust,ignore
benchmark_main!(|runner| { /* register groups */ });
```

It expands to `fn main()` that calls `run_benchmark_main(BenchmarkMainOptions::default(), |runner| ...)`. `run_benchmark_main` does, in order:

1. Parse an optional filter from `env::args()` (first non-`--` argument).
2. Construct a `BenchmarkRunner` with that filter.
3. Apply your `BenchmarkRuntimeOptions` from `BenchmarkMainOptions::runtime`.
4. Call your registration closure.
5. Build the report and call `report.print_summary_with(comparison_policy)` (default `LatestCompatible`).
6. Save the report to `./target/benchmark_results_<timestamp>.json`.

For custom suite name, custom filter help text, a different `ComparisonPolicy`, or disabling persistence, use `run_benchmark_main` directly:

```rust,ignore
run_benchmark_main(
    BenchmarkMainOptions {
        suite: Some("nightly".into()),
        comparison_policy: ComparisonPolicy::LatestCompatible,
        save_results: true,
        runtime: BenchmarkRuntimeOptions::default(),
        ..BenchmarkMainOptions::default()
    },
    |runner| { /* ... */ },
);
```

## The measurement backend (briefly)

Every sample is measured by a `MeasurementBackend`. The platform default is `LinuxPerfBackend` on Linux and `WallClockBackend` everywhere else. A group can override it with `g.backend(|| Box::new(MyBackend::new()))` — this is how CUDA event timing, custom counters, or other device APIs plug in.

Backends own the measurement window (`begin` / `end` / `collect` per sample) and decide what `results.duration` means (host wall-clock, or device event time for a GPU backend). They can also push custom metrics into the same per-sample `Vec<MetricValue>`. The full contract is in [GPU Benchmarks](./gpu.md) and [GPU Benchmarking Sharp Edges](./gpu-sharp-edges.md).

## Statistics you will see in the report

For every benchmark, the stats table reports:

- throughput (`X/s`, scaled with SI prefixes)
- latency per operation (`ns/op` or `ms/op`)
- median, p95, MAD (median absolute deviation), CV (coefficient of variation)
- outlier count across `samples`
- when PMU is available: cycles/op, instructions/op, IPC, branches/op, branch miss %, cache refs/op, cache misses/op, cache miss %, frontend/backend stall %

A `possible bottlenecks:` section may follow, derived from the PMU counters. Domain rules (Cpu/Gpu/Mixed) control whether these appear — see [GPU Benchmarks](./gpu.md#measurement-domain).

That's the whole conceptual surface. The remaining chapters specialize it.