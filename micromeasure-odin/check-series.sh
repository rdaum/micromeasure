#!/usr/bin/env bash
# Checks Odin output against the Rust CLI and an independent reference fixture.
set -euo pipefail
package_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/micromeasure-series.XXXXXX")"
trap 'rm -rf "${work_dir}"' EXIT
odin_bin="${ODIN_BIN:-odin}"
cli="${MICROMEASURE_BIN:-micromeasure}"
"${cli}" --version
"${odin_bin}" build "${package_root}/examples/series-fixture" -out:"${work_dir}/fixture"
"${work_dir}/fixture" "${work_dir}/current.json"
"${cli}" validate "${work_dir}/current.json"
"${cli}" compare --baseline "${package_root}/fixtures/series-baseline.json" \
  --current "${work_dir}/current.json" --json-output "${work_dir}/comparison.json" \
  --markdown-output "${work_dir}/comparison.md" --minimum-change 5 \
  --fail-on-regression --fail-on-invalid
python3 - "${work_dir}/comparison.json" <<'PY'
import json
import math
import sys

with open(sys.argv[1]) as source:
    report = json.load(source)
summary = report["summary"]
assert summary["matched_cases"] == 3, summary
assert summary["added_cases"] == summary["removed_cases"] == 0, summary
assert summary["improvements"] == 2 and summary["informational"] == 1, summary
cases = report["comparisons"][0]["comparison"]["matched"]
by_metric = {case["identity"]["measurement"]: case for case in cases}
assert math.isclose(by_metric["latency"]["percent_improvement"], 20)
assert math.isclose(by_metric["throughput"]["percent_improvement"], 25)
assert by_metric["memory"]["current"]["primary"]["samples"] == [-50]
assert by_metric["memory"]["percent_improvement"] is None
print("Odin/Rust interchange: expected identities, samples, and deltas passed")
PY
# Missing diagnostic evidence must survive parsing and fail an invalid-result gate.
"${work_dir}/fixture" "${work_dir}/invalid.json" invalid-metrics
"${cli}" validate "${work_dir}/invalid.json"
status=0
"${cli}" compare --baseline "${work_dir}/invalid.json" \
  --current "${work_dir}/invalid.json" --json-output "${work_dir}/invalid-comparison.json" \
  --fail-on-invalid >"${work_dir}/invalid-comparison.txt" || status=$?
if [[ "${status}" != 1 ]]; then
  cat "${work_dir}/invalid-comparison.txt"
  echo "expected an invalid-result gate failure (exit 1), got ${status}" >&2
  exit 1
fi
python3 - "${work_dir}/invalid.json" "${work_dir}/invalid-comparison.json" <<'PY'
import json
import sys

with open(sys.argv[1]) as source:
    report = json.load(source)
invalid = [r for r in report["results"] if r["validity"]["status"] == "invalid"]
assert len(invalid) == 18
assert all(r["samples"] == [] and r["validity"]["reason"] for r in invalid)
with open(sys.argv[2]) as source:
    comparison = json.load(source)
assert comparison["summary"]["invalid"] == 18
print("Odin/Rust interchange: unavailable measurements remain invalid")
PY
# Exercise actual collection as well as deterministic fixture serialization.
"${odin_bin}" run "${package_root}/examples/basic" -o:speed \
  -out:"${work_dir}/example" -- "${work_dir}/live.json" "interchange-smoke"
"${cli}" validate "${work_dir}/live.json"
