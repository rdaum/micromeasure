# Regression Policy & Rendering

Structured comparison produces evidence, not a pass/fail decision.
`RegressionPolicy` applies a separate, serializable decision layer to a
`ComparisonReport`, and `ComparisonAnalysis` keeps both together for rendering
or persistence:

```rust,ignore
use micromeasure::{
    ComparisonOptions, RegressionPolicy, ReportDocument, compare_reports,
};

let current = ReportDocument::load_from_path("artifacts/current.json")?;
let baseline = ReportDocument::load_from_path("artifacts/baseline.json")?;
let comparison = compare_reports(
    &current,
    &baseline,
    &ComparisonOptions::default().allow_partial_result_set(true),
)?;

let policy = RegressionPolicy::gating()
    .with_minimum_change_percent(5.0)
    .with_maximum_cv_percent(Some(10.0))
    .with_maximum_outlier_fraction(Some(0.10));
let analysis = comparison.analyze(&policy)?;

eprintln!("{}", analysis.render_terminal());
std::fs::write("comparison.md", analysis.render_markdown())?;
std::fs::write("comparison.json", analysis.render_json_pretty()?)?;
```

The terminal and Markdown renderers are side-effect-free: they return strings
and do not know about any CI service. The JSON renderer serializes the complete
versioned analysis, including exact report references, normalized evidence,
policy configuration, per-case classifications and findings, and the aggregate
gate decision.

## Classification

The policy evaluates the direction-aware percentage improvement already
computed for each matched case. Positive always means better:

- `improvement` — improvement is greater than the configured material-change
  threshold;
- `regression` — improvement is less than the negative threshold;
- `no_material_change` — the change is within or exactly on the threshold;
- `informational` — the measurement direction explicitly forbids a
  better/worse conclusion;
- `invalid` — either result failed its independent correctness check; and
- `inconclusive` — no finite percentage comparison is available, for example
  because the baseline value is zero.

The threshold describes materiality, not statistical significance.
`micromeasure` reports its deterministic median comparison and stability
evidence; it does not claim that a change is statistically significant.

## Advisory and gating modes

`RegressionPolicy::advisory()` is the default:

```rust,ignore
RegressionPolicy {
    minimum_change_percent: 5.0,
    maximum_cv_percent: Some(10.0),
    maximum_outlier_fraction: Some(0.10),
    fail_on_regression: false,
}
```

It classifies changes and reports stability findings without failing a
regression gate. `RegressionPolicy::gating()` uses the same thresholds and
enables `fail_on_regression`. A material regression is then marked `blocking`;
one or more blocking cases fail the gate.

The CV and outlier settings produce findings for the current and baseline
evidence independently. They do not turn a case into a regression and do not
block the gate. Set either option to `None` to disable that finding. Outlier
fractions are configured from `0.0` through `1.0`; `0.10` means ten percent.
Invalid and inconclusive cases remain visible but do not become regressions.

Policy construction is fluent, but evaluation validates the final public
fields as well. Thresholds must be finite and non-negative, and the outlier
fraction must be in range.

## Stable process status

`ComparisonExitStatus` defines the status contract for command-line frontends:

| Code | Variant | Meaning |
|---:|---|---|
| `0` | `Success` | comparison completed and no enabled regression gate failed |
| `1` | `RegressionGateFailed` | a configured regression gate found at least one blocking regression |
| `2` | `Error` | invocation, report loading, schema validation, compatibility, policy, rendering, or other operational failure |

`analysis.exit_status().code()` returns `0` or `1`. A frontend maps errors
encountered before an analysis exists to `ComparisonExitStatus::Error`.
The library does not terminate the process itself.

This contract intentionally distinguishes a valid negative performance result
from a failure to produce or interpret a result.

The separately packaged [`micromeasure` suite CLI](./suite-cli.md) implements
this contract for report files and directories.
