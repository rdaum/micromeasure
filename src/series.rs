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

//! Portable raw-sample reports for externally orchestrated measurements.

use crate::{MeasurementDirection, MeasurementKind, ReportContext};
use serde::{Deserialize, Serialize};
use std::{collections::BTreeMap, error::Error, fmt};

/// JSON document discriminator required on external series reports.
pub const SERIES_DOCUMENT_TYPE: &str = "micromeasure-series";

/// JSON schema emitted and accepted for external series reports.
pub const SERIES_SCHEMA_VERSION: u32 = 1;

/// Whether evidence passed its independent correctness checks.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ValidityStatus {
    #[default]
    Valid,
    Invalid,
}

/// Correctness status for a report or individual result.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct Validity {
    pub status: ValidityStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl Validity {
    pub fn valid() -> Self {
        Self::default()
    }

    pub fn invalid(reason: impl Into<String>) -> Self {
        Self {
            status: ValidityStatus::Invalid,
            reason: Some(reason.into()),
        }
    }

    pub fn is_valid(&self) -> bool {
        self.status == ValidityStatus::Valid
    }

    fn validate(&self, location: &str) -> Result<(), SeriesValidationError> {
        if self.status == ValidityStatus::Invalid
            && self
                .reason
                .as_deref()
                .is_none_or(|reason| reason.trim().is_empty())
        {
            return Err(SeriesValidationError::new(format!(
                "{location} validity is invalid but has no non-empty reason"
            )));
        }
        Ok(())
    }
}

/// Raw observations for one externally orchestrated measurement.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SeriesResult {
    pub group: String,
    pub name: String,
    pub measurement: MeasurementKind,
    pub unit: String,
    pub direction: MeasurementDirection,
    pub samples: Vec<f64>,
    pub validity: Validity,
    #[serde(default)]
    pub dimensions: BTreeMap<String, String>,
    #[serde(default)]
    pub provenance: BTreeMap<String, String>,
}

impl SeriesResult {
    pub fn new(
        group: impl Into<String>,
        name: impl Into<String>,
        measurement: MeasurementKind,
        unit: impl Into<String>,
        direction: MeasurementDirection,
        samples: Vec<f64>,
    ) -> Self {
        Self {
            group: group.into(),
            name: name.into(),
            measurement,
            unit: unit.into(),
            direction,
            samples,
            validity: Validity::valid(),
            dimensions: BTreeMap::new(),
            provenance: BTreeMap::new(),
        }
    }

    pub fn with_validity(mut self, validity: Validity) -> Self {
        self.validity = validity;
        self
    }

    pub fn with_dimension(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.dimensions.insert(key.into(), value.into());
        self
    }

    pub fn with_provenance(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.provenance.insert(key.into(), value.into());
        self
    }

    fn validate(&self, index: usize) -> Result<(), SeriesValidationError> {
        let location = format!("result {index} ({}/{})", self.group, self.name);
        validate_nonempty("group", &self.group, &location)?;
        validate_nonempty("name", &self.name, &location)?;
        validate_nonempty("unit", &self.unit, &location)?;
        self.validity.validate(&location)?;
        validate_map("dimensions", &self.dimensions, &location)?;
        validate_map("provenance", &self.provenance, &location)?;

        if self.validity.is_valid() && self.samples.is_empty() {
            return Err(SeriesValidationError::new(format!(
                "{location} is valid but has no samples"
            )));
        }
        if self.samples.iter().any(|sample| !sample.is_finite()) {
            return Err(SeriesValidationError::new(format!(
                "{location} contains a non-finite sample"
            )));
        }
        Ok(())
    }
}

/// A portable external report containing chronological raw samples.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SeriesReport {
    pub document_type: String,
    pub schema_version: u32,
    pub timestamp: String,
    pub suite: String,
    pub validity: Validity,
    pub context: ReportContext,
    pub results: Vec<SeriesResult>,
}

impl SeriesReport {
    pub fn new(
        timestamp: impl Into<String>,
        suite: impl Into<String>,
        context: ReportContext,
        results: Vec<SeriesResult>,
    ) -> Self {
        Self {
            document_type: SERIES_DOCUMENT_TYPE.to_string(),
            schema_version: SERIES_SCHEMA_VERSION,
            timestamp: timestamp.into(),
            suite: suite.into(),
            validity: Validity::valid(),
            context,
            results,
        }
    }

    pub fn with_validity(mut self, validity: Validity) -> Self {
        self.validity = validity;
        self
    }

    /// Validate the document's semantic invariants.
    pub fn validate(&self) -> Result<(), SeriesValidationError> {
        if self.document_type != SERIES_DOCUMENT_TYPE {
            return Err(SeriesValidationError::new(format!(
                "document_type must be {SERIES_DOCUMENT_TYPE:?}, found {:?}",
                self.document_type
            )));
        }
        if self.schema_version != SERIES_SCHEMA_VERSION {
            return Err(SeriesValidationError::new(format!(
                "unsupported series schema version {}; supported version is {}",
                self.schema_version, SERIES_SCHEMA_VERSION
            )));
        }
        validate_nonempty("timestamp", &self.timestamp, "report")?;
        validate_nonempty("suite", &self.suite, "report")?;
        validate_nonempty("runner_id", &self.context.runner_id, "report context")?;
        self.context
            .validate()
            .map_err(|error| SeriesValidationError::new(error.to_string()))?;
        self.validity.validate("report")?;
        if self.validity.is_valid() && self.results.is_empty() {
            return Err(SeriesValidationError::new(
                "valid series report must contain at least one result",
            ));
        }
        for (index, result) in self.results.iter().enumerate() {
            result.validate(index)?;
        }
        Ok(())
    }
}

fn validate_nonempty(
    field: &str,
    value: &str,
    location: &str,
) -> Result<(), SeriesValidationError> {
    if value.trim().is_empty() {
        return Err(SeriesValidationError::new(format!(
            "{location} {field} must not be empty"
        )));
    }
    Ok(())
}

fn validate_map(
    name: &str,
    values: &BTreeMap<String, String>,
    location: &str,
) -> Result<(), SeriesValidationError> {
    for (key, value) in values {
        if key.trim().is_empty() {
            return Err(SeriesValidationError::new(format!(
                "{location} {name} keys must not be empty"
            )));
        }
        if value.trim().is_empty() {
            return Err(SeriesValidationError::new(format!(
                "{location} {name} value for {key:?} must not be empty"
            )));
        }
    }
    Ok(())
}

/// Semantic failure in an otherwise structurally readable series report.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SeriesValidationError {
    message: String,
}

impl SeriesValidationError {
    fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
        }
    }
}

impl fmt::Display for SeriesValidationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl Error for SeriesValidationError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn valid_result() -> SeriesResult {
        SeriesResult::new(
            "lifecycle",
            "create",
            MeasurementKind::Latency,
            "ms",
            MeasurementDirection::Lower,
            vec![10.0, 11.0, 12.0],
        )
    }

    fn valid_report() -> SeriesReport {
        SeriesReport::new(
            "2026-07-27T15:10:00Z",
            "container-lifecycle",
            ReportContext::new("runner-a"),
            vec![valid_result()],
        )
    }

    #[test]
    fn valid_series_passes_semantic_validation() {
        valid_report().validate().unwrap();
    }

    #[test]
    fn invalid_validity_requires_a_reason() {
        let mut report = valid_report();
        report.results[0].validity = Validity {
            status: ValidityStatus::Invalid,
            reason: None,
        };
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("non-empty reason")
        );
    }

    #[test]
    fn invalid_results_may_have_no_timing_samples() {
        let mut report = valid_report();
        report.results[0].samples.clear();
        report.results[0].validity = Validity::invalid("checksum mismatch");
        report.validate().unwrap();
    }

    #[test]
    fn valid_results_require_samples() {
        let mut report = valid_report();
        report.results[0].samples.clear();
        assert!(
            report
                .validate()
                .unwrap_err()
                .to_string()
                .contains("has no samples")
        );
    }

    #[test]
    fn invalid_reports_may_have_no_results() {
        let mut report = valid_report();
        report.results.clear();
        report.validity = Validity::invalid("shared setup failed");
        report.validate().unwrap();
    }
}
