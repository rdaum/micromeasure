# Examples

`micromeasure` ships ten runnable examples in [`examples/`](https://github.com/rdaum/micromeasure/tree/main/examples). Each demonstrates one feature in isolation so you can copy the shape that matches your workload.

Run any of them from the repo root. Add `--release` for realistic numbers.

| Example | Demonstrates |
|---|---|
| [basic](./examples/basic.md) | Minimal single-threaded bench, runtime options, throughput in bytes |
| [throughput_units](./examples/throughput-units.md) | `Throughput::per_operation(N, "lines")` -> `lines/s` |
| [factory_builder](./examples/factory-builder.md) | `BenchContext` with an inline `factory()` overriding the default constructor |
| [concurrent_scenario](./examples/concurrent-scenario.md) | Reader/writer contention, `concurrent_group`, `sample_duration` |
| [concurrent_counters](./examples/concurrent-counters.md) | Same, plus a `read_misses` event counter via `with_counter` |
| [gpu_domain](./examples/gpu-domain.md) | `MeasurementDomain::Gpu` suppresses CPU-PMU diagnostics |
| [mixed_domain](./examples/mixed-domain.md) | `MeasurementDomain::Mixed` emits diagnostics with `[host]` prefix |
| [custom_metrics](./examples/custom-metrics.md) | `bench_sample` returning `BenchSampleResult` with `MetricValue`s |
| [custom_backend](./examples/custom-backend.md) | Pluggable `MeasurementBackend` (simulated CUDA events, no CUDA link) |
| [cuda_event_backend](./examples/cuda-event-backend.md) | Real `CudaEventBackend` with `cudaMemsetAsync` (needs `cuda` feature) |

Each page below shows what the example demonstrates, the run command, a key code excerpt, and what to look for in the output.

> The examples all use `benchmark_main!`, which means the binary name is the example name and the filter (if any) comes after `--` or as the first positional arg: `cargo run --example basic --release -- add`.