# Concurrent Benchmarks

`micromeasure` can benchmark coordinated concurrent workloads while still using the same sample-driven measurement pipeline as the single-threaded path. That means concurrent benchmarks still get the usual sample count and calibration flow, the usual timing statistics, Linux PMU counters when available, and persisted `BenchmarkResult` data.

The difference is the shape of one sample: instead of one function running on one thread, a sample runs multiple worker roles against shared state for a fixed sample window.

## When to use it

Use concurrent benchmarks when the thing you care about only shows up under contention:

- cache misses caused by reader/writer interference
- branch miss behaviour in optimistic retry loops
- lock or latch implementations under mixed access patterns
- asynchronous storage, network, or device services shared by worker roles

Do not use it for "run the same CPU bench on N threads and sum the throughput" — that's better expressed as N independent single-threaded benchmarks.

## The API surface

| Type | Role |
|---|---|
| `ConcurrentBenchContext` | Trait for the shared state. `prepare(num_threads)` constructs it once per sample. |
| `ConcurrentWorker<C>` | A named role: `name`, `threads` (how many threads run this role), `run` (the worker fn). |
| `ConcurrentBenchControl` | Per-thread control handle passed to the worker fn. `should_stop()`, `thread_index()`, `role_thread_index()`. |
| `ConcurrentWorkerResult` | What a worker returns: `operations: u64` plus optional named `counters`. |
| `ConcurrentBenchmarkGroup<C>` | Fluent group builder, registered via `runner.concurrent_group::<C>(...)`. |
| `ConcurrentSampleLifecycle<C>` | Optional setup/quiescence observer around every warm-up and measurement sample. |
| `ConcurrentSampleInfo` | Sample phase (`WarmUp` or `Measurement`) and zero-based phase-local index. |

## The worker function

A worker is a plain function:

```rust,ignore
fn optimistic_reader(
    ctx: &CounterLatch,
    control: &ConcurrentBenchControl,
) -> ConcurrentWorkerResult {
    let mut operations = 0_u64;
    while !control.should_stop() {
        // ... do one logical operation ...
        operations = operations.wrapping_add(1);
    }
    ConcurrentWorkerResult::operations(operations)
}
```

The contract:

- Loop until `control.should_stop()` returns true. The deadline is set by the runner based on `sample_duration`.
- Count your operations in a worker-local integer. Package it once at the end with `ConcurrentWorkerResult::operations(n)`.
- `thread_index()` is the worker's index across all threads in the sample; `role_thread_index()` is its index within its role. Use them to perturb behaviour (e.g. different stride per thread) or to avoid false sharing.
- **Do not** put allocation, locking of framework-owned locks, or I/O inside the hot loop unless that's what you're measuring.

## Reporting bench-specific events

If a concurrent benchmark needs to report scenario-specific events — retries, failed try-locks, dropped work, backoffs — return `ConcurrentWorkerResult` with named counters:

```rust,ignore
ConcurrentWorkerResult::operations(operations)
    .with_counter("read_misses", read_misses)
```

These event counters are intended to be:

- worker-local plain integers in the hot loop
- packaged once at the end of the sample
- aggregated by worker role after join

That keeps event reporting out of the measured hot path. The framework reports them under each worker role as `bench event counters`, including total count, per-operation rate, and per-second rate.

`Per Op` uses the total event count divided by total operations across all
samples. `Per Sec` uses total measured duration over the same samples.

## Shared state and scenario metrics

Worker counters are deliberately integer and role-local. Use a lifecycle
observer for shared service telemetry, floating-point values, distributions,
or unit-bearing metrics:

```rust,ignore
struct WalLifecycle;

impl ConcurrentSampleLifecycle<WalContext> for WalLifecycle {
    fn before_sample(&mut self, ctx: &mut WalContext, _sample: ConcurrentSampleInfo) {
        ctx.reset_wal();
    }

    fn after_sample(
        &mut self,
        ctx: &mut WalContext,
        sample: ConcurrentSampleInfo,
    ) -> Vec<MetricValue> {
        ctx.wait_until_quiescent();
        if sample.phase == ConcurrentSamplePhase::WarmUp {
            return Vec::new();
        }
        let telemetry = ctx.telemetry();
        vec![
            MetricValue::new("writes_per_barrier", telemetry.writes_per_barrier, "writes"),
            MetricValue::new("fsync_service_ms", telemetry.fsync_service_ms, "ms"),
        ]
    }
}

g.lifecycle(|| WalLifecycle)
    .measurement_domain(MeasurementDomain::Io)
    .metadata("filesystem", "ext4")
    .metadata("direct_io", "false")
    .sample_duration(Duration::from_millis(50))
    .bench("wal", &workers);
```

`before_sample` runs after context construction and before timing starts.
`after_sample` runs after every worker has joined and after timing stops, so it
can quiesce background services and collect the final sample without relying on
the next factory call. Warm-up callbacks run too, but their metrics are
discarded. Measured lifecycle metrics are automatically assigned the
`scenario` section, aggregated in `stats.metrics`, and retained individually in
chronological `stats.sample_metrics`.

## Whole-sample measurement backends

Concurrent groups accept the same opt-in `backend(|| Box<dyn
MeasurementBackend>)` configuration as standard groups. `begin` runs after all
workers are ready and immediately before their coordinated release; `end` runs
after all worker joins. `collect` then contributes scenario metrics and may
replace the combined duration while worker PMU summaries remain available as
host-orchestration context.

Lifecycle and backend responsibilities stay separate: the backend owns the
measurement window, while the lifecycle owns reset, quiescence, and telemetry
outside that window.

## Wiring it up

```rust,ignore
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

runner
    .set_runtime(runtime)
    .concurrent_group::<CounterLatch>("Contention", |g| {
        g.sample_duration(Duration::from_millis(50))
            .bench("rwlock_readers_vs_writer", &workers);
    });
```

A `concurrent_group` is configured fluently like a normal group, but with `sample_duration(...)` instead of letting calibration pick the chunk size. Each sample runs all worker roles for `sample_duration` and then joins.

## Reading the output

Concurrent output is organized as **per-role tables first**. Each worker role gets the same stats table shape as the normal benchmark path — throughput, latency, PMU-derived metrics (instructions/op, branch misses, cache misses), MAD, CV, outliers — computed across that role's threads and across all samples.

A `bench event counters` table appears under each role when workers return named counters.

The `workers combined` section at the bottom is a whole-scenario aggregate. It is mainly useful as the PMU view of the entire interacting workload; the per-role tables are usually the more meaningful place to interpret throughput and latency.

Raw throughput, latency, and custom metric samples are persisted in execution
order. Percentiles sort temporary copies rather than changing the report data.

## Calibration and sample count

Concurrent benchmarks do not run the CPU-style chunk-size calibration (there is no single chunk to size). Instead:

- Each sample runs for `sample_duration` (default 50 ms; set per group with `g.sample_duration(...)`).
- The runner runs `min_samples..max_samples` samples, same as the single-threaded path, clamped by `benchmark_duration`.
- Warm-up still happens, using the configured `warm_up_duration`.

Because the work window is fixed by `sample_duration`, throughput is `operations / sample_duration` per role, aggregated across samples.

## Thread pinning

On Linux, the runner pins worker threads to detected performance cores (via `detect_performance_cores`) so concurrent samples are not silently migrated across heterogeneous cores (P/E cores on Intel, etc.). Pinning can be disabled — see the affinity documentation in the source if you need to opt out for a specific scenario.

## I/O runs and case order

Use `MeasurementDomain::Io` for I/O-bound scenarios. CPU PMU values remain in
worker summaries but are labelled as host orchestration and do not produce CPU
bottleneck conclusions.

Attach storage state with repeated `metadata(key, value)` calls. Metadata and
execution order are persisted, and a previous report with different metadata,
throughput, or measurement domain is not considered comparison-compatible.

For caller-owned case arrays, `BenchmarkCaseOrder::Randomized { seed }` and
`runner.ordered_case_indices(...)` provide deterministic randomization. Record
the seed as metadata. `runner.set_case_cooldown(duration)` adds an unmeasured
pause between benchmark cases and works directly with the mutable runner
supplied by `benchmark_main!`; `with_case_cooldown` is available for owned
builders. The runner never clears caches or mutates device state itself.

## Worked examples

- [concurrent_scenario](./examples/concurrent-scenario.md) — reader/writer contention on a `RwLock<u64>` with `sample_duration` and a custom runtime.
- [concurrent_counters](./examples/concurrent-counters.md) — same shape, plus a `read_misses` event counter reported via `with_counter`.
