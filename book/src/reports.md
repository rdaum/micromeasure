# Persisted Reports & Comparison

`micromeasure` persists every benchmark run to JSON so you can compare a current run against the last compatible run immediately. This is one of the headline workflow features: no separate baseline-management step, no external tooling — the next run prints a regression analysis against the previous one automatically.

## Where reports are saved

By default, reports go to:

```text
<target-dir>/benchmark_results_<timestamp>.json
```

`<target-dir>` is resolved in order:

1. `CARGO_TARGET_DIR` environment variable
2. `cargo metadata --format-version 1` → `target_directory` field
3. fallback `./target`

The runner prints the saved path at the end of a run:

```text
💾 Results saved to: ./target/benchmark_results_1738000000.json
```

To disable saving, set `BenchmarkMainOptions { save_results: false, ... }` and call `run_benchmark_main` directly instead of the `benchmark_main!` macro.

## What's in a report

`BenchmarkReport` (serde-serialized) contains:

- `timestamp` — unix seconds string
- `hostname` — used to gate comparison (different host = incompatible)
- `suite` — optional suite name (defaults to the crate name); comparison requires same suite
- `git_commit` — short SHA captured at session start (best-effort)
- `results: Vec<BenchmarkResult>` — one per benchmark, each with `name`, `kind` (`Standard`/`Concurrent`), stats, and (for concurrent) per-worker summaries

`BenchmarkStats` (the per-benchmark payload) carries the aggregated numbers: throughput median/p95, latency median/p95, MAD, CV, outlier count, sample count, all the PMU-derived per-op counts, the PMU coverage fields, the `measurement_label`, `measurement_domain`, `emits_cpu_diagnostics`, and the custom `metrics: Vec<MetricSummary>` (mean/median/p95/min/max/sample count per `(section, name, unit)`).

## Comparison policy

`ComparisonPolicy` controls whether the session summary loads a previous report and prints a regression analysis:

```rust,ignore
pub enum ComparisonPolicy {
    None,              // no comparison
    LatestCompatible,  // default: compare against the most recent compatible report
}
```

`benchmark_main!` uses `LatestCompatible` by default. To change it, use `run_benchmark_main`:

```rust,ignore
run_benchmark_main(
    BenchmarkMainOptions {
        comparison_policy: ComparisonPolicy::None,
        ..BenchmarkMainOptions::default()
    },
    |runner| { /* ... */ },
);
```

## What "compatible" means

A previous report is **compatible** with the current run when all of:

- same `hostname`
- same `suite` (crate name by default)
- the **same set** of `(name, kind)` benchmark results

If the previous report has a different set of benchmarks (you added/removed/renamed one), it is not compatible and the runner skips the comparison rather than printing misleading deltas. Rename a benchmark and you lose comparability with the previous run — by design.

The runner loads the most recent compatible report from the target directory by scanning `benchmark_results_*.json`, parsing timestamps, and picking the latest one whose result set matches.

## The regression analysis

When a compatible previous report is found, the session summary prints:

```text
📊 REGRESSION ANALYSIS:
   ✅ Improvements: 3 benchmarks
   ❌ Regressions: 1 benchmark
   📈 Average change: +2.4%
```

A benchmark counts as an improvement if throughput increased by more than 1% vs the previous run, a regression if it decreased by more than 1%. The average is across benchmarks that had finite, comparable throughput values.

The comparison is on **throughput per second**, normalised by `Throughput` so different units (bytes/s, lines/s) are compared by their rate, not their raw magnitude. Benchmarks with non-finite throughput or a zero previous value are skipped.

## Inspecting raw reports

The JSON is pretty-printed and stable enough to diff. A common workflow:

```sh
ls -t target/benchmark_results_*.json | head -2
diff <(jq '.results[0].stats' target/benchmark_results_<prev>.json) \
     <(jq '.results[0].stats' target/benchmark_results_<curr>.json)
```

Because `BenchmarkStats` derives serde, custom metrics from `bench_sample` / `diagnostic_pass` are persisted alongside the standard fields under `metrics: Vec<MetricSummary>`.

## Workflow tips

- Run from the same checkout and the same machine for comparable reports. Hostname and suite mismatch intentionally block comparison.
- If you change `Throughput` units for a benchmark, previous comparisons for that benchmark become meaningless (different rate magnitude). The compatibility check doesn't catch this — it's on you to bump the suite name or accept a noisy diff.
- The report captures `git_commit` best-effort. If you are benchmarking uncommitted changes, the SHA still points at HEAD, not your working tree.
- Reports accumulate in `target/`. They are not garbage-collected. Either add a `make clean-reports` target or clean the directory manually when it gets large.