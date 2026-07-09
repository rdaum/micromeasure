# API Reference

This page is a quick index of the public API surface, grouped by what you use it for. For full signatures and doc comments, see [docs.rs/micromeasure](https://docs.rs/micromeasure) — this page points you at the right type for each job.

## Entry point

### `benchmark_main!`

```rust,ignore
benchmark_main!(|runner| { /* register groups */ });
benchmark_main!(options, |runner| { /* ... */ });
```

Expands to `fn main()` calling `run_benchmark_main`. Parses an optional filter from `env::args()`, constructs a `BenchmarkRunner`, applies runtime options, runs your registration closure, prints the session summary, and saves the report.

`micromeasure::src/lib.rs`

### `run_benchmark_main` / `BenchmarkMainOptions`

For custom suite name, filter help, comparison policy, or disabling persistence:

```rust,ignore
pub struct BenchmarkMainOptions {
    pub suite: Option<String>,
    pub filter_help: Option<String>,
    pub comparison_policy: ComparisonPolicy,
    pub save_results: bool,
    pub runtime: BenchmarkRuntimeOptions,
}
```

`micromeasure::src/launcher.rs`

### `benchmark_filter_from_args` / `benchmark_filter_from_env`

Filter parsing helpers, exposed for custom entry points.

## Runner & groups

| Type | Purpose | Source |
|---|---|---|
| `BenchmarkRunner` | Owns groups, runtime options, filter. `group(...)`, `concurrent_group(...)`, `set_runtime(...)`, `with_filter(...)`, `with_suite(...)`. | `src/bench.rs` |
| `BenchmarkGroup<C>` | Fluent builder for single-threaded benches. `throughput`, `factory`, `measurement_domain`, `backend`, `bench`, `bench_sample`, `diagnostic_pass`, `diagnostic_samples`. | `src/bench.rs` |
| `ConcurrentBenchmarkGroup<C>` | Fluent builder for concurrent benches. `sample_duration`, `throughput`, `bench`. | `src/bench.rs` |
| `BenchmarkRuntimeOptions` | `warm_up_duration`, `benchmark_duration`, `min_samples`, `max_samples`. | `src/bench.rs` |

## Bench context

| Type | Purpose | Source |
|---|---|---|
| `BenchContext` | Trait: `prepare(num_chunks)`, `chunk_size() -> Option<usize>`, `operations_per_chunk() -> Option<u64>`. | `src/bench.rs` |
| `NoContext` | Zero-sized context; implements `BenchContext` and `ConcurrentBenchContext`. | `src/bench.rs` |
| `ConcurrentBenchContext` | Trait: `prepare(num_threads)`. | `src/bench.rs` |

## Concurrent benchmarks

| Type | Purpose | Source |
|---|---|---|
| `ConcurrentWorker<C>` | `{ name, threads, run }`. | `src/bench.rs` |
| `ConcurrentBenchControl` | `should_stop()`, `thread_index()`, `role_thread_index()`. | `src/bench.rs` |
| `ConcurrentWorkerResult` | `operations: u64` + named `counters`. `operations(n)`, `with_counter(name, value)`. | `src/bench.rs` |
| `CounterValue` | `{ name, value }`. | `src/bench.rs` |

## Throughput

```rust,ignore
Throughput::ops()                          // ops/s
Throughput::bytes(n)                       // bytes/s
Throughput::per_operation(amount, unit)    // arbitrary unit
```

`amount_per_operation()`, `unit()`, `rate_for_operations(...)`, `format_rate(...)`.

`src/bench.rs`

## Measurement backend

| Type | Purpose | Source |
|---|---|---|
| `MeasurementBackend` | Object-safe trait: `begin`, `end`, `collect`, `measurement_label`, `emits_cpu_diagnostics`. | `src/bench/backend.rs` |
| `MeasurementDomain` | `Cpu` / `Gpu` / `Mixed`. | `src/bench/backend.rs` |
| `WallClockBackend` | Timing-only fallback. | `src/bench/backend.rs` |
| `LinuxPerfBackend` | Linux default (perf-event group + fallback). Linux only. | `src/bench/perf.rs` |
| `PerfCounters` | Low-level perf counter handle. Linux only. | `src/bench/perf.rs` |
| `CudaEventBackend` | CUDA event timing on default stream. `cuda` feature. | `src/bench/cuda.rs` |
| `CudaEvent` / `CudaError` / `CudaResult` | CUDA runtime helpers. `cuda` feature. | `src/bench/cuda.rs` |

## Per-sample custom metrics

| Type | Purpose | Source |
|---|---|---|
| `BenchSampleResult` | `{ operations: u64, metrics: Vec<MetricValue> }`. `operations(n)`, `with_metric(...)`, `push_metric(...)`. `From<u64>`. | `src/bench/backend.rs` |
| `MetricValue` | `{ name, value, unit, section, display_name, format }`. Constructors: `new`, `duration_ms`, `bandwidth_gib_s`, `throughput_tflops`, `integer`. Builders: `with_display_name`, `with_section`, `with_format`. | `src/bench/backend.rs` |
| `MetricFormat` | `Number` (default, adaptive) / `Integer` (no decimals/scientific). | `src/bench/backend.rs` |

## Diagnostic replay pass

| Type | Purpose | Source |
|---|---|---|
| `DiagnosticResult` | `{ section, metrics }`. `new(section)`, `push_metric(metric)`. | `src/bench/backend.rs` |
| `DiagnosticError` | `{ message }`. `new(msg)`, `From<String>`, `From<&str>`. | `src/bench/backend.rs` |

Register with `g.diagnostic_pass(f)` and `g.diagnostic_samples(n)` on a `BenchmarkGroup`.

## Results & stats (persisted)

| Type | Purpose | Source |
|---|---|---|
| `Results` | Per-sample measurement: PMU counters, `has_*` flags, `pmu_time_enabled_ns`/`pmu_time_running_ns`, `duration`, `iterations`, `chunks_executed`. `add`, `divide`. | `src/bench.rs` |
| `BenchmarkStats` | Aggregated per-benchmark stats: throughput/latency median+p95, MAD, CV, outliers, PMU per-op counts, `metrics: Vec<MetricSummary>`, `measurement_label`, `measurement_domain`, `emits_cpu_diagnostics`. | `src/session.rs` |
| `MetricSummary` | Aggregated custom metric: mean, median, p95, min, max, sample count, `format`. | `src/session.rs` |

## Reports & comparison

| Type | Purpose | Source |
|---|---|---|
| `BenchmarkReport` | Persisted report: `timestamp`, `hostname`, `suite`, `git_commit`, `results`. `save_to_default_location()`, `print_summary_with(policy)`. | `src/session.rs` |
| `BenchmarkResult` | One benchmark's persisted entry: `name`, `kind`, stats, worker summaries. | `src/session.rs` |
| `BenchmarkKind` | `Standard` / `Concurrent`. | `src/session.rs` |
| `ComparisonPolicy` | `None` / `LatestCompatible`. | `src/session.rs` |
| `WorkerSummary` | Per-role summary for concurrent benchmarks: `name`, `threads`, `stats`, `counters`. | `src/session.rs` |
| `WorkerCounterSummary` | Aggregated event counter: `name`, `total`, `per_op`, `per_sec`. | `src/session.rs` |

## Table formatting

| Type | Purpose | Source |
|---|---|---|
| `TableFormatter` | Unicode table builder: `new(headers, widths)`, `with_alignments`, `with_group_split_after`, `with_border_color`, `add_row`, `print`. | `src/table.rs` |
| `Alignment` | `Left` / `Right`. | `src/table.rs` |
| `BorderColor` | Table border color variant. | `src/table.rs` |

## Re-exports

`micromeasure::black_box` (from `std::hint::black_box`), `micromeasure::Instant` (from `std::time::Instant`), and on Linux `micromeasure::perf_event` (the `perf-event2` crate re-exported for advanced PMU access).

## Full rustdoc

All of the above with full signatures, doc comments, and intra-doc links is at [docs.rs/micromeasure](https://docs.rs/micromeasure). This page is an index; the rustdoc is the authoritative reference.