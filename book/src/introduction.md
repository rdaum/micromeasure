# Introduction

`micromeasure` is a microbenchmark harness for Rust for systems work where timing alone is not enough information.

It is aimed at very focused operations where you care about instruction count, branch predictor behaviour, cache misses, and operation latency. It also supports GPU benchmarks via pluggable measurement backends and per-sample custom metrics.

It grew out of the needs of the [`mooR`](https://codeberg.org/timbran/moor) project, where many of the interesting questions were about tiny operations and internal data-structure mechanics. The goal was to measure things like:

- what changed in the instruction count for something as small as `1 + 1` on a custom value type?
- did a small change in an internal data structure alter branch predictor behaviour?
- did cache misses move for a tight lookup or mutation path?
- did a micro-operation get noisier even if mean elapsed time barely moved?

That means:

- direct Linux perf counter (PMU) integration as a first-class feature
- simple hand-written microbench drivers, not macro-heavy harness structure
- output that emphasizes instruction count, branch behaviour, cache misses, and timing together
- benchmark binaries that can be filtered and run directly during systems work
- persisted raw samples so you can compare a current run against the last compatible run immediately

## What this crate is not

- not a replacement for Criterion in the general case
- not intended for large end-to-end benchmark suites
- not trying to hide measurement mechanics behind a lot of framework structure
- not a cross-platform PMU abstraction layer

## Where to go next

If you are new, read [Getting Started](./getting-started.md), then [Concepts](./concepts.md). From there, jump to the chapter that matches what you are benchmarking:

- tight CPU loops and value operations: [Single-Threaded Benchmarks](./single-threaded.md)
- lock/latch implementations, reader/writer contention: [Concurrent Benchmarks](./concurrent.md)
- cuBLASLt GEMM, CUDA streams, kernels with host/device latency splits: [GPU Benchmarks](./gpu.md)
- setting up `perf_event_paranoid`, fallback behavior, capabilities: [Linux PMU Setup](./linux-pmu.md)

If you already know the API and want a concrete example to copy, go straight to [Examples](./examples.md).

If you are evaluating `micromeasure` against Criterion, see [micromeasure vs Criterion](./vs-criterion.md).