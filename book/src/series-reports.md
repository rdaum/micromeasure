# External Sample Series

`SeriesReport` lets an external orchestrator feed raw measurements into the
same comparison model as native micromeasure benchmarks. The orchestrator may
remain shell, Python, Rust, or another tool; micromeasure owns schema
validation, summary statistics, semantic matching, and direction-aware
comparison.

This format is intentionally about evidence, not orchestration. It does not
provide Docker, FUSE, CUDA, cache-reset, readiness, or service-management
helpers.

## Producer requirements

An external producer should:

- measure elapsed time with a monotonic clock;
- keep setup and untimed preparation outside the measurement boundary;
- retain chronological individual samples;
- check correctness independently of timing;
- state report and result validity explicitly;
- identify dimensions that affect comparability;
- keep artifact/build provenance separate from those dimensions; and
- write the finished JSON atomically.

Wall-clock timestamps are useful for report provenance, but wall clocks should
not measure elapsed time because they can be adjusted while a run is active.

## Schema

Every document has `document_type: "micromeasure-series"` and
`schema_version: 1`:

```json
{
  "document_type": "micromeasure-series",
  "schema_version": 1,
  "timestamp": "2026-07-27T15:10:00Z",
  "suite": "container-lifecycle",
  "validity": {
    "status": "valid"
  },
  "context": {
    "runner_id": "gpu-node-05",
    "environment": {
      "hardware_class": "gb300",
      "gpu_driver": "595.71.05"
    },
    "provenance": {
      "commit": "83a20ec058e2fb00e7fa4558c4c6e81e2dcf253d"
    }
  },
  "results": [
    {
      "group": "pytorch-cold-start",
      "name": "create",
      "measurement": "latency",
      "unit": "ms",
      "direction": "lower",
      "samples": [411.2, 405.8, 409.1],
      "validity": {
        "status": "valid"
      },
      "dimensions": {
        "model": "pytorch-2.10-cuda-13.1",
        "cache_state": "warm-cas"
      },
      "provenance": {
        "image_digest": "sha256:..."
      }
    }
  ]
}
```

`measurement` accepts:

- `latency`
- `throughput`
- `memory`
- `occupancy`
- `custom`

`direction` accepts:

- `lower` — smaller values are better;
- `higher` — larger values are better; and
- `informational` — retain and summarize values without calculating an
  improvement.

Measurement kind is descriptive and participates in result identity. Unit and
direction determine comparison semantics. Samples may be any finite JSON
numbers; a valid result must contain at least one. Micromeasure derives the
median, p95, median absolute deviation, coefficient of variation, and Tukey
outlier count from those samples while preserving their original order.

## Identity and provenance

An external result's identity is:

```text
group + name + measurement + unit + direction + dimensions
```

Changing any identity field produces an added/removed pair. `dimensions`
therefore holds workload facts that must match: model, data set, precision,
matrix size, cache state, concurrency, or a semantic runtime configuration.

`provenance` identifies the particular artifact that was measured: image
digest, builder revision, package lock digest, or similar build identity.
Provenance never participates in matching. Comparing a newly built artifact
with its predecessor is the normal reason it differs.

The report-level `context.environment` map still has to match exactly, along
with `context.runner_id`, unless the caller supplies an explicit environment
override. Report-level and per-result provenance remain non-comparing.

## Validity

Both reports and results require:

```json
{"status": "valid"}
```

or:

```json
{"status": "invalid", "reason": "checksum mismatch"}
```

An invalid status requires a non-empty reason.

- An invalid report represents shared setup or orchestration failure. It can
  contain no results, remains loadable evidence, and causes comparison to
  return `ComparisonError::InvalidReport`.
- An invalid result remains matched and visible in `ComparisonReport`, but its
  primary value and percentage improvement are omitted. It may contain no
  samples when correctness failed before a timing observation was accepted.

Native `BenchmarkReport` documents remain implicitly valid because they are
only emitted after the benchmark invocation has produced a report.

## Loading and comparison

`ReportDocument::load_from_path` detects native and series documents:

```rust,ignore
use micromeasure::{ComparisonOptions, ReportDocument, compare_reports};

let current = ReportDocument::load_from_path("current-series.json")?;
let baseline = ReportDocument::load_from_path("baseline-series.json")?;
let comparison = compare_reports(
    &current,
    &baseline,
    &ComparisonOptions::default().allow_partial_result_set(true),
)?;
```

`SeriesReport::load_from_path` loads only the series schema, and
`SeriesReport::validate` lets an in-process producer check a document before
serialization. `SeriesReport::new` and `SeriesResult::new` provide Rust
construction helpers; their builder methods add dimensions, provenance, and
invalidity.

Loading distinguishes malformed JSON, malformed structure, unsupported
document/schema versions, and semantic invalidity. Duplicate identities are a
comparison error, just as they are for native reports.

The repository includes stable `tests/fixtures/series/python-current.json` and
`tests/fixtures/series/rust-baseline.json` fixtures. One uses compact output
typical of Python's `json.dump`; the other uses pretty output matching
`serde_json::to_writer_pretty`. Exact formatting does not affect parsing, but
each loaded artifact retains the SHA-256 digest of its exact bytes.
