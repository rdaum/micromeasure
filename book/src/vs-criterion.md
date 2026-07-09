# micromeasure vs Criterion

[Criterion](https://docs.rs/criterion) is the dominant general-purpose Rust benchmarking library. It has the bulk of ecosystem mindshare, better polished statistical analysis, and a mature presentation story (HTML reports, plots, regression detection across runs). `micromeasure` is narrower. Neither subsumes the other.

## Use `micromeasure` when

- you are tuning very small operations and want PMU-derived metrics beside latency/throughput
- you want to inspect instruction count, branch misses, cache misses, and timing in **one** report from **one** run
- you are working on internal value operations, cache lookups, symbol tables, allocators, or similar hot paths
- you want a small custom benchmark binary that you control directly
- you want immediate "last run vs this run" output from persisted sample data, without a separate baseline workflow
- you are benchmarking GPU work and want device-event timing, host/device latency split, optional NVIDIA counter diagnostics, and domain-specific metrics in the same report

## Use Criterion when

- you want a polished general-purpose benchmark framework
- you want richer out-of-the-box statistical analysis and reporting
- you want HTML reports and the broader Criterion workflow
- PMU metrics are not the main reason you are benchmarking
- you need cross-platform PMU-ish data (Criterion has its own measurement plugins) and Linux-first is a dealbreaker

## The concrete PMU difference

Criterion's perf integrations are generally measurement **plugins**, which means a given run tends to be centred on one selected perf event. The use case here is different: `micromeasure` collects timing, throughput, and multiple PMU-derived metrics (cycles, instructions, branches, branch misses, cache refs, cache misses, L1I misses, frontend/backend stalls) together in **one** run, with `time_enabled`/`time_running` multiplexing correction, so you can see whether a tiny operation changed latency, instruction count, branch misses, and cache-miss behaviour at the same time.

That simultaneous view is the whole point for the kind of work `micromeasure` was built for: "did this internal data-structure change alter branch predictor behaviour" is a question you answer by reading the `branches/op` and `branch misses/op` columns next to `ns/op`, not by running the bench seven times with seven different perf events.

## The GPU difference

Criterion does not have first-class support for device-event timing, host/device latency split, measurement-domain tagging, diagnostic replay counters, or per-sample custom metrics. `micromeasure`'s `MeasurementBackend` trait, `MeasurementDomain`, `bench_sample`/`BenchSampleResult`, `CudaEventBackend`, and `GpuCounterCollector` were designed together for this. See [GPU Benchmarks](./gpu.md) and [GPU Benchmarking Sharp Edges](./gpu-sharp-edges.md).

If GPU benchmarking is not relevant to you, this difference doesn't matter.

## The workflow difference

- Criterion saves baselines explicitly (`--save-baseline`) and compares against named baselines. Powerful, but you have to opt in.
- `micromeasure` saves **every** run to `target/` and the next run automatically prints a regression analysis against the latest compatible previous report. No baseline management step. The trade-off: reports accumulate and are not garbage-collected, and "compatible" requires the same hostname, suite, and set of benchmark names. See [Persisted Reports & Comparison](./reports.md).

## The honesty disclaimer

From the README, and worth keeping: *"I am not a professional statistician. It's possible my code is lying to you. If so, please tell me."* Criterion's statistics are more mature and more thoroughly reviewed. If your conclusion depends on a subtle statistical claim (e.g. detecting a <1% regression with high confidence), prefer Criterion. `micromeasure`'s strength is the PMU-and-timing combined view for tiny operations, not statistical rigor.

## Origin bias

`micromeasure` started inside `mooR`, a multithreaded MOO server and transactional object database/runtime. Its benchmark harness grew out of performance work on tiny VM & DB operations, value manipulation, caches, symbol handling, string processing — internal systems paths where the interesting behaviour was often below the level of a conventional application benchmark. That origin explains the design bias: systems-level microbenchmarks, direct execution from custom bench binaries, PMU-aware analysis, and immediate regression visibility while iterating on low-level code. Criterion was built for a broader audience and it shows, in both directions.
