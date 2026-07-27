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
- `hostname` — operating-system hostname retained for compatibility and used as the default runner identity
- `suite` — optional suite name; `benchmark_main!` defaults it to the stable Cargo benchmark target name, and comparison requires the same suite
- `git_commit` — short SHA captured at session start (best-effort)
- `context` — resolved stable runner identity, exact comparison-environment map, and provenance map
- `results: Vec<BenchmarkResult>` — one per benchmark, each with `name`, `kind` (`Standard`/`Concurrent`), `execution_index`, metadata, stats, and (for concurrent) per-worker summaries

New reports capture full source commit, branch, dirty-worktree state, and Rust
toolchain in `context.provenance` when those values are available. The legacy
short `git_commit` field remains for compatibility. Provenance describes the
evidence but never controls whether two reports may be compared.

`BenchmarkStats` (the per-benchmark payload) carries the aggregated numbers: throughput median/p95, latency median/p95, MAD, CV, outlier count, sample count, all the PMU-derived per-op counts, the PMU coverage fields, the `measurement_label`, `measurement_domain`, `emits_cpu_diagnostics`, custom `metrics: Vec<MetricSummary>` (mean/median/p95/min/max/sample count per `(section, name, unit)`), and chronological `sample_metrics`.

## Supplying report context

Automation can provide one shared JSON context document to every benchmark
executable:

```sh
MICROMEASURE_CONTEXT_FILE=/work/benchmark-context.json \
MICROMEASURE_OUTPUT=/work/current.json \
cargo bench --bench basic
```

```json
{
  "runner_id": "gpu-node-05",
  "environment": {
    "hardware_class": "gb300",
    "gpu_driver": "595.71.05",
    "cuda_toolkit": "13.1"
  },
  "provenance": {
    "ci": "example-ci",
    "commit": "83a20ec058e2fb00e7fa4558c4c6e81e2dcf253d",
    "build_number": "418"
  }
}
```

`runner_id` defaults to the operating-system hostname when omitted or empty.
The complete `environment` map participates in compatibility; `provenance`
does not. Supplied provenance keys win over locally detected defaults, which
lets trusted automation provide the full CI commit and build identity.

An unreadable, malformed, or semantically invalid explicitly requested context
file is fatal. Empty environment/provenance keys or values are invalid. Do not
put credentials or secrets in the document: the resolved context is persisted
verbatim in the report. Rust callers can supply the same data through
`BenchmarkMainOptions::report_context` or
`BenchmarkRunner::with_report_context`.

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

- same known effective runner identity (`context.runner_id`, falling back to `hostname` for legacy reports)
- exactly equal `context.environment` maps
- same `suite` (`CARGO_CRATE_NAME`, normally the benchmark target name, by default)
- the same number of results, matched one-to-one by group, name, and kind
- matching throughput configuration, measurement domain, and per-result metadata

If the previous report has a different set of benchmarks (you added/removed/renamed one), it is not compatible and the runner skips the comparison rather than printing misleading deltas. Rename a benchmark and you lose comparability with the previous run — by design.

The runner loads the most recent compatible report from the target directory by scanning
`benchmark_results_*.json` in modification-time order and selecting the newest report whose result
set matches.

`benchmark_main!` supplies `env!("CARGO_CRATE_NAME")` as the default suite, so rebuilding a Cargo
benchmark does not change comparison identity when Cargo changes the executable's hash suffix.
An explicit `BenchmarkMainOptions::suite` still takes precedence. Code that constructs a
`BenchmarkRunner` directly falls back to the executable stem and strips a trailing 16-character
hexadecimal Cargo artifact hash when present.

On Unix, hostname comes from the operating-system hostname API, with `HOSTNAME` and `COMPUTERNAME`
used as fallbacks on platforms where necessary. If every lookup fails, the report retains
`"unknown"` for transparency but is not eligible for automatic comparison; treating reports from
unknown machines as the same host would make performance claims unsafe.

Reports created before `schema_version` was added are interpreted as schema 1.
Reports created before `context` was added deserialize with empty context and
continue comparing by hostname. Reports carrying a different schema version
are skipped for comparison rather than being interpreted using incompatible
assumptions. External consumers can compare the document field with
`micromeasure::REPORT_SCHEMA_VERSION`.

## Selecting an exact baseline

An automated run can select one exact baseline artifact:

```sh
MICROMEASURE_CONTEXT_FILE=/work/benchmark-context.json \
MICROMEASURE_BASELINE=/work/baseline/basic.json \
MICROMEASURE_OUTPUT=/work/current/basic.json \
cargo bench --bench basic
```

The launcher loads the context and baseline before running benchmarks. A
missing, unreadable, malformed, unsupported, or incompatible explicit baseline
is fatal and names the requested artifact; it never falls back to a local
`LatestCompatible` report. The current report is still persisted before the
compatibility check so a valid measurement is not lost when the selected
baseline is incompatible. Explicit launcher comparison allows partial result
sets by default, reporting matching, added, and removed cases.

Rust callers configure the same flow with
`BenchmarkMainOptions::report_context` and
`BenchmarkMainOptions::explicit_comparison`; the exact baseline path remains
selected with `MICROMEASURE_BASELINE`.

## Structured comparison

The local `LatestCompatible` workflow remains deliberately strict and
zero-configuration. Automation which has already selected an exact baseline can
instead load two evidence documents and request a reusable structured
comparison:

```rust,ignore
use micromeasure::{
    ComparisonOptions, ReportDocument, compare_reports,
};

let current = ReportDocument::load_from_path("artifacts/current.json")?;
let baseline = ReportDocument::load_from_path("artifacts/baseline.json")?;
let options = ComparisonOptions::default().allow_partial_result_set(true);
let comparison = compare_reports(&current, &baseline, &options)?;

serde_json::to_writer_pretty(std::io::stdout(), &comparison)?;
```

`ReportDocument::load_from_path` detects native benchmark and external series
documents and distinguishes I/O, malformed JSON, malformed structure,
unsupported document/schema versions, and semantic validation errors. It also
gives the document a `ReportReference` containing a SHA-256 digest of the exact
loaded bytes. For an in-memory comparison, `BenchmarkReport::compare` and
`SeriesReport::compare` derive references from their normal pretty JSON
representations.

Native benchmark cases are identified by group, name, kind, `Throughput`,
measurement domain, and benchmark metadata. Duplicate identities are errors in
the structured API. With partial matching enabled, semantically matching cases
are compared while current-only cases are reported as `added` and
baseline-only cases as `removed`; changed identity fields therefore become an
added/removed pair rather than a misleading numeric comparison. Strict
comparison is the default.

External series cases are identified by group, name, measurement kind, unit,
direction, and explicit comparison dimensions. Their artifact provenance does
not participate in identity. See [External Sample Series](./series-reports.md)
for the producer schema and validity rules.

`ComparisonReport` is policy-free, versioned JSON. Each matched case contains:

- chronological samples and a direction-aware median primary measurement;
- signed percentage improvement, where positive always means better;
- CV, p95, MAD, sample, and outlier evidence;
- result validity and per-case provenance; and
- native throughput/latency projections and matching custom metrics when
  present.

Comparison-ready native reports must contain at least one result. An empty
native report remains readable JSON but fails comparison validation with
`ComparisonError::EmptyResultSet`.

Suite, effective runner identity, and the complete comparison-environment maps
must match. `ComparisonOptions::with_environment_override(reason)` can permit
an intentional mismatch. The resulting `ComparisonReport.environment` records
the reason, both runner IDs, both original environment maps, and whether the
inputs were an exact match. An empty override reason is rejected. Percentage
improvement divides by the absolute baseline magnitude, so negative directional
measurements retain the correct sign. A zero or non-finite baseline value is
retained as evidence but produces no percentage improvement.

Apply a configurable advisory or gating decision and render the same evidence
as terminal text, Markdown, or versioned JSON with `RegressionPolicy` and
`ComparisonAnalysis`. See
[Regression Policy & Rendering](./policy-and-rendering.md) for classification,
stability findings, and the stable `0`/`1`/`2` frontend status contract.

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

- Run with the same suite, stable runner identity, and explicit environment map for comparable reports.
- Changing `Throughput`, `MeasurementDomain`, or benchmark metadata makes the previous report incompatible, preventing a misleading comparison.
- `context.provenance.commit` points at `HEAD`; use `dirty_worktree` to distinguish uncommitted changes.
- Reports accumulate in `target/`. They are not garbage-collected. Either add a `make clean-reports` target or clean the directory manually when it gets large.
