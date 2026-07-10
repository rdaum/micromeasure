# Getting Started

This page walks through wiring `micromeasure` into a crate, writing the smallest possible benchmark, and running it. Every later chapter assumes you have done this once.

## 1. Add it as a dev-dependency

```toml
[dev-dependencies]
micromeasure = "0.9"
```

## 2. Declare a custom bench target with `harness = false`

`micromeasure` owns the `main` of its benchmark binaries, so you must disable Cargo's built-in benchmark harness:

```toml
[[bench]]
name = "basic"
harness = false
```

The bench file usually lives at `benches/basic.rs`. Because `harness = false`, you write `fn main()` yourself — but you almost never do that by hand. Use the [`benchmark_main!`](./api-reference.md#benchmark_main) macro (shown below), which handles argument parsing, report printing, and result persistence for you.

## 3. Write the benchmark

```rust,ignore
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

A few things to notice, all of which are covered in detail in [Concepts](./concepts.md):

- The bench function signature is `fn(&mut C, usize, usize)` — context, chunk size, chunk number. `NoContext` is the zero-cost context for benches that need no setup.
- `black_box` (re-exported from `std::hint::black_box`) prevents the optimizer from folding your work away. Use it on inputs and the final result.
- Everything is registered under a named `group`. A group holds shared configuration (throughput, context factory, backend, measurement domain) and owns one or more `bench` entries.
- `Throughput::per_operation(8, "bytes")` says one measured operation represents 8 bytes, so the report will render throughput as `bytes/s` rather than generic `ops/s`.

## 4. Run it

In this repo, the same code lives as an example, so you can try it without creating a project:

```sh
cargo run --example basic --release
```

In a consuming crate you would normally run:

```sh
cargo bench --bench basic
```

You will see a calibration spinner (`🔥 calibrating benchmark`), then a per-benchmark stats table, then a session summary, and finally a line like:

```text
💾 Results saved to: ./target/benchmark_results_<timestamp>.json
```

That JSON is the persisted report — see [Persisted Reports & Comparison](./reports.md) for what it contains and how the next run compares against it.

## 5. Filter a single benchmark

`benchmark_main!` parses a filter from the command line. Any non-`--` argument after the binary name is treated as a substring filter on benchmark names:

```sh
cargo bench --bench basic -- add
cargo run --example basic --release -- add_loop
```

When a filter is set, only benchmarks whose name contains the filter string run, and the runner prints `Running benchmarks matching filter: '<filter>'`.

## What just happened

1. The runner warmed up the benchmark for 1 second (default; configurable via `BenchmarkRuntimeOptions`).
2. It calibrated a chunk size that lands near 50 ms per sample, up to 15 passes.
3. It ran `min_samples..max_samples` samples (default 20..100), each a single chunk of the calibrated size.
4. On Linux it attached a perf-event group around each sample; on other platforms (or when PMU access is denied) it fell back to timing-only and told you.
5. It aggregated samples into `BenchmarkStats` (median, p95, MAD, CV, outliers, per-op PMU counts), printed the stats table, printed any bottleneck diagnostics, and saved the report.

You are now ready for [Concepts](./concepts.md).
