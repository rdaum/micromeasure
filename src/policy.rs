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

//! Policy evaluation kept separate from benchmark evidence and comparison.

use crate::{
    ComparisonCaseIdentity, ComparisonCaseSnapshot, ComparisonReport, ComparisonSide,
    MeasurementDirection, ValidityStatus,
};
use serde::{Deserialize, Serialize};
use std::{error::Error, fmt};

/// Default material-change threshold for advisory evaluation.
pub const DEFAULT_MINIMUM_CHANGE_PERCENT: f64 = 5.0;

/// Default coefficient-of-variation threshold for a stability finding.
pub const DEFAULT_MAXIMUM_CV_PERCENT: f64 = 10.0;

/// Default outlier-fraction threshold for a stability finding.
pub const DEFAULT_MAXIMUM_OUTLIER_FRACTION: f64 = 0.10;

/// Configurable classification and regression-gating policy.
///
/// The default is advisory: it classifies material changes and reports
/// stability findings, but never fails a gate.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct RegressionPolicy {
    pub minimum_change_percent: f64,
    pub maximum_cv_percent: Option<f64>,
    pub maximum_outlier_fraction: Option<f64>,
    #[serde(default)]
    pub fail_on_regression: bool,
    #[serde(default)]
    pub fail_on_invalid: bool,
}

impl Default for RegressionPolicy {
    fn default() -> Self {
        Self::advisory()
    }
}

impl RegressionPolicy {
    pub fn advisory() -> Self {
        Self {
            minimum_change_percent: DEFAULT_MINIMUM_CHANGE_PERCENT,
            maximum_cv_percent: Some(DEFAULT_MAXIMUM_CV_PERCENT),
            maximum_outlier_fraction: Some(DEFAULT_MAXIMUM_OUTLIER_FRACTION),
            fail_on_regression: false,
            fail_on_invalid: false,
        }
    }

    pub fn gating() -> Self {
        Self {
            fail_on_regression: true,
            ..Self::advisory()
        }
    }

    pub fn with_minimum_change_percent(mut self, minimum: f64) -> Self {
        self.minimum_change_percent = minimum;
        self
    }

    pub fn with_maximum_cv_percent(mut self, maximum: Option<f64>) -> Self {
        self.maximum_cv_percent = maximum;
        self
    }

    pub fn with_maximum_outlier_fraction(mut self, maximum: Option<f64>) -> Self {
        self.maximum_outlier_fraction = maximum;
        self
    }

    pub fn fail_on_regression(mut self, fail: bool) -> Self {
        self.fail_on_regression = fail;
        self
    }

    pub fn fail_on_invalid(mut self, fail: bool) -> Self {
        self.fail_on_invalid = fail;
        self
    }

    pub fn validate(&self) -> Result<(), PolicyError> {
        validate_nonnegative_finite("minimum_change_percent", self.minimum_change_percent)?;
        if let Some(maximum) = self.maximum_cv_percent {
            validate_nonnegative_finite("maximum_cv_percent", maximum)?;
        }
        if let Some(maximum) = self.maximum_outlier_fraction
            && (!maximum.is_finite() || !(0.0..=1.0).contains(&maximum))
        {
            return Err(PolicyError::InvalidConfiguration {
                reason: "maximum_outlier_fraction must be finite and between 0 and 1".to_string(),
            });
        }
        Ok(())
    }

    /// Evaluate a comparison without modifying its evidence.
    pub fn evaluate(&self, comparison: &ComparisonReport) -> Result<PolicyEvaluation, PolicyError> {
        self.validate()?;
        let cases: Vec<_> = comparison
            .matched
            .iter()
            .map(|matched| {
                let mut findings = stability_findings(
                    &matched.current,
                    ComparisonSide::Current,
                    self.maximum_cv_percent,
                    self.maximum_outlier_fraction,
                );
                findings.extend(stability_findings(
                    &matched.baseline,
                    ComparisonSide::Baseline,
                    self.maximum_cv_percent,
                    self.maximum_outlier_fraction,
                ));

                let classification = classify_case(
                    &matched.current,
                    &matched.baseline,
                    matched.percent_improvement,
                    self.minimum_change_percent,
                );
                let blocking = (self.fail_on_regression
                    && classification == ChangeClassification::Regression)
                    || (self.fail_on_invalid && classification == ChangeClassification::Invalid);

                PolicyCaseEvaluation {
                    identity: matched.identity.clone(),
                    classification,
                    percent_improvement: matched.percent_improvement,
                    findings,
                    blocking,
                }
            })
            .collect();

        let summary = PolicySummary {
            improvements: count_classification(&cases, ChangeClassification::Improvement),
            regressions: count_classification(&cases, ChangeClassification::Regression),
            no_material_change: count_classification(
                &cases,
                ChangeClassification::NoMaterialChange,
            ),
            informational: count_classification(&cases, ChangeClassification::Informational),
            invalid: count_classification(&cases, ChangeClassification::Invalid),
            inconclusive: count_classification(&cases, ChangeClassification::Inconclusive),
            unstable: cases
                .iter()
                .filter(|case| {
                    case.findings.iter().any(|finding| {
                        matches!(
                            finding,
                            PolicyFinding::HighCoefficientOfVariation { .. }
                                | PolicyFinding::ExcessiveOutlierFraction { .. }
                        )
                    })
                })
                .count(),
            blocking: cases.iter().filter(|case| case.blocking).count(),
        };

        Ok(PolicyEvaluation {
            policy: self.clone(),
            cases,
            gate_failed: summary.blocking > 0,
            summary,
        })
    }
}

fn validate_nonnegative_finite(field: &str, value: f64) -> Result<(), PolicyError> {
    if !value.is_finite() || value < 0.0 {
        return Err(PolicyError::InvalidConfiguration {
            reason: format!("{field} must be finite and non-negative"),
        });
    }
    Ok(())
}

fn classify_case(
    current: &ComparisonCaseSnapshot,
    baseline: &ComparisonCaseSnapshot,
    percent_improvement: Option<f64>,
    minimum_change_percent: f64,
) -> ChangeClassification {
    if current.validity.status == ValidityStatus::Invalid
        || baseline.validity.status == ValidityStatus::Invalid
    {
        return ChangeClassification::Invalid;
    }
    if current.primary.direction == MeasurementDirection::Informational {
        return ChangeClassification::Informational;
    }
    let Some(improvement) = percent_improvement else {
        return ChangeClassification::Inconclusive;
    };
    if improvement > minimum_change_percent {
        ChangeClassification::Improvement
    } else if improvement < -minimum_change_percent {
        ChangeClassification::Regression
    } else {
        ChangeClassification::NoMaterialChange
    }
}

fn stability_findings(
    snapshot: &ComparisonCaseSnapshot,
    side: ComparisonSide,
    maximum_cv_percent: Option<f64>,
    maximum_outlier_fraction: Option<f64>,
) -> Vec<PolicyFinding> {
    let mut findings = Vec::new();
    if snapshot.validity.status == ValidityStatus::Invalid {
        findings.push(PolicyFinding::InvalidResult {
            side,
            reason: snapshot
                .validity
                .reason
                .clone()
                .unwrap_or_else(|| "no reason supplied".to_string()),
        });
    }
    if let (Some(maximum), Some(actual)) = (maximum_cv_percent, snapshot.statistics.cv_percent)
        && actual > maximum
    {
        findings.push(PolicyFinding::HighCoefficientOfVariation {
            side,
            actual_percent: actual,
            maximum_percent: maximum,
        });
    }
    if let Some(maximum) = maximum_outlier_fraction
        && snapshot.statistics.samples > 0
    {
        let actual = snapshot.statistics.outliers as f64 / snapshot.statistics.samples as f64;
        if actual > maximum {
            findings.push(PolicyFinding::ExcessiveOutlierFraction {
                side,
                actual,
                maximum,
            });
        }
    }
    findings
}

fn count_classification(
    cases: &[PolicyCaseEvaluation],
    classification: ChangeClassification,
) -> usize {
    cases
        .iter()
        .filter(|case| case.classification == classification)
        .count()
}

/// Direction-aware material-change classification.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ChangeClassification {
    Improvement,
    Regression,
    NoMaterialChange,
    Informational,
    Invalid,
    Inconclusive,
}

impl fmt::Display for ChangeClassification {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Improvement => "improvement",
            Self::Regression => "regression",
            Self::NoMaterialChange => "no material change",
            Self::Informational => "informational",
            Self::Invalid => "invalid",
            Self::Inconclusive => "inconclusive",
        })
    }
}

/// Stability or correctness observation attached during policy evaluation.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum PolicyFinding {
    InvalidResult {
        side: ComparisonSide,
        reason: String,
    },
    HighCoefficientOfVariation {
        side: ComparisonSide,
        actual_percent: f64,
        maximum_percent: f64,
    },
    ExcessiveOutlierFraction {
        side: ComparisonSide,
        actual: f64,
        maximum: f64,
    },
}

/// Policy result for one matched case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PolicyCaseEvaluation {
    pub identity: ComparisonCaseIdentity,
    pub classification: ChangeClassification,
    pub percent_improvement: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub findings: Vec<PolicyFinding>,
    pub blocking: bool,
}

/// Aggregate policy counts.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PolicySummary {
    pub improvements: usize,
    pub regressions: usize,
    pub no_material_change: usize,
    pub informational: usize,
    pub invalid: usize,
    pub inconclusive: usize,
    pub unstable: usize,
    pub blocking: usize,
}

/// Complete policy evaluation for a comparison.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PolicyEvaluation {
    pub policy: RegressionPolicy,
    pub cases: Vec<PolicyCaseEvaluation>,
    pub summary: PolicySummary,
    pub gate_failed: bool,
}

impl PolicyEvaluation {
    pub fn exit_status(&self) -> ComparisonExitStatus {
        if self.gate_failed {
            ComparisonExitStatus::RegressionGateFailed
        } else {
            ComparisonExitStatus::Success
        }
    }
}

/// Stable process-status contract for comparison frontends.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[repr(i32)]
#[non_exhaustive]
pub enum ComparisonExitStatus {
    Success = 0,
    RegressionGateFailed = 1,
    Error = 2,
}

impl ComparisonExitStatus {
    pub const fn code(self) -> i32 {
        self as i32
    }
}

/// Invalid regression-policy configuration.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum PolicyError {
    InvalidConfiguration { reason: String },
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidConfiguration { reason } => {
                write!(formatter, "invalid regression policy: {reason}")
            }
        }
    }
}

impl Error for PolicyError {}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        ComparisonCaseSnapshot, ComparisonStatistics, NativeMeasurementProjection,
        PrimaryMeasurement, SeriesCaseIdentity, Validity,
    };
    use std::collections::BTreeMap;

    fn snapshot(
        direction: MeasurementDirection,
        value: Option<f64>,
        cv_percent: Option<f64>,
        outliers: usize,
        samples: usize,
    ) -> ComparisonCaseSnapshot {
        ComparisonCaseSnapshot {
            primary: PrimaryMeasurement {
                measurement: crate::MeasurementKind::Custom,
                unit: "unit".to_string(),
                direction,
                samples: Vec::new(),
                value,
            },
            statistics: ComparisonStatistics {
                cv_percent,
                mad: Some(1.0),
                p95: value,
                samples,
                outliers,
            },
            native: NativeMeasurementProjection {
                mean_throughput_per_sec: None,
                median_throughput_per_sec: None,
                median_ns_per_op: None,
                p95_ns_per_op: None,
                mad_ns_per_op: None,
            },
            validity: Validity::valid(),
            provenance: BTreeMap::new(),
        }
    }

    fn identity() -> ComparisonCaseIdentity {
        ComparisonCaseIdentity::Series(SeriesCaseIdentity {
            group: "group".to_string(),
            name: "case".to_string(),
            measurement: crate::MeasurementKind::Custom,
            unit: "unit".to_string(),
            direction: MeasurementDirection::Higher,
            dimensions: BTreeMap::new(),
        })
    }

    fn comparison(
        current: ComparisonCaseSnapshot,
        baseline: ComparisonCaseSnapshot,
        percent_improvement: Option<f64>,
    ) -> ComparisonReport {
        ComparisonReport {
            schema_version: crate::COMPARISON_SCHEMA_VERSION,
            current: reference(),
            baseline: reference(),
            suite: "suite".to_string(),
            environment: crate::EnvironmentComparison::default(),
            matched: vec![crate::MatchedBenchmark {
                identity: identity(),
                current,
                baseline,
                absolute_change: None,
                percent_improvement,
                stability_warnings: Vec::new(),
                metric_changes: Vec::new(),
            }],
            added: Vec::new(),
            removed: Vec::new(),
            summary: crate::ComparisonSummary {
                matched: 1,
                added: 0,
                removed: 0,
            },
        }
    }

    fn reference() -> crate::ReportReference {
        crate::ReportReference {
            document_type: crate::ReportDocumentType::Series,
            schema_version: 1,
            suite: Some("suite".to_string()),
            content_digest: "sha256:test".to_string(),
            capture_time: "now".to_string(),
            source_provenance: BTreeMap::new(),
            display_path: None,
        }
    }

    #[test]
    fn advisory_and_gating_modes_classify_the_same_regression() {
        let comparison = comparison(
            snapshot(MeasurementDirection::Higher, Some(90.0), Some(1.0), 0, 10),
            snapshot(MeasurementDirection::Higher, Some(100.0), Some(1.0), 0, 10),
            Some(-10.0),
        );

        let advisory = RegressionPolicy::advisory().evaluate(&comparison).unwrap();
        assert_eq!(
            advisory.cases[0].classification,
            ChangeClassification::Regression
        );
        assert!(!advisory.gate_failed);
        assert_eq!(advisory.exit_status(), ComparisonExitStatus::Success);

        let gating = RegressionPolicy::gating().evaluate(&comparison).unwrap();
        assert!(gating.gate_failed);
        assert!(gating.cases[0].blocking);
        assert_eq!(
            gating.exit_status(),
            ComparisonExitStatus::RegressionGateFailed
        );
    }

    #[test]
    fn threshold_is_material_but_not_statistical_significance() {
        let comparison = comparison(
            snapshot(MeasurementDirection::Lower, Some(95.0), Some(1.0), 0, 10),
            snapshot(MeasurementDirection::Lower, Some(100.0), Some(1.0), 0, 10),
            Some(5.0),
        );
        let evaluation = RegressionPolicy::default().evaluate(&comparison).unwrap();
        assert_eq!(
            evaluation.cases[0].classification,
            ChangeClassification::NoMaterialChange
        );
    }

    #[test]
    fn stability_thresholds_report_both_sides() {
        let comparison = comparison(
            snapshot(MeasurementDirection::Higher, Some(100.0), Some(12.0), 2, 10),
            snapshot(MeasurementDirection::Higher, Some(100.0), Some(11.0), 0, 10),
            Some(0.0),
        );
        let evaluation = RegressionPolicy::default().evaluate(&comparison).unwrap();
        assert_eq!(evaluation.summary.unstable, 1);
        assert_eq!(evaluation.cases[0].findings.len(), 3);
        assert!(!evaluation.gate_failed);
    }

    #[test]
    fn invalid_and_informational_results_are_not_regressions() {
        let mut invalid = snapshot(MeasurementDirection::Higher, None, None, 0, 0);
        invalid.validity = Validity::invalid("checksum mismatch");
        let evaluation = RegressionPolicy::gating()
            .evaluate(&comparison(
                invalid,
                snapshot(MeasurementDirection::Higher, Some(100.0), None, 0, 1),
                None,
            ))
            .unwrap();
        assert_eq!(
            evaluation.cases[0].classification,
            ChangeClassification::Invalid
        );
        assert!(!evaluation.gate_failed);

        let invalid_gate = RegressionPolicy::advisory()
            .fail_on_invalid(true)
            .evaluate(&comparison(
                {
                    let mut invalid = snapshot(MeasurementDirection::Higher, None, None, 0, 0);
                    invalid.validity = Validity::invalid("checksum mismatch");
                    invalid
                },
                snapshot(MeasurementDirection::Higher, Some(100.0), None, 0, 1),
                None,
            ))
            .unwrap();
        assert!(invalid_gate.gate_failed);
        assert!(invalid_gate.cases[0].blocking);

        let informational = RegressionPolicy::gating()
            .evaluate(&comparison(
                snapshot(MeasurementDirection::Informational, Some(50.0), None, 0, 1),
                snapshot(MeasurementDirection::Informational, Some(100.0), None, 0, 1),
                None,
            ))
            .unwrap();
        assert_eq!(
            informational.cases[0].classification,
            ChangeClassification::Informational
        );
        assert!(!informational.gate_failed);
    }

    #[test]
    fn invalid_policy_configuration_is_explicit() {
        assert!(
            RegressionPolicy::default()
                .with_minimum_change_percent(f64::NAN)
                .validate()
                .is_err()
        );
        assert!(
            RegressionPolicy::default()
                .with_maximum_outlier_fraction(Some(1.1))
                .validate()
                .is_err()
        );
    }

    #[test]
    fn exit_status_codes_are_stable() {
        assert_eq!(ComparisonExitStatus::Success.code(), 0);
        assert_eq!(ComparisonExitStatus::RegressionGateFailed.code(), 1);
        assert_eq!(ComparisonExitStatus::Error.code(), 2);
    }
}
