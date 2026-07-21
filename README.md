# micromeasure

`micromeasure` is a microbenchmark harness for Rust for systems work where timing alone is not enough information.

[![Crates.io](https://img.shields.io/crates/v/micromeasure.svg)](https://crates.io/crates/micromeasure)
[![Documentation](https://docs.rs/micromeasure/badge.svg)](https://docs.rs/micromeasure)
[![License](https://img.shields.io/badge/license-Apache%202.0-blue.svg)](./LICENSE)
[![Sponsor](https://img.shields.io/badge/Sponsor-%E2%9D%A4-pink)](https://github.com/sponsors/rdaum)

It is aimed at very focused operations where you care about instruction count, branch predictor behaviour, cache misses,
and operation latency. It also supports GPU benchmarks via pluggable measurement backends and per-sample custom metrics.

It grew out of the needs of my [`mooR`](https://codeberg.org/timbran/moor) project, where many of the interesting
questions were about tiny operations and internal data-structure mechanics. The goal was to measure things like:

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
- GPU benchmarking with pluggable measurement backends, per-sample custom metrics, measurement domain tagging, and optional NVIDIA GPU counter diagnostics

## Documentation

The full documentation lives in **[the mdbook](./book/src/SUMMARY.md)**. Build it locally with:

```sh
mdbook serve book --open
```

Quick orientation:

- [Introduction](./book/src/introduction.md) — what this crate is, what it isn't
- [Getting Started](./book/src/getting-started.md) — wire it up, write a bench, run it
- [Concepts](./book/src/concepts.md) — `BenchContext`, groups, throughput, runtime options, `black_box`
- [Single-Threaded Benchmarks](./book/src/single-threaded.md)
- [Concurrent Benchmarks](./book/src/concurrent.md)
- [GPU Benchmarks](./book/src/gpu.md) — measurement domain, pluggable backend, custom metrics, GPU counters, diagnostic replay
- [Linux PMU Setup](./book/src/linux-pmu.md) — `perf_event_paranoid`, capabilities, fallback
- [Persisted Reports & Comparison](./book/src/reports.md)
- [Examples](./book/src/examples.md) — one annotated walkthrough per runnable example
- [GPU Benchmarking Sharp Edges](./book/src/gpu-sharp-edges.md) — the failure modes that motivated the GPU design
- [micromeasure vs Criterion](./book/src/vs-criterion.md)

API reference is at [docs.rs/micromeasure](https://docs.rs/micromeasure).

## ... but why not Criterion?

Criterion is a strong general-purpose Rust benchmarking library. This crate is narrower.

Use this crate when:

- you are tuning very small operations and want PMU-derived metrics beside latency/throughput
- you want to inspect instruction count, branch misses, cache misses, and timing in one report
- you are working on internal value operations, cache lookups, symbol tables, allocators, or similar hot paths
- you want a small custom benchmark binary that you control directly
- you want immediate "last run vs this run" output from persisted sample data
- you are benchmarking GPU work and want device-event timing, host/device latency split, and optional NVIDIA counter diagnostics

Use Criterion when:

- you want a polished general-purpose benchmark framework
- you want richer out-of-the-box statistical analysis and reporting
- you want HTML reports and the broader Criterion workflow
- PMU metrics are not the main reason you are benchmarking

See [micromeasure vs Criterion](./book/src/vs-criterion.md) for the full comparison, including the concrete PMU and
GPU differences.

## Quick start

Add `micromeasure` as a dev-dependency:

```toml
[dev-dependencies]
micromeasure = "0.11"
```

Then add a custom bench target in your `Cargo.toml`:

```toml
[[bench]]
name = "basic"
harness = false
```

That bench target usually lives at `benches/basic.rs`. For the bench entrypoint, use the shared `benchmark_main!`
launcher instead of hand-rolling argument parsing, report printing, and result persistence.

```rust
use micromeasure::{NoContext, Throughput, benchmark_main, black_box};

fn add_bench(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
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
            .bench("add_loop", add_bench);
    });
});
```

In a consuming crate, run it with:

```sh
cargo bench --bench basic
```

In this repo, the same code lives as an example:

```sh
cargo run --example basic --release
```

Example output:

![micromeasure example output](screenshot.png)

For more examples (concurrent, throughput units, GPU domain, custom metrics, custom backend, CUDA event backend, GPU counters), see [Examples](./book/src/examples.md).

## Linux-first, and why

This crate is strongly Linux-specific, as its main differentiator is direct integration with Linux perf events and PMU
counters. The timing side of the crate is portable enough, but the most important measurements here are things like:

- instructions retired
- branch instructions
- branch misses
- cache misses

If those counters are not available, the crate still runs and still reports timing data, but you are only getting part
of what it is designed for. When PMU access is unavailable, the crate falls back to timing-only measurement and tells
you that it has done so.

See [Linux PMU Setup](./book/src/linux-pmu.md) for `perf_event_paranoid` settings, capabilities, and the fallback chain.

## What this crate is not

- not a replacement for Criterion in the general case
- not intended for large end-to-end benchmark suites
- not trying to hide measurement mechanics behind a lot of framework structure
- not a cross-platform PMU abstraction layer

## Origin

This crate started inside `mooR`, a multithreaded MOO server and transactional object database/runtime. Its benchmark
harness grew out of performance work on tiny VM & DB operations, value manipulation, caches, symbol handling, string
processing, and other internal systems paths where the interesting behaviour was often below the level of a conventional
application benchmark.

That origin explains the design bias:

- systems-level microbenchmarks
- direct execution from custom bench binaries
- PMU-aware analysis
- immediate regression visibility while iterating on low-level code

## License

`micromeasure` is licensed under the Apache License, Version 2.0. See [LICENSE](./LICENSE).

Unless explicitly stated otherwise, any contribution intentionally submitted for inclusion in this
project is contributed under the same license.

## Contributing

Contributions are welcome, especially around:

- better statistical analysis and comparison reporting
- improved presentation and terminal output
- additional platform backends for non-Linux systems
- the mdbook (typos, clarifications, missing examples)

If you find defects, I am very interested in hearing about them.

> If `micromeasure` is useful in your work, consider sponsoring development on GitHub Sponsors.
> I am also available for consulting in systems engineering, profiling and performance tuning, and
> Rust development (10 years at Google, 25+ years in software development). 
> If this project is useful or interesting for your team, feel free to reach out.

*I am not a professional statistician. It's possible my code is lying to you. If so, please tell me.*
