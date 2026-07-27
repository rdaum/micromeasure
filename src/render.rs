// Copyright 2026 Ryan Daum
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Side-effect-free rendering of comparison and policy results.

use crate::{
    ChangeClassification, ComparisonCaseIdentity, ComparisonExitStatus, ComparisonReport,
    PolicyCaseEvaluation, PolicyError, PolicyEvaluation, PolicyFinding, RegressionPolicy,
};
use serde::{Deserialize, Serialize};
use std::fmt::Write;

/// JSON schema emitted for a comparison plus its separate policy evaluation.
pub const ANALYSIS_SCHEMA_VERSION: u32 = 1;

/// A comparison bundled with a reproducible policy evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ComparisonAnalysis {
    pub schema_version: u32,
    pub comparison: ComparisonReport,
    pub policy: PolicyEvaluation,
}

impl ComparisonAnalysis {
    pub fn new(
        comparison: ComparisonReport,
        policy: &RegressionPolicy,
    ) -> Result<Self, PolicyError> {
        let evaluation = policy.evaluate(&comparison)?;
        Ok(Self {
            schema_version: ANALYSIS_SCHEMA_VERSION,
            comparison,
            policy: evaluation,
        })
    }

    pub fn exit_status(&self) -> ComparisonExitStatus {
        self.policy.exit_status()
    }

    pub fn render_terminal(&self) -> String {
        render_terminal(self)
    }

    pub fn render_markdown(&self) -> String {
        render_markdown(self)
    }

    pub fn render_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

impl ComparisonReport {
    /// Apply policy while retaining the policy-free comparison unchanged.
    pub fn analyze(&self, policy: &RegressionPolicy) -> Result<ComparisonAnalysis, PolicyError> {
        ComparisonAnalysis::new(self.clone(), policy)
    }
}

/// Render a concise plain-text comparison for an interactive job log.
pub fn render_terminal(analysis: &ComparisonAnalysis) -> String {
    let comparison = &analysis.comparison;
    let policy = &analysis.policy;
    let mut output = String::new();
    let _ = writeln!(
        output,
        "micromeasure comparison: {}",
        inline_text(&comparison.suite)
    );
    let _ = writeln!(
        output,
        "current:  {} {}",
        document_type_name(comparison.current.document_type),
        comparison.current.content_digest
    );
    let _ = writeln!(
        output,
        "baseline: {} {}",
        document_type_name(comparison.baseline.document_type),
        comparison.baseline.content_digest
    );
    let _ = writeln!(
        output,
        "environment: {}",
        inline_text(&environment_description(comparison))
    );
    let _ = writeln!(output, "policy: {}", policy_description(policy));
    let _ = writeln!(
        output,
        "results: {} matched, {} added, {} removed",
        comparison.summary.matched, comparison.summary.added, comparison.summary.removed
    );
    let _ = writeln!(
        output,
        "classification: {} improvements, {} regressions, {} unchanged, {} informational, {} invalid, {} inconclusive, {} unstable",
        policy.summary.improvements,
        policy.summary.regressions,
        policy.summary.no_material_change,
        policy.summary.informational,
        policy.summary.invalid,
        policy.summary.inconclusive,
        policy.summary.unstable,
    );
    let _ = writeln!(output, "gate: {}", gate_description(policy));

    if !comparison.matched.is_empty() {
        output.push_str("\ncases:\n");
        for matched in &comparison.matched {
            let evaluation = policy_case(policy, &matched.identity);
            let classification = evaluation
                .map(|case| case.classification)
                .unwrap_or(ChangeClassification::Inconclusive);
            let _ = writeln!(
                output,
                "  {:<18} {}/{} [{} {}]: {} -> {} ({})",
                classification_label(classification),
                inline_text(matched.identity.group()),
                inline_text(matched.identity.name()),
                measurement_name(matched.current.primary.measurement),
                inline_text(&matched.current.primary.unit),
                format_value(
                    matched.baseline.primary.value,
                    &matched.baseline.primary.unit
                ),
                format_value(matched.current.primary.value, &matched.current.primary.unit),
                format_percent(matched.percent_improvement),
            );
            if let Some(evaluation) = evaluation {
                for finding in &evaluation.findings {
                    let _ = writeln!(
                        output,
                        "    - {}",
                        inline_text(&finding_description(finding))
                    );
                }
            }
        }
    }

    render_unmatched_terminal(&mut output, "added", &comparison.added);
    render_unmatched_terminal(&mut output, "removed", &comparison.removed);
    output
}

/// Render Markdown suitable for a CI annotation or persisted summary.
pub fn render_markdown(analysis: &ComparisonAnalysis) -> String {
    let comparison = &analysis.comparison;
    let policy = &analysis.policy;
    let mut output = String::new();
    let _ = writeln!(
        output,
        "## Benchmark comparison: {}\n",
        escape_markdown(&comparison.suite)
    );
    let _ = writeln!(
        output,
        "- Current: `{}` `{}`",
        document_type_name(comparison.current.document_type),
        comparison.current.content_digest
    );
    let _ = writeln!(
        output,
        "- Baseline: `{}` `{}`",
        document_type_name(comparison.baseline.document_type),
        comparison.baseline.content_digest
    );
    let _ = writeln!(
        output,
        "- Environment: {}",
        escape_markdown(&environment_description(comparison))
    );
    let _ = writeln!(
        output,
        "- Policy: {}",
        escape_markdown(&policy_description(policy))
    );
    let _ = writeln!(
        output,
        "- Gate: **{}**\n",
        escape_markdown(&gate_description(policy))
    );

    output.push_str("| Outcome | Count |\n|---|---:|\n");
    let _ = writeln!(output, "| Matched | {} |", comparison.summary.matched);
    let _ = writeln!(output, "| Added | {} |", comparison.summary.added);
    let _ = writeln!(output, "| Removed | {} |", comparison.summary.removed);
    let _ = writeln!(output, "| Improvements | {} |", policy.summary.improvements);
    let _ = writeln!(output, "| Regressions | {} |", policy.summary.regressions);
    let _ = writeln!(
        output,
        "| No material change | {} |",
        policy.summary.no_material_change
    );
    let _ = writeln!(
        output,
        "| Informational | {} |",
        policy.summary.informational
    );
    let _ = writeln!(output, "| Invalid | {} |", policy.summary.invalid);
    let _ = writeln!(output, "| Inconclusive | {} |", policy.summary.inconclusive);
    let _ = writeln!(output, "| Unstable | {} |", policy.summary.unstable);

    if !comparison.matched.is_empty() {
        output.push_str(
            "\n| Case | Measurement | Baseline | Current | Improvement | Classification | Findings |\n",
        );
        output.push_str("|---|---|---:|---:|---:|---|---|\n");
        for matched in &comparison.matched {
            let evaluation = policy_case(policy, &matched.identity);
            let classification = evaluation
                .map(|case| case.classification)
                .unwrap_or(ChangeClassification::Inconclusive);
            let findings = evaluation
                .map(|case| {
                    case.findings
                        .iter()
                        .map(finding_description)
                        .collect::<Vec<_>>()
                        .join("; ")
                })
                .filter(|findings| !findings.is_empty())
                .unwrap_or_else(|| "—".to_string());
            let _ = writeln!(
                output,
                "| {}/{} | {} {} | {} | {} | {} | {} | {} |",
                escape_markdown(matched.identity.group()),
                escape_markdown(matched.identity.name()),
                measurement_name(matched.current.primary.measurement),
                escape_markdown(&matched.current.primary.unit),
                escape_markdown(&format_value(
                    matched.baseline.primary.value,
                    &matched.baseline.primary.unit
                )),
                escape_markdown(&format_value(
                    matched.current.primary.value,
                    &matched.current.primary.unit
                )),
                escape_markdown(&format_percent(matched.percent_improvement)),
                escape_markdown(&classification.to_string()),
                escape_markdown(&findings),
            );
        }
    }

    render_unmatched_markdown(&mut output, "Added cases", &comparison.added);
    render_unmatched_markdown(&mut output, "Removed cases", &comparison.removed);
    output
}

fn render_unmatched_terminal(
    output: &mut String,
    heading: &str,
    cases: &[crate::UnmatchedBenchmark],
) {
    if cases.is_empty() {
        return;
    }
    let _ = writeln!(output, "\n{heading}:");
    for case in cases {
        let _ = writeln!(
            output,
            "  {}/{} [{} {}]",
            inline_text(case.identity.group()),
            inline_text(case.identity.name()),
            measurement_name(case.measurement.primary.measurement),
            inline_text(&case.measurement.primary.unit),
        );
    }
}

fn render_unmatched_markdown(
    output: &mut String,
    heading: &str,
    cases: &[crate::UnmatchedBenchmark],
) {
    if cases.is_empty() {
        return;
    }
    let _ = writeln!(output, "\n### {heading}\n");
    for case in cases {
        let _ = writeln!(
            output,
            "- {}/{} — {} {}",
            escape_markdown(case.identity.group()),
            escape_markdown(case.identity.name()),
            measurement_name(case.measurement.primary.measurement),
            escape_markdown(&case.measurement.primary.unit),
        );
    }
}

fn policy_case<'a>(
    policy: &'a PolicyEvaluation,
    identity: &ComparisonCaseIdentity,
) -> Option<&'a PolicyCaseEvaluation> {
    policy.cases.iter().find(|case| &case.identity == identity)
}

fn policy_description(policy: &PolicyEvaluation) -> String {
    let mode = match (
        policy.policy.fail_on_regression,
        policy.policy.fail_on_invalid,
    ) {
        (true, true) => "regression and invalid-result gating",
        (true, false) => "regression gating",
        (false, true) => "invalid-result gating",
        (false, false) => "advisory",
    };
    let cv = policy
        .policy
        .maximum_cv_percent
        .map(|maximum| format!("{maximum:.2}%"))
        .unwrap_or_else(|| "disabled".to_string());
    let outliers = policy
        .policy
        .maximum_outlier_fraction
        .map(|maximum| format!("{:.2}%", maximum * 100.0))
        .unwrap_or_else(|| "disabled".to_string());
    format!(
        "{mode}; material change > {:.2}%; max CV {cv}; max outliers {outliers}",
        policy.policy.minimum_change_percent
    )
}

fn gate_description(policy: &PolicyEvaluation) -> String {
    if policy.gate_failed {
        format!("FAIL ({} blocking cases)", policy.summary.blocking)
    } else if policy.policy.fail_on_regression || policy.policy.fail_on_invalid {
        "PASS (gating enabled)".to_string()
    } else {
        "PASS (advisory only)".to_string()
    }
}

fn environment_description(comparison: &ComparisonReport) -> String {
    if comparison.environment.exact_match {
        format!(
            "exact match on runner {}",
            comparison.environment.current_runner_id
        )
    } else if let Some(environment_override) = comparison.environment.operator_override.as_ref() {
        format!(
            "operator override {:?}: {} vs {}",
            environment_override.reason,
            comparison.environment.current_runner_id,
            comparison.environment.baseline_runner_id,
        )
    } else {
        format!(
            "mismatch: {} vs {}",
            comparison.environment.current_runner_id, comparison.environment.baseline_runner_id,
        )
    }
}

fn finding_description(finding: &PolicyFinding) -> String {
    match finding {
        PolicyFinding::InvalidResult { side, reason } => {
            format!("{side} result invalid: {reason}")
        }
        PolicyFinding::HighCoefficientOfVariation {
            side,
            actual_percent,
            maximum_percent,
        } => format!("{side} CV {actual_percent:.2}% exceeds {maximum_percent:.2}%"),
        PolicyFinding::ExcessiveOutlierFraction {
            side,
            actual,
            maximum,
        } => format!(
            "{side} outliers {:.2}% exceed {:.2}%",
            actual * 100.0,
            maximum * 100.0
        ),
    }
}

fn classification_label(classification: ChangeClassification) -> &'static str {
    match classification {
        ChangeClassification::Improvement => "IMPROVEMENT",
        ChangeClassification::Regression => "REGRESSION",
        ChangeClassification::NoMaterialChange => "UNCHANGED",
        ChangeClassification::Informational => "INFORMATIONAL",
        ChangeClassification::Invalid => "INVALID",
        ChangeClassification::Inconclusive => "INCONCLUSIVE",
    }
}

fn measurement_name(measurement: crate::MeasurementKind) -> &'static str {
    match measurement {
        crate::MeasurementKind::Latency => "latency",
        crate::MeasurementKind::Throughput => "throughput",
        crate::MeasurementKind::Memory => "memory",
        crate::MeasurementKind::Occupancy => "occupancy",
        crate::MeasurementKind::Custom => "custom",
    }
}

fn document_type_name(document_type: crate::ReportDocumentType) -> &'static str {
    match document_type {
        crate::ReportDocumentType::NativeBenchmark => "native_benchmark",
        crate::ReportDocumentType::Series => "series",
    }
}

fn format_value(value: Option<f64>, unit: &str) -> String {
    value
        .map(|value| format!("{value:.4} {}", inline_text(unit)))
        .unwrap_or_else(|| "n/a".to_string())
}

fn format_percent(percent: Option<f64>) -> String {
    percent
        .map(|percent| format!("{percent:+.2}%"))
        .unwrap_or_else(|| "n/a".to_string())
}

fn escape_markdown(value: &str) -> String {
    value
        .replace('\\', "\\\\")
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('|', "\\|")
        .replace('`', "\\`")
        .replace('*', "\\*")
        .replace('_', "\\_")
        .replace('[', "\\[")
        .replace(']', "\\]")
        .replace(['\r', '\n'], " ")
}

fn inline_text(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn markdown_escaping_protects_tables() {
        assert_eq!(escape_markdown("a|b\nc"), "a\\|b c");
        assert_eq!(
            escape_markdown("<a>`b` *c* [d]"),
            "&lt;a&gt;\\`b\\` \\*c\\* \\[d\\]"
        );
    }

    #[test]
    fn terminal_values_stay_on_one_line() {
        assert_eq!(inline_text("a\nb\tc"), "a b c");
    }

    #[test]
    fn exit_status_is_forwarded_from_policy() {
        let policy = PolicyEvaluation {
            policy: RegressionPolicy::gating(),
            cases: Vec::new(),
            summary: crate::PolicySummary {
                blocking: 1,
                ..crate::PolicySummary::default()
            },
            gate_failed: true,
        };
        let analysis = ComparisonAnalysis {
            schema_version: ANALYSIS_SCHEMA_VERSION,
            comparison: empty_comparison(),
            policy,
        };
        assert_eq!(
            analysis.exit_status(),
            ComparisonExitStatus::RegressionGateFailed
        );
    }

    fn empty_comparison() -> ComparisonReport {
        let reference = crate::ReportReference {
            document_type: crate::ReportDocumentType::Series,
            schema_version: 1,
            suite: Some("suite".to_string()),
            content_digest: "sha256:test".to_string(),
            capture_time: "now".to_string(),
            source_provenance: Default::default(),
            display_path: None,
        };
        ComparisonReport {
            schema_version: crate::COMPARISON_SCHEMA_VERSION,
            current: reference.clone(),
            baseline: reference,
            suite: "suite".to_string(),
            environment: crate::EnvironmentComparison::default(),
            matched: Vec::new(),
            added: Vec::new(),
            removed: Vec::new(),
            summary: crate::ComparisonSummary {
                matched: 0,
                added: 0,
                removed: 0,
            },
        }
    }
}
