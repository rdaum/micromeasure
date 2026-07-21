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

Automation can select an exact destination with `MICROMEASURE_OUTPUT`:

```sh
MICROMEASURE_OUTPUT=artifacts/benchmark.json cargo bench --bench basic
```

The runner creates missing parent directories and atomically replaces the destination, so an
artifact reader does not observe a partially written report. An explicit destination takes
precedence over `BenchmarkMainOptions::save_results`, including when that option is `false`.
Failure to write an explicitly requested artifact terminates the benchmark process; automation
should not continue under the assumption that its evidence was saved.

When `MICROMEASURE_OUTPUT` is unset, disable saving by setting
`BenchmarkMainOptions { save_results: false, ... }` and calling `run_benchmark_main` directly
instead of the `benchmark_main!` macro.

## What's in a report

`BenchmarkReport` (serde-serialized) contains:

- `schema_version` — JSON document format version; currently `1`
- `timestamp` — unix seconds string
- `hostname` — operating-system hostname used to gate comparison (different host = incompatible)
- `suite` — optional suite name; `benchmark_main!` defaults it to the stable Cargo benchmark target name, and comparison requires the same suite
- `git_commit` — short SHA captured at session start (best-effort)
- `results: Vec<BenchmarkResult>` — one per benchmark, each with `name`, `kind` (`Standard`/`Concurrent`), `execution_index`, metadata, stats, and (for concurrent) per-worker summaries

`BenchmarkStats` (the per-benchmark payload) carries the aggregated numbers: throughput median/p95, latency median/p95, MAD, CV, outlier count, sample count, all the PMU-derived per-op counts, the PMU coverage fields, the `measurement_label`, `measurement_domain`, `emits_cpu_diagnostics`, custom `metrics: Vec<MetricSummary>` (mean/median/p95/min/max/sample count per `(section, name, unit)`), and chronological `sample_metrics`.

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

- same known `hostname`; reports whose hostname cannot be resolved do not compare automatically
- same `suite` (`CARGO_CRATE_NAME`, normally the benchmark target name, by default)
- the same number of results, matched one-to-one by group, name, and kind
- matching throughput configuration, measurement domain, and per-result metadata

If the previous report has a different set of benchmarks (you added/removed/renamed one), it is not compatible and the runner skips the comparison rather than printing misleading deltas. Rename a benchmark and you lose comparability with the previous run — by design.

The runner loads the most recent compatible report from the target directory by scanning `benchmark_results_*.json`, parsing timestamps, and picking the latest one whose result set matches.

`benchmark_main!` supplies `env!("CARGO_CRATE_NAME")` as the default suite, so rebuilding a Cargo
benchmark does not change comparison identity when Cargo changes the executable's hash suffix.
An explicit `BenchmarkMainOptions::suite` still takes precedence. Code that constructs a
`BenchmarkRunner` directly falls back to the executable stem and strips a trailing 16-character
hexadecimal Cargo artifact hash when present.

On Unix, hostname comes from the operating-system hostname API, with `HOSTNAME` and `COMPUTERNAME`
used as fallbacks on platforms where necessary. If every lookup fails, the report retains
`"unknown"` for transparency but is not eligible for automatic comparison; treating reports from
unknown machines as the same host would make performance claims unsafe.

Reports created before `schema_version` was added are interpreted as schema 1. Reports carrying a
different schema version are skipped for comparison rather than being interpreted using incompatible
assumptions. External consumers can compare the document field with
`micromeasure::REPORT_SCHEMA_VERSION`.

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

Because `BenchmarkStats` derives serde, custom metrics from `bench_sample`,
concurrent lifecycle/backend collection, and `diagnostic_pass` are persisted as
summaries under `metrics`. Timing-sample metrics are also retained under
`sample_metrics` in execution order. Throughput and latency sample arrays are
chronological as well; percentile calculation never sorts the stored arrays.

## Workflow tips

- Run from the same checkout and the same machine for comparable reports. Hostname and suite mismatch intentionally block comparison.
- Changing `Throughput`, `MeasurementDomain`, or benchmark metadata makes the previous report incompatible, preventing a misleading comparison.
- The report captures `git_commit` best-effort. If you are benchmarking uncommitted changes, the SHA still points at HEAD, not your working tree.
- Reports accumulate in `target/`. They are not garbage-collected. Either add a `make clean-reports` target or clean the directory manually when it gets large.
