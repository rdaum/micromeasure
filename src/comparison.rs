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

//! Side-effect-free comparison of persisted benchmark evidence.

use crate::{
    BenchmarkKind, BenchmarkReport, BenchmarkResult, MeasurementDomain, MetricFormat,
    REPORT_SCHEMA_VERSION, SERIES_DOCUMENT_TYPE, SERIES_SCHEMA_VERSION, SeriesReport, SeriesResult,
    Throughput, Validity, ValidityStatus,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// JSON schema emitted for structured comparison reports.
pub const COMPARISON_SCHEMA_VERSION: u32 = 1;

/// Options controlling native report comparison.
///
/// The default remains conservative: reports must contain exactly the same
/// result set. CI callers which intentionally selected a baseline may opt into
/// partial matching with [`ComparisonOptions::allow_partial_result_set`].
#[derive(Clone, Debug, Default, Eq, PartialEq)]
#[non_exhaustive]
pub struct ComparisonOptions {
    pub allow_partial_result_set: bool,
    pub environment_override: Option<EnvironmentOverride>,
}

impl ComparisonOptions {
    pub fn allow_partial_result_set(mut self, allow: bool) -> Self {
        self.allow_partial_result_set = allow;
        self
    }

    /// Permit an explicitly justified runner or environment mismatch.
    ///
    /// The reason and both original environments are retained in the
    /// resulting [`ComparisonReport`].
    pub fn with_environment_override(mut self, reason: impl Into<String>) -> Self {
        self.environment_override = Some(EnvironmentOverride {
            reason: reason.into(),
        });
        self
    }
}

/// Operator-supplied justification for comparing non-identical environments.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EnvironmentOverride {
    pub reason: String,
}

/// Complete environment relationship recorded in a comparison artifact.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct EnvironmentComparison {
    pub current_runner_id: String,
    pub baseline_runner_id: String,
    pub current_environment: BTreeMap<String, String>,
    pub baseline_environment: BTreeMap<String, String>,
    pub exact_match: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operator_override: Option<EnvironmentOverride>,
}

/// The semantic kind of a comparison's primary measurement.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MeasurementKind {
    Latency,
    Throughput,
    Memory,
    Occupancy,
    Custom,
}

/// Whether larger or smaller primary values represent an improvement.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum MeasurementDirection {
    Lower,
    Higher,
    Informational,
}

/// Which side of a comparison supplied a value or caused an error.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComparisonSide {
    Current,
    Baseline,
}

impl fmt::Display for ComparisonSide {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Current => formatter.write_str("current"),
            Self::Baseline => formatter.write_str("baseline"),
        }
    }
}

/// Stable semantic identity for one native benchmark case.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct BenchmarkCaseIdentity {
    pub group: String,
    pub name: String,
    pub kind: BenchmarkKind,
    pub throughput: Throughput,
    pub measurement_domain: MeasurementDomain,
    pub metadata: BTreeMap<String, String>,
}

impl BenchmarkCaseIdentity {
    fn from_result(result: &BenchmarkResult) -> Self {
        Self {
            group: result.group.clone(),
            name: result.name.clone(),
            kind: result.kind,
            throughput: result.stats.throughput.clone(),
            measurement_domain: result.stats.measurement_domain,
            metadata: result.metadata.clone(),
        }
    }
}

/// Stable semantic identity for one external series case.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SeriesCaseIdentity {
    pub group: String,
    pub name: String,
    pub measurement: MeasurementKind,
    pub unit: String,
    pub direction: MeasurementDirection,
    pub dimensions: BTreeMap<String, String>,
}

impl SeriesCaseIdentity {
    fn from_result(result: &SeriesResult) -> Self {
        Self {
            group: result.group.clone(),
            name: result.name.clone(),
            measurement: result.measurement,
            unit: result.unit.clone(),
            direction: result.direction,
            dimensions: result.dimensions.clone(),
        }
    }
}

/// Semantic identity for a native benchmark or external series case.
///
/// The untagged representation preserves the original native comparison JSON
/// shape while allowing series identities to carry their declared
/// measurement semantics and dimensions.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
#[non_exhaustive]
pub enum ComparisonCaseIdentity {
    Native(BenchmarkCaseIdentity),
    Series(SeriesCaseIdentity),
}

impl ComparisonCaseIdentity {
    pub fn group(&self) -> &str {
        match self {
            Self::Native(identity) => &identity.group,
            Self::Series(identity) => &identity.group,
        }
    }

    pub fn name(&self) -> &str {
        match self {
            Self::Native(identity) => &identity.name,
            Self::Series(identity) => &identity.name,
        }
    }
}

/// A direction-aware primary value used for comparison.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct PrimaryMeasurement {
    pub measurement: MeasurementKind,
    pub unit: String,
    pub direction: MeasurementDirection,
    /// Chronological observations retained from the evidence document.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub samples: Vec<f64>,
    /// Median primary value, or `None` when the report did not contain a
    /// finite comparable value or the result is invalid.
    pub value: Option<f64>,
}

/// Stability evidence retained for one side of a matched or unmatched case.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ComparisonStatistics {
    pub cv_percent: Option<f64>,
    /// Median absolute deviation in the primary measurement's unit.
    pub mad: Option<f64>,
    /// 95th percentile in the primary measurement's unit.
    #[serde(default)]
    pub p95: Option<f64>,
    pub samples: usize,
    pub outliers: usize,
}

/// Native-only measurements kept alongside the normalized primary value.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct NativeMeasurementProjection {
    pub mean_throughput_per_sec: Option<f64>,
    pub median_throughput_per_sec: Option<f64>,
    pub median_ns_per_op: Option<f64>,
    pub p95_ns_per_op: Option<f64>,
    pub mad_ns_per_op: Option<f64>,
}

/// Measurement evidence for one side of a comparison.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ComparisonCaseSnapshot {
    pub primary: PrimaryMeasurement,
    pub statistics: ComparisonStatistics,
    pub native: NativeMeasurementProjection,
    #[serde(default)]
    pub validity: Validity,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub provenance: BTreeMap<String, String>,
}

/// A custom metric present in both versions of a matched benchmark.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MetricComparison {
    pub name: String,
    pub unit: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub section: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub display_name: String,
    pub format: MetricFormat,
    pub current_median: Option<f64>,
    pub baseline_median: Option<f64>,
    pub absolute_change: Option<f64>,
    /// Raw percentage change. Custom native metrics do not yet declare a
    /// preferred direction, so this is not labelled as an improvement.
    pub percent_change: Option<f64>,
}

/// Structured comparison for a benchmark found on both sides.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct MatchedBenchmark {
    pub identity: ComparisonCaseIdentity,
    pub current: ComparisonCaseSnapshot,
    pub baseline: ComparisonCaseSnapshot,
    /// Current minus baseline in the primary measurement's native unit.
    pub absolute_change: Option<f64>,
    /// Signed improvement: positive is better for both higher- and
    /// lower-is-better measurements.
    pub percent_improvement: Option<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub stability_warnings: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub metric_changes: Vec<MetricComparison>,
}

/// A benchmark present on only one side of a partial comparison.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct UnmatchedBenchmark {
    pub identity: ComparisonCaseIdentity,
    pub measurement: ComparisonCaseSnapshot,
}

/// Counts summarizing a structured comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ComparisonSummary {
    pub matched: usize,
    pub added: usize,
    pub removed: usize,
}

/// Kind of evidence document referenced by a comparison.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum ReportDocumentType {
    NativeBenchmark,
    Series,
}

/// Durable reference to one evidence document.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ReportReference {
    pub document_type: ReportDocumentType,
    pub schema_version: u32,
    pub suite: Option<String>,
    /// SHA-256 over the exact loaded bytes, or over the normal pretty JSON
    /// serialization for an in-memory report.
    pub content_digest: String,
    pub capture_time: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub source_provenance: BTreeMap<String, String>,
    /// Invocation-local hint only. It does not participate in identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_path: Option<String>,
}

impl ReportReference {
    fn native(report: &BenchmarkReport, bytes: &[u8], display_path: Option<&Path>) -> Self {
        let mut source_provenance = report.context.provenance.clone();
        if let Some(commit) = report.git_commit.as_deref()
            && !source_provenance.contains_key("commit")
        {
            source_provenance.insert("commit".to_string(), commit.to_string());
        }

        Self {
            document_type: ReportDocumentType::NativeBenchmark,
            schema_version: report.schema_version,
            suite: report.suite.clone(),
            content_digest: sha256_digest(bytes),
            capture_time: report.timestamp.clone(),
            source_provenance,
            display_path: display_path.map(|path| path.display().to_string()),
        }
    }

    fn series(report: &SeriesReport, bytes: &[u8], display_path: Option<&Path>) -> Self {
        Self {
            document_type: ReportDocumentType::Series,
            schema_version: report.schema_version,
            suite: Some(report.suite.clone()),
            content_digest: sha256_digest(bytes),
            capture_time: report.timestamp.clone(),
            source_provenance: report.context.provenance.clone(),
            display_path: display_path.map(|path| path.display().to_string()),
        }
    }
}

/// Loaded evidence plus the reference derived from its original bytes.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ReportDocument {
    Native {
        report: BenchmarkReport,
        reference: ReportReference,
    },
    Series {
        report: SeriesReport,
        reference: ReportReference,
    },
}

impl ReportDocument {
    /// Load a native benchmark or external series report and retain a digest
    /// of the exact bytes.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ReportError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| ReportError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        parse_report_document(&bytes, Some(path))
    }

    /// Wrap an in-memory report using its normal persisted representation.
    pub fn from_native(report: BenchmarkReport) -> Result<Self, ReportError> {
        validate_schema(report.schema_version, None)?;
        let bytes = serde_json::to_vec_pretty(&report)
            .map_err(|source| ReportError::MalformedReport { path: None, source })?;
        let reference = ReportReference::native(&report, &bytes, None);
        Ok(Self::Native { report, reference })
    }

    /// Wrap an in-memory series report using its normal persisted
    /// representation.
    pub fn from_series(report: SeriesReport) -> Result<Self, ReportError> {
        validate_series_report(&report, None)?;
        let bytes = serde_json::to_vec_pretty(&report)
            .map_err(|source| ReportError::MalformedReport { path: None, source })?;
        let reference = ReportReference::series(&report, &bytes, None);
        Ok(Self::Series { report, reference })
    }

    pub fn as_native(&self) -> Option<&BenchmarkReport> {
        match self {
            Self::Native { report, .. } => Some(report),
            Self::Series { .. } => None,
        }
    }

    pub fn as_series(&self) -> Option<&SeriesReport> {
        match self {
            Self::Native { .. } => None,
            Self::Series { report, .. } => Some(report),
        }
    }

    pub fn reference(&self) -> &ReportReference {
        match self {
            Self::Native { reference, .. } | Self::Series { reference, .. } => reference,
        }
    }
}

/// Serializable, policy-free relationship between two benchmark reports.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct ComparisonReport {
    pub schema_version: u32,
    pub current: ReportReference,
    pub baseline: ReportReference,
    pub suite: String,
    #[serde(default)]
    pub environment: EnvironmentComparison,
    pub matched: Vec<MatchedBenchmark>,
    pub added: Vec<UnmatchedBenchmark>,
    pub removed: Vec<UnmatchedBenchmark>,
    pub summary: ComparisonSummary,
}

/// Failure to load or interpret a persisted report.
#[derive(Debug)]
#[non_exhaustive]
pub enum ReportError {
    Io {
        path: PathBuf,
        source: io::Error,
    },
    MalformedJson {
        path: Option<PathBuf>,
        source: serde_json::Error,
    },
    MalformedReport {
        path: Option<PathBuf>,
        source: serde_json::Error,
    },
    InvalidSchemaVersion {
        path: Option<PathBuf>,
    },
    UnsupportedSchema {
        path: Option<PathBuf>,
        found: u32,
        supported: u32,
    },
    UnsupportedDocumentType {
        path: Option<PathBuf>,
        found: String,
    },
    InvalidReport {
        path: Option<PathBuf>,
        reason: String,
    },
}

impl fmt::Display for ReportError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => {
                write!(
                    formatter,
                    "failed to read report {}: {source}",
                    path.display()
                )
            }
            Self::MalformedJson { path, source } => {
                write!(formatter, "malformed JSON{}: {source}", path_suffix(path))
            }
            Self::MalformedReport { path, source } => {
                write!(
                    formatter,
                    "malformed benchmark report{}: {source}",
                    path_suffix(path)
                )
            }
            Self::InvalidSchemaVersion { path } => {
                write!(
                    formatter,
                    "benchmark report{} has an invalid schema_version",
                    path_suffix(path)
                )
            }
            Self::UnsupportedSchema {
                path,
                found,
                supported,
            } => write!(
                formatter,
                "report{} uses schema version {found}, but this version supports {supported}",
                path_suffix(path)
            ),
            Self::UnsupportedDocumentType { path, found } => write!(
                formatter,
                "unsupported report document_type {found:?}{}",
                path_suffix(path)
            ),
            Self::InvalidReport { path, reason } => {
                write!(formatter, "invalid report{}: {reason}", path_suffix(path))
            }
        }
    }
}

impl Error for ReportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            Self::MalformedJson { source, .. } | Self::MalformedReport { source, .. } => {
                Some(source)
            }
            Self::InvalidSchemaVersion { .. }
            | Self::UnsupportedSchema { .. }
            | Self::UnsupportedDocumentType { .. }
            | Self::InvalidReport { .. } => None,
        }
    }
}

/// Semantic failure while comparing two otherwise readable reports.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ComparisonError {
    UnsupportedSchema {
        side: ComparisonSide,
        found: u32,
        supported: u32,
    },
    MissingSuite {
        side: ComparisonSide,
    },
    SuiteMismatch {
        current: String,
        baseline: String,
    },
    UnknownRunner {
        side: ComparisonSide,
    },
    RunnerMismatch {
        current: String,
        baseline: String,
    },
    EnvironmentMismatch {
        current: BTreeMap<String, String>,
        baseline: BTreeMap<String, String>,
    },
    InvalidEnvironmentOverride,
    DuplicateIdentity {
        side: ComparisonSide,
        identity: ComparisonCaseIdentity,
    },
    InvalidReport {
        side: ComparisonSide,
        reason: String,
    },
    ResultSetMismatch {
        added: usize,
        removed: usize,
    },
    ReferenceSerialization {
        side: ComparisonSide,
        message: String,
    },
}

impl fmt::Display for ComparisonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedSchema {
                side,
                found,
                supported,
            } => write!(
                formatter,
                "{side} report uses schema version {found}, but this version supports {supported}"
            ),
            Self::MissingSuite { side } => write!(formatter, "{side} report has no suite identity"),
            Self::SuiteMismatch { current, baseline } => write!(
                formatter,
                "suite mismatch: current report is {current:?}, baseline report is {baseline:?}"
            ),
            Self::UnknownRunner { side } => {
                write!(formatter, "{side} report has no known runner identity")
            }
            Self::RunnerMismatch { current, baseline } => write!(
                formatter,
                "runner mismatch: current report is from {current:?}, baseline report is from {baseline:?}"
            ),
            Self::EnvironmentMismatch { current, baseline } => write!(
                formatter,
                "comparison environment mismatch: current is {current:?}, baseline is {baseline:?}"
            ),
            Self::InvalidEnvironmentOverride => {
                formatter.write_str("environment override reason must not be empty")
            }
            Self::DuplicateIdentity { side, identity } => write!(
                formatter,
                "{side} report contains duplicate benchmark identity {}/{}",
                identity.group(),
                identity.name()
            ),
            Self::InvalidReport { side, reason } => {
                write!(formatter, "{side} report is invalid: {reason}")
            }
            Self::ResultSetMismatch { added, removed } => write!(
                formatter,
                "result sets differ: {added} current-only and {removed} baseline-only benchmarks"
            ),
            Self::ReferenceSerialization { side, message } => {
                write!(formatter, "failed to identify {side} report: {message}")
            }
        }
    }
}

impl Error for ComparisonError {}

#[derive(Clone, Debug)]
struct NormalizedReport {
    suite: String,
    runner_id: String,
    environment: BTreeMap<String, String>,
    validity: Validity,
    cases: Vec<NormalizedCase>,
}

#[derive(Clone, Debug)]
struct NormalizedCase {
    identity: ComparisonCaseIdentity,
    primary: PrimaryMeasurement,
    statistics: ComparisonStatistics,
    native: NativeMeasurementProjection,
    validity: Validity,
    provenance: BTreeMap<String, String>,
    metrics: Vec<NormalizedMetric>,
}

#[derive(Clone, Debug)]
struct NormalizedMetric {
    name: String,
    unit: String,
    section: String,
    display_name: String,
    format: MetricFormat,
    median: Option<f64>,
}

impl BenchmarkReport {
    /// Load a native benchmark report with precise schema and parse errors.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ReportError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| ReportError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        parse_native_report(&bytes, Some(path))
    }

    /// Compare two in-memory native reports without terminal or filesystem
    /// side effects.
    pub fn compare(
        &self,
        baseline: &BenchmarkReport,
        options: &ComparisonOptions,
    ) -> Result<ComparisonReport, ComparisonError> {
        let current = in_memory_native_document(self, ComparisonSide::Current)?;
        let baseline = in_memory_native_document(baseline, ComparisonSide::Baseline)?;
        compare_reports(&current, &baseline, options)
    }
}

impl SeriesReport {
    /// Load an external series report with structural and semantic validation.
    pub fn load_from_path(path: impl AsRef<Path>) -> Result<Self, ReportError> {
        let path = path.as_ref();
        let bytes = fs::read(path).map_err(|source| ReportError::Io {
            path: path.to_path_buf(),
            source,
        })?;
        parse_series_report(&bytes, Some(path))
    }

    /// Compare two in-memory external series reports.
    pub fn compare(
        &self,
        baseline: &SeriesReport,
        options: &ComparisonOptions,
    ) -> Result<ComparisonReport, ComparisonError> {
        let current = in_memory_series_document(self, ComparisonSide::Current)?;
        let baseline = in_memory_series_document(baseline, ComparisonSide::Baseline)?;
        compare_reports(&current, &baseline, options)
    }
}

/// Compare loaded evidence documents without terminal or filesystem effects.
pub fn compare_reports(
    current: &ReportDocument,
    baseline: &ReportDocument,
    options: &ComparisonOptions,
) -> Result<ComparisonReport, ComparisonError> {
    let current_report = normalize_document(current, ComparisonSide::Current)?;
    let baseline_report = normalize_document(baseline, ComparisonSide::Baseline)?;
    compare_normalized_reports(
        current_report,
        current.reference().clone(),
        baseline_report,
        baseline.reference().clone(),
        options,
    )
}

fn compare_normalized_reports(
    current: NormalizedReport,
    current_reference: ReportReference,
    baseline: NormalizedReport,
    baseline_reference: ReportReference,
    options: &ComparisonOptions,
) -> Result<ComparisonReport, ComparisonError> {
    validate_report_validity(&current.validity, ComparisonSide::Current)?;
    validate_report_validity(&baseline.validity, ComparisonSide::Baseline)?;

    if current.suite != baseline.suite {
        return Err(ComparisonError::SuiteMismatch {
            current: current.suite,
            baseline: baseline.suite,
        });
    }

    validate_runner(&current.runner_id, ComparisonSide::Current)?;
    validate_runner(&baseline.runner_id, ComparisonSide::Baseline)?;

    if matches!(
        options.environment_override.as_ref(),
        Some(environment_override) if environment_override.reason.trim().is_empty()
    ) {
        return Err(ComparisonError::InvalidEnvironmentOverride);
    }

    let runner_matches = current.runner_id == baseline.runner_id;
    let environment_matches = current.environment == baseline.environment;
    if !runner_matches && options.environment_override.is_none() {
        return Err(ComparisonError::RunnerMismatch {
            current: current.runner_id,
            baseline: baseline.runner_id,
        });
    }
    if !environment_matches && options.environment_override.is_none() {
        return Err(ComparisonError::EnvironmentMismatch {
            current: current.environment,
            baseline: baseline.environment,
        });
    }
    let environment = EnvironmentComparison {
        current_runner_id: current.runner_id.clone(),
        baseline_runner_id: baseline.runner_id.clone(),
        current_environment: current.environment.clone(),
        baseline_environment: baseline.environment.clone(),
        exact_match: runner_matches && environment_matches,
        operator_override: options.environment_override.clone(),
    };

    reject_duplicate_identities(&current.cases, ComparisonSide::Current)?;
    reject_duplicate_identities(&baseline.cases, ComparisonSide::Baseline)?;

    let pairs = pair_normalized_cases(&current.cases, &baseline.cases);
    let mut matched_current = vec![false; current.cases.len()];
    let mut matched_baseline = vec![false; baseline.cases.len()];
    let mut matched = Vec::with_capacity(pairs.len());

    for (current_index, baseline_index) in pairs {
        matched_current[current_index] = true;
        matched_baseline[baseline_index] = true;
        matched.push(matched_benchmark(
            &current.cases[current_index],
            &baseline.cases[baseline_index],
        ));
    }

    let added: Vec<_> = current
        .cases
        .iter()
        .enumerate()
        .filter(|(index, _)| !matched_current[*index])
        .map(|(_, result)| unmatched_benchmark(result))
        .collect();
    let removed: Vec<_> = baseline
        .cases
        .iter()
        .enumerate()
        .filter(|(index, _)| !matched_baseline[*index])
        .map(|(_, result)| unmatched_benchmark(result))
        .collect();

    if !options.allow_partial_result_set && (!added.is_empty() || !removed.is_empty()) {
        return Err(ComparisonError::ResultSetMismatch {
            added: added.len(),
            removed: removed.len(),
        });
    }

    let summary = ComparisonSummary {
        matched: matched.len(),
        added: added.len(),
        removed: removed.len(),
    };

    Ok(ComparisonReport {
        schema_version: COMPARISON_SCHEMA_VERSION,
        current: current_reference,
        baseline: baseline_reference,
        suite: current.suite,
        environment,
        matched,
        added,
        removed,
        summary,
    })
}

fn validate_report_validity(
    validity: &Validity,
    side: ComparisonSide,
) -> Result<(), ComparisonError> {
    if validity.status == ValidityStatus::Invalid {
        return Err(ComparisonError::InvalidReport {
            side,
            reason: validity
                .reason
                .clone()
                .unwrap_or_else(|| "no reason supplied".to_string()),
        });
    }
    Ok(())
}

fn validate_runner(hostname: &str, side: ComparisonSide) -> Result<(), ComparisonError> {
    if hostname.trim().is_empty() || hostname.eq_ignore_ascii_case("unknown") {
        return Err(ComparisonError::UnknownRunner { side });
    }
    Ok(())
}

fn effective_runner_id(report: &BenchmarkReport) -> &str {
    if report.context.runner_id.trim().is_empty() {
        &report.hostname
    } else {
        &report.context.runner_id
    }
}

fn normalize_document(
    document: &ReportDocument,
    side: ComparisonSide,
) -> Result<NormalizedReport, ComparisonError> {
    match document {
        ReportDocument::Native { report, .. } => normalize_native_report(report, side),
        ReportDocument::Series { report, .. } => normalize_series_report(report, side),
    }
}

fn normalize_native_report(
    report: &BenchmarkReport,
    side: ComparisonSide,
) -> Result<NormalizedReport, ComparisonError> {
    if report.schema_version != REPORT_SCHEMA_VERSION {
        return Err(ComparisonError::UnsupportedSchema {
            side,
            found: report.schema_version,
            supported: REPORT_SCHEMA_VERSION,
        });
    }
    let suite = report
        .suite
        .as_deref()
        .filter(|suite| !suite.trim().is_empty())
        .ok_or(ComparisonError::MissingSuite { side })?;
    Ok(NormalizedReport {
        suite: suite.to_string(),
        runner_id: effective_runner_id(report).to_string(),
        environment: report.context.environment.clone(),
        validity: Validity::valid(),
        cases: report.results.iter().map(normalize_native_case).collect(),
    })
}

fn normalize_series_report(
    report: &SeriesReport,
    side: ComparisonSide,
) -> Result<NormalizedReport, ComparisonError> {
    if report.schema_version != SERIES_SCHEMA_VERSION {
        return Err(ComparisonError::UnsupportedSchema {
            side,
            found: report.schema_version,
            supported: SERIES_SCHEMA_VERSION,
        });
    }
    report
        .validate()
        .map_err(|error| ComparisonError::InvalidReport {
            side,
            reason: error.to_string(),
        })?;
    Ok(NormalizedReport {
        suite: report.suite.clone(),
        runner_id: report.context.runner_id.clone(),
        environment: report.context.environment.clone(),
        validity: report.validity.clone(),
        cases: report.results.iter().map(normalize_series_case).collect(),
    })
}

fn normalize_native_case(result: &BenchmarkResult) -> NormalizedCase {
    let median_throughput = median_throughput(result);
    let samples: Vec<_> = result
        .stats
        .sample_throughput_per_sec
        .iter()
        .copied()
        .filter(|sample| sample.is_finite())
        .collect();
    let throughput_mad = primary_mad(result, median_throughput);
    NormalizedCase {
        identity: ComparisonCaseIdentity::Native(BenchmarkCaseIdentity::from_result(result)),
        primary: PrimaryMeasurement {
            measurement: MeasurementKind::Throughput,
            unit: format!("{}/s", result.stats.throughput.unit()),
            direction: MeasurementDirection::Higher,
            samples,
            value: finite_nonnegative(median_throughput),
        },
        statistics: ComparisonStatistics {
            cv_percent: finite_nonnegative(result.stats.cv_percent),
            mad: throughput_mad,
            p95: primary_p95(result),
            samples: result.stats.samples,
            outliers: primary_outlier_count(result),
        },
        native: NativeMeasurementProjection {
            mean_throughput_per_sec: finite_nonnegative(mean_throughput(result)),
            median_throughput_per_sec: finite_nonnegative(median_throughput),
            median_ns_per_op: finite_nonnegative(result_median_latency(result)),
            p95_ns_per_op: finite_nonnegative(result_p95_latency(result)),
            mad_ns_per_op: finite_nonnegative(result_mad_latency(result)),
        },
        validity: Validity::valid(),
        provenance: BTreeMap::new(),
        metrics: result
            .stats
            .metrics
            .iter()
            .map(|metric| NormalizedMetric {
                name: metric.name.clone(),
                unit: metric.unit.clone(),
                section: metric.section.clone(),
                display_name: metric.display_name.clone(),
                format: metric.format,
                median: finite(metric.median),
            })
            .collect(),
    }
}

fn normalize_series_case(result: &SeriesResult) -> NormalizedCase {
    let mut sorted = result.samples.clone();
    sorted.sort_by(f64::total_cmp);
    let median = (!sorted.is_empty()).then(|| percentile(&sorted, 0.5));
    let p95 = (!sorted.is_empty()).then(|| percentile(&sorted, 0.95));
    let mad = median.map(|median| {
        let mut deviations: Vec<_> = sorted
            .iter()
            .map(|sample| (sample - median).abs())
            .collect();
        deviations.sort_by(f64::total_cmp);
        percentile(&deviations, 0.5)
    });

    NormalizedCase {
        identity: ComparisonCaseIdentity::Series(SeriesCaseIdentity::from_result(result)),
        primary: PrimaryMeasurement {
            measurement: result.measurement,
            unit: result.unit.clone(),
            direction: result.direction,
            samples: result.samples.clone(),
            value: result.validity.is_valid().then_some(median).flatten(),
        },
        statistics: ComparisonStatistics {
            cv_percent: coefficient_of_variation_percent(&sorted),
            mad,
            p95,
            samples: sorted.len(),
            outliers: tukey_outlier_count(&sorted),
        },
        native: NativeMeasurementProjection {
            mean_throughput_per_sec: None,
            median_throughput_per_sec: None,
            median_ns_per_op: None,
            p95_ns_per_op: None,
            mad_ns_per_op: None,
        },
        validity: result.validity.clone(),
        provenance: result.provenance.clone(),
        metrics: Vec::new(),
    }
}

fn reject_duplicate_identities(
    cases: &[NormalizedCase],
    side: ComparisonSide,
) -> Result<(), ComparisonError> {
    for (index, case) in cases.iter().enumerate() {
        if cases[..index]
            .iter()
            .any(|previous| case.identity == previous.identity)
        {
            return Err(ComparisonError::DuplicateIdentity {
                side,
                identity: case.identity.clone(),
            });
        }
    }
    Ok(())
}

fn pair_normalized_cases(
    current: &[NormalizedCase],
    baseline: &[NormalizedCase],
) -> Vec<(usize, usize)> {
    let mut matched_baseline = vec![false; baseline.len()];
    let mut pairs = Vec::with_capacity(current.len().min(baseline.len()));
    for (current_index, current_case) in current.iter().enumerate() {
        let Some((baseline_index, _)) =
            baseline
                .iter()
                .enumerate()
                .find(|(baseline_index, baseline_case)| {
                    !matched_baseline[*baseline_index]
                        && current_case.identity == baseline_case.identity
                })
        else {
            continue;
        };
        matched_baseline[baseline_index] = true;
        pairs.push((current_index, baseline_index));
    }
    pairs
}

fn matched_benchmark(current: &NormalizedCase, baseline: &NormalizedCase) -> MatchedBenchmark {
    let current_snapshot = comparison_snapshot(current);
    let baseline_snapshot = comparison_snapshot(baseline);
    let current_value = current_snapshot.primary.value;
    let baseline_value = baseline_snapshot.primary.value;
    let absolute_change = finite_difference(current_value, baseline_value);
    let percent_improvement = improvement_percent(
        current_value,
        baseline_value,
        current_snapshot.primary.direction,
    );
    let mut stability_warnings = Vec::new();
    if !current.validity.is_valid() {
        stability_warnings.push(format!(
            "current result is invalid: {}",
            current
                .validity
                .reason
                .as_deref()
                .unwrap_or("no reason supplied")
        ));
    } else if current_value.is_none() {
        stability_warnings.push("current primary measurement is not finite".to_string());
    }
    if !baseline.validity.is_valid() {
        stability_warnings.push(format!(
            "baseline result is invalid: {}",
            baseline
                .validity
                .reason
                .as_deref()
                .unwrap_or("no reason supplied")
        ));
    } else if baseline_value.is_none() {
        stability_warnings.push("baseline primary measurement is not finite".to_string());
    }

    MatchedBenchmark {
        identity: current.identity.clone(),
        current: current_snapshot,
        baseline: baseline_snapshot,
        absolute_change,
        percent_improvement,
        stability_warnings,
        metric_changes: compare_metrics(current, baseline),
    }
}

fn unmatched_benchmark(result: &NormalizedCase) -> UnmatchedBenchmark {
    UnmatchedBenchmark {
        identity: result.identity.clone(),
        measurement: comparison_snapshot(result),
    }
}

fn comparison_snapshot(result: &NormalizedCase) -> ComparisonCaseSnapshot {
    ComparisonCaseSnapshot {
        primary: result.primary.clone(),
        statistics: result.statistics.clone(),
        native: result.native.clone(),
        validity: result.validity.clone(),
        provenance: result.provenance.clone(),
    }
}

fn compare_metrics(current: &NormalizedCase, baseline: &NormalizedCase) -> Vec<MetricComparison> {
    let mut matched_baseline = vec![false; baseline.metrics.len()];
    let mut comparisons = Vec::new();
    for current_metric in &current.metrics {
        let Some((index, baseline_metric)) =
            baseline
                .metrics
                .iter()
                .enumerate()
                .find(|(index, candidate)| {
                    !matched_baseline[*index]
                        && current_metric.name == candidate.name
                        && current_metric.unit == candidate.unit
                        && current_metric.section == candidate.section
                })
        else {
            continue;
        };
        matched_baseline[index] = true;
        let current_median = current_metric.median;
        let baseline_median = baseline_metric.median;
        comparisons.push(MetricComparison {
            name: current_metric.name.clone(),
            unit: current_metric.unit.clone(),
            section: current_metric.section.clone(),
            display_name: current_metric.display_name.clone(),
            format: current_metric.format,
            current_median,
            baseline_median,
            absolute_change: finite_difference(current_median, baseline_median),
            percent_change: raw_percent_change(current_median, baseline_median),
        });
    }
    comparisons
}

fn in_memory_native_document(
    report: &BenchmarkReport,
    side: ComparisonSide,
) -> Result<ReportDocument, ComparisonError> {
    if report.schema_version != REPORT_SCHEMA_VERSION {
        return Err(ComparisonError::UnsupportedSchema {
            side,
            found: report.schema_version,
            supported: REPORT_SCHEMA_VERSION,
        });
    }
    let bytes = serde_json::to_vec_pretty(report).map_err(|error| {
        ComparisonError::ReferenceSerialization {
            side,
            message: error.to_string(),
        }
    })?;
    Ok(ReportDocument::Native {
        report: report.clone(),
        reference: ReportReference::native(report, &bytes, None),
    })
}

fn in_memory_series_document(
    report: &SeriesReport,
    side: ComparisonSide,
) -> Result<ReportDocument, ComparisonError> {
    if report.schema_version != SERIES_SCHEMA_VERSION {
        return Err(ComparisonError::UnsupportedSchema {
            side,
            found: report.schema_version,
            supported: SERIES_SCHEMA_VERSION,
        });
    }
    report
        .validate()
        .map_err(|error| ComparisonError::InvalidReport {
            side,
            reason: error.to_string(),
        })?;
    let bytes = serde_json::to_vec_pretty(report).map_err(|error| {
        ComparisonError::ReferenceSerialization {
            side,
            message: error.to_string(),
        }
    })?;
    Ok(ReportDocument::Series {
        report: report.clone(),
        reference: ReportReference::series(report, &bytes, None),
    })
}

fn parse_report_document(bytes: &[u8], path: Option<&Path>) -> Result<ReportDocument, ReportError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| ReportError::MalformedJson {
            path: path.map(Path::to_path_buf),
            source,
        })?;
    match value.get("document_type") {
        Some(serde_json::Value::String(document_type)) if document_type == SERIES_DOCUMENT_TYPE => {
            let report = parse_series_value(value, path)?;
            let reference = ReportReference::series(&report, bytes, path);
            Ok(ReportDocument::Series { report, reference })
        }
        Some(serde_json::Value::String(document_type)) => {
            Err(ReportError::UnsupportedDocumentType {
                path: path.map(Path::to_path_buf),
                found: document_type.clone(),
            })
        }
        Some(_) => Err(ReportError::InvalidReport {
            path: path.map(Path::to_path_buf),
            reason: "document_type must be a string".to_string(),
        }),
        None => {
            let report = parse_native_value(value, path)?;
            let reference = ReportReference::native(&report, bytes, path);
            Ok(ReportDocument::Native { report, reference })
        }
    }
}

fn parse_native_report(bytes: &[u8], path: Option<&Path>) -> Result<BenchmarkReport, ReportError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| ReportError::MalformedJson {
            path: path.map(Path::to_path_buf),
            source,
        })?;
    parse_native_value(value, path)
}

fn parse_native_value(
    value: serde_json::Value,
    path: Option<&Path>,
) -> Result<BenchmarkReport, ReportError> {
    let schema_version = match value.get("schema_version") {
        None => REPORT_SCHEMA_VERSION,
        Some(value) => {
            let Some(version) = value
                .as_u64()
                .and_then(|version| u32::try_from(version).ok())
            else {
                return Err(ReportError::InvalidSchemaVersion {
                    path: path.map(Path::to_path_buf),
                });
            };
            version
        }
    };
    validate_schema(schema_version, path)?;

    serde_json::from_value(value).map_err(|source| ReportError::MalformedReport {
        path: path.map(Path::to_path_buf),
        source,
    })
}

fn parse_series_report(bytes: &[u8], path: Option<&Path>) -> Result<SeriesReport, ReportError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|source| ReportError::MalformedJson {
            path: path.map(Path::to_path_buf),
            source,
        })?;
    parse_series_value(value, path)
}

fn parse_series_value(
    value: serde_json::Value,
    path: Option<&Path>,
) -> Result<SeriesReport, ReportError> {
    let document_type = value
        .get("document_type")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| ReportError::InvalidReport {
            path: path.map(Path::to_path_buf),
            reason: format!("document_type must be {SERIES_DOCUMENT_TYPE:?}"),
        })?;
    if document_type != SERIES_DOCUMENT_TYPE {
        return Err(ReportError::UnsupportedDocumentType {
            path: path.map(Path::to_path_buf),
            found: document_type.to_string(),
        });
    }
    let schema_version = value
        .get("schema_version")
        .and_then(serde_json::Value::as_u64)
        .and_then(|version| u32::try_from(version).ok())
        .ok_or_else(|| ReportError::InvalidSchemaVersion {
            path: path.map(Path::to_path_buf),
        })?;
    if schema_version != SERIES_SCHEMA_VERSION {
        return Err(ReportError::UnsupportedSchema {
            path: path.map(Path::to_path_buf),
            found: schema_version,
            supported: SERIES_SCHEMA_VERSION,
        });
    }
    let report: SeriesReport =
        serde_json::from_value(value).map_err(|source| ReportError::MalformedReport {
            path: path.map(Path::to_path_buf),
            source,
        })?;
    validate_series_report(&report, path)?;
    Ok(report)
}

fn validate_series_report(report: &SeriesReport, path: Option<&Path>) -> Result<(), ReportError> {
    report
        .validate()
        .map_err(|error| ReportError::InvalidReport {
            path: path.map(Path::to_path_buf),
            reason: error.to_string(),
        })
}

fn validate_schema(schema_version: u32, path: Option<&Path>) -> Result<(), ReportError> {
    if schema_version != REPORT_SCHEMA_VERSION {
        return Err(ReportError::UnsupportedSchema {
            path: path.map(Path::to_path_buf),
            found: schema_version,
            supported: REPORT_SCHEMA_VERSION,
        });
    }
    Ok(())
}

fn path_suffix(path: &Option<PathBuf>) -> String {
    path.as_ref()
        .map(|path| format!(" at {}", path.display()))
        .unwrap_or_default()
}

fn sha256_digest(bytes: &[u8]) -> String {
    format!("sha256:{:x}", Sha256::digest(bytes))
}

fn finite(value: f64) -> Option<f64> {
    value.is_finite().then_some(value)
}

fn finite_nonnegative(value: f64) -> Option<f64> {
    (value.is_finite() && value >= 0.0).then_some(value)
}

fn finite_difference(current: Option<f64>, baseline: Option<f64>) -> Option<f64> {
    let difference = current? - baseline?;
    finite(difference)
}

fn raw_percent_change(current: Option<f64>, baseline: Option<f64>) -> Option<f64> {
    let current = current?;
    let baseline = baseline?;
    if baseline.abs() <= f64::EPSILON {
        return None;
    }
    finite(((current - baseline) / baseline) * 100.0)
}

fn improvement_percent(
    current: Option<f64>,
    baseline: Option<f64>,
    direction: MeasurementDirection,
) -> Option<f64> {
    let current = current?;
    let baseline = baseline?;
    if baseline.abs() <= f64::EPSILON {
        return None;
    }
    let ratio = match direction {
        MeasurementDirection::Higher => (current - baseline) / baseline,
        MeasurementDirection::Lower => (baseline - current) / baseline,
        MeasurementDirection::Informational => return None,
    };
    finite(ratio * 100.0)
}

fn mean_throughput(result: &BenchmarkResult) -> f64 {
    if result.stats.sample_throughput_per_sec.is_empty() {
        return result.stats.throughput_per_sec;
    }
    result.stats.sample_throughput_per_sec.iter().sum::<f64>()
        / result.stats.sample_throughput_per_sec.len() as f64
}

fn median_throughput(result: &BenchmarkResult) -> f64 {
    if result.stats.median_throughput_per_sec.is_finite()
        && result.stats.median_throughput_per_sec > 0.0
    {
        return result.stats.median_throughput_per_sec;
    }
    if result.stats.sample_throughput_per_sec.is_empty() {
        return result.stats.throughput_per_sec;
    }
    percentile(&result.stats.sample_throughput_per_sec, 0.5)
}

fn result_median_latency(result: &BenchmarkResult) -> f64 {
    if result.stats.sample_latency_ns_per_op.is_empty() {
        return result.stats.median_ns_per_op;
    }
    percentile(&result.stats.sample_latency_ns_per_op, 0.5)
}

fn result_p95_latency(result: &BenchmarkResult) -> f64 {
    if result.stats.sample_latency_ns_per_op.is_empty() {
        return result.stats.p95_ns_per_op;
    }
    percentile(&result.stats.sample_latency_ns_per_op, 0.95)
}

fn result_mad_latency(result: &BenchmarkResult) -> f64 {
    if result.stats.sample_latency_ns_per_op.is_empty() {
        return result.stats.mad_ns_per_op;
    }
    let median = result_median_latency(result);
    let deviations: Vec<_> = result
        .stats
        .sample_latency_ns_per_op
        .iter()
        .map(|value| (value - median).abs())
        .collect();
    percentile(&deviations, 0.5)
}

fn primary_mad(result: &BenchmarkResult, median: f64) -> Option<f64> {
    if result.stats.sample_throughput_per_sec.is_empty() || !median.is_finite() {
        return None;
    }
    let deviations: Vec<_> = result
        .stats
        .sample_throughput_per_sec
        .iter()
        .filter(|value| value.is_finite())
        .map(|value| (value - median).abs())
        .collect();
    if deviations.is_empty() {
        return None;
    }
    finite(percentile(&deviations, 0.5))
}

fn primary_p95(result: &BenchmarkResult) -> Option<f64> {
    let samples: Vec<_> = result
        .stats
        .sample_throughput_per_sec
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    if samples.is_empty() {
        return None;
    }
    finite(percentile(&samples, 0.95))
}

fn primary_outlier_count(result: &BenchmarkResult) -> usize {
    let samples: Vec<_> = result
        .stats
        .sample_throughput_per_sec
        .iter()
        .copied()
        .filter(|value| value.is_finite())
        .collect();
    tukey_outlier_count(&samples)
}

fn coefficient_of_variation_percent(samples: &[f64]) -> Option<f64> {
    if samples.is_empty() {
        return None;
    }
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    if !mean.is_finite() || mean.abs() <= f64::EPSILON {
        return None;
    }
    let variance = samples
        .iter()
        .map(|sample| (sample - mean).powi(2))
        .sum::<f64>()
        / samples.len() as f64;
    finite((variance.sqrt() / mean.abs()) * 100.0)
}

fn tukey_outlier_count(samples: &[f64]) -> usize {
    if samples.len() < 4 {
        return 0;
    }
    let mut samples = samples.to_vec();
    samples.sort_by(|a, b| a.total_cmp(b));
    let q1 = percentile(&samples, 0.25);
    let q3 = percentile(&samples, 0.75);
    let iqr = q3 - q1;
    let lower = q1 - 1.5 * iqr;
    let upper = q3 + 1.5 * iqr;
    samples
        .iter()
        .filter(|value| **value < lower || **value > upper)
        .count()
}

fn percentile(values: &[f64], percentile: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }
    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let percentile = percentile.clamp(0.0, 1.0);
    let position = percentile * (sorted.len() - 1) as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        return sorted[lower];
    }
    let weight = position - lower as f64;
    sorted[lower] * (1.0 - weight) + sorted[upper] * weight
}

pub(crate) fn result_identity_matches(
    current: &BenchmarkResult,
    previous: &BenchmarkResult,
) -> bool {
    current.name == previous.name
        && current.group == previous.group
        && current.kind == previous.kind
        && current.metadata == previous.metadata
        && current.stats.throughput == previous.stats.throughput
        && current.stats.measurement_domain == previous.stats.measurement_domain
}

pub(crate) fn pair_results_one_to_one<'current, 'previous>(
    current_results: &'current [BenchmarkResult],
    previous_results: &'previous [BenchmarkResult],
) -> Vec<(&'current BenchmarkResult, &'previous BenchmarkResult)> {
    let mut matched_previous = vec![false; previous_results.len()];
    let mut pairs = Vec::with_capacity(current_results.len().min(previous_results.len()));
    for current in current_results {
        let Some((index, previous)) =
            previous_results
                .iter()
                .enumerate()
                .find(|(index, previous)| {
                    !matched_previous[*index] && result_identity_matches(current, previous)
                })
        else {
            continue;
        };
        matched_previous[index] = true;
        pairs.push((current, previous));
    }
    pairs
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{BenchmarkStats, MetricSummary, ReportContext};

    fn result(group: &str, name: &str, rate: f64) -> BenchmarkResult {
        BenchmarkResult {
            name: name.to_string(),
            group: group.to_string(),
            kind: BenchmarkKind::Standard,
            execution_index: 0,
            stats: BenchmarkStats {
                throughput: Throughput::ops(),
                throughput_per_sec: rate,
                median_throughput_per_sec: rate,
                ns_per_op: 1_000_000_000.0 / rate,
                median_ns_per_op: 1_000_000_000.0 / rate,
                p95_ns_per_op: 1_000_000_000.0 / rate,
                mad_ns_per_op: 0.0,
                cycles_per_op: 0.0,
                instructions_per_op: 0.0,
                ipc: 0.0,
                cache_references_per_op: 0.0,
                l1i_misses_per_op: 0.0,
                branches_per_op: 0.0,
                branch_miss_rate: 0.0,
                branch_misses_per_op: 0.0,
                cache_misses_per_op: 0.0,
                cache_miss_percent: 0.0,
                frontend_stall_cycles_per_op: 0.0,
                frontend_stall_percent: 0.0,
                backend_stall_cycles_per_op: 0.0,
                backend_stall_percent: 0.0,
                cv_percent: 1.0,
                outlier_count: 0,
                samples: 3,
                operations: 3,
                total_duration_sec: 1.0,
                sample_throughput_per_sec: vec![rate, rate, rate],
                sample_latency_ns_per_op: vec![
                    1_000_000_000.0 / rate,
                    1_000_000_000.0 / rate,
                    1_000_000_000.0 / rate,
                ],
                has_cycles: false,
                has_instructions: false,
                has_cache_references: false,
                has_l1i_misses: false,
                has_branches: false,
                has_branch_misses: false,
                has_cache_misses: false,
                has_stalled_cycles_frontend: false,
                has_stalled_cycles_backend: false,
                pmu_time_enabled_ns: 0,
                pmu_time_running_ns: 0,
                measurement_domain: MeasurementDomain::Cpu,
                measurement_label: String::new(),
                emits_cpu_diagnostics: true,
                metrics: Vec::new(),
                sample_metrics: Vec::new(),
            },
            worker_summaries: Vec::new(),
            metadata: BTreeMap::new(),
        }
    }

    fn report(names_and_rates: &[(&str, f64)]) -> BenchmarkReport {
        BenchmarkReport {
            schema_version: REPORT_SCHEMA_VERSION,
            timestamp: "123".to_string(),
            hostname: "host-a".to_string(),
            suite: Some("suite-a".to_string()),
            git_commit: Some("abc123".to_string()),
            context: ReportContext::default(),
            results: names_and_rates
                .iter()
                .map(|(name, rate)| result("group", name, *rate))
                .collect(),
        }
    }

    #[test]
    fn native_comparison_is_direction_aware_and_serializable() {
        let current = report(&[("a", 120.0)]);
        let baseline = report(&[("a", 100.0)]);
        let comparison = current
            .compare(&baseline, &ComparisonOptions::default())
            .unwrap();

        assert_eq!(comparison.summary.matched, 1);
        assert_eq!(
            comparison.matched[0].current.primary.measurement,
            MeasurementKind::Throughput
        );
        assert_eq!(
            comparison.matched[0].current.primary.direction,
            MeasurementDirection::Higher
        );
        assert_eq!(comparison.matched[0].percent_improvement, Some(20.0));
        let document = serde_json::to_value(&comparison).unwrap();
        assert_eq!(document["schema_version"], COMPARISON_SCHEMA_VERSION);
        assert_eq!(document["matched"][0]["percent_improvement"], 20.0);
    }

    #[test]
    fn signed_improvement_respects_measurement_direction() {
        assert_eq!(
            improvement_percent(Some(80.0), Some(100.0), MeasurementDirection::Lower),
            Some(20.0)
        );
        assert_eq!(
            improvement_percent(Some(120.0), Some(100.0), MeasurementDirection::Higher),
            Some(20.0)
        );
        assert_eq!(
            improvement_percent(
                Some(120.0),
                Some(100.0),
                MeasurementDirection::Informational
            ),
            None
        );
    }

    #[test]
    fn partial_comparison_reports_added_and_removed_cases() {
        let current = report(&[("a", 120.0), ("d", 40.0)]);
        let baseline = report(&[("a", 100.0), ("c", 30.0)]);

        let strict = current.compare(&baseline, &ComparisonOptions::default());
        assert_eq!(
            strict.unwrap_err(),
            ComparisonError::ResultSetMismatch {
                added: 1,
                removed: 1
            }
        );

        let comparison = current
            .compare(
                &baseline,
                &ComparisonOptions::default().allow_partial_result_set(true),
            )
            .unwrap();
        assert_eq!(
            comparison.summary,
            ComparisonSummary {
                matched: 1,
                added: 1,
                removed: 1
            }
        );
        assert_eq!(comparison.added[0].identity.name(), "d");
        assert_eq!(comparison.removed[0].identity.name(), "c");
    }

    #[test]
    fn duplicate_identities_are_explicit_errors() {
        let current = report(&[("a", 120.0), ("a", 130.0)]);
        let baseline = report(&[("a", 100.0)]);
        let error = current
            .compare(
                &baseline,
                &ComparisonOptions::default().allow_partial_result_set(true),
            )
            .unwrap_err();
        assert!(matches!(
            error,
            ComparisonError::DuplicateIdentity {
                side: ComparisonSide::Current,
                ..
            }
        ));
    }

    #[test]
    fn identity_changes_are_added_and_removed_not_numeric_matches() {
        let mut current = report(&[("a", 120.0)]);
        current.results[0]
            .metadata
            .insert("cache".to_string(), "cold".to_string());
        let baseline = report(&[("a", 100.0)]);

        let comparison = current
            .compare(
                &baseline,
                &ComparisonOptions::default().allow_partial_result_set(true),
            )
            .unwrap();
        assert_eq!(comparison.summary.matched, 0);
        assert_eq!(comparison.summary.added, 1);
        assert_eq!(comparison.summary.removed, 1);
    }

    #[test]
    fn matching_custom_metrics_are_compared() {
        let mut current = report(&[("a", 120.0)]);
        let mut baseline = report(&[("a", 100.0)]);
        current.results[0].stats.metrics.push(MetricSummary {
            name: "power".to_string(),
            unit: "W".to_string(),
            section: "gpu".to_string(),
            display_name: "Power".to_string(),
            format: MetricFormat::Number,
            mean: 220.0,
            median: 220.0,
            p95: 220.0,
            min: 220.0,
            max: 220.0,
            samples: 1,
        });
        baseline.results[0].stats.metrics.push(MetricSummary {
            name: "power".to_string(),
            unit: "W".to_string(),
            section: "gpu".to_string(),
            display_name: "Power".to_string(),
            format: MetricFormat::Number,
            mean: 200.0,
            median: 200.0,
            p95: 200.0,
            min: 200.0,
            max: 200.0,
            samples: 1,
        });

        let comparison = current
            .compare(&baseline, &ComparisonOptions::default())
            .unwrap();
        assert_eq!(comparison.matched[0].metric_changes.len(), 1);
        assert_eq!(
            comparison.matched[0].metric_changes[0].percent_change,
            Some(10.0)
        );
    }

    #[test]
    fn report_document_digest_uses_exact_loaded_bytes() {
        assert_eq!(
            sha256_digest(b"abc"),
            "sha256:ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );

        let report = report(&[("a", 100.0)]);
        let bytes = serde_json::to_vec(&report).unwrap();
        let path = std::env::temp_dir().join(format!(
            "micromeasure-comparison-report-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::write(&path, &bytes).unwrap();

        let loaded = ReportDocument::load_from_path(&path).unwrap();
        assert_eq!(loaded.reference().content_digest, sha256_digest(&bytes));
        assert_eq!(
            loaded.reference().display_path.as_deref(),
            Some(path.to_string_lossy().as_ref())
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn loaded_comparison_retains_both_exact_report_references() {
        let current_report = report(&[("a", 120.0)]);
        let baseline_report = report(&[("a", 100.0)]);
        let current_bytes = serde_json::to_vec(&current_report).unwrap();
        let baseline_bytes = serde_json::to_vec_pretty(&baseline_report).unwrap();
        let directory = std::env::temp_dir().join(format!(
            "micromeasure-loaded-comparison-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let current_path = directory.join("current.json");
        let baseline_path = directory.join("baseline.json");
        fs::write(&current_path, &current_bytes).unwrap();
        fs::write(&baseline_path, &baseline_bytes).unwrap();

        let current = ReportDocument::load_from_path(&current_path).unwrap();
        let baseline = ReportDocument::load_from_path(&baseline_path).unwrap();
        let comparison =
            compare_reports(&current, &baseline, &ComparisonOptions::default()).unwrap();
        assert_eq!(
            comparison.current.content_digest,
            sha256_digest(&current_bytes)
        );
        assert_eq!(
            comparison.baseline.content_digest,
            sha256_digest(&baseline_bytes)
        );
        assert_eq!(comparison.matched[0].percent_improvement, Some(20.0));

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn loading_distinguishes_malformed_and_unsupported_reports() {
        let malformed = br#"{"schema_version":1,"results":"wrong"}"#;
        assert!(matches!(
            parse_native_report(malformed, None),
            Err(ReportError::MalformedReport { .. })
        ));

        let future = br#"{"schema_version":999}"#;
        assert!(matches!(
            parse_native_report(future, None),
            Err(ReportError::UnsupportedSchema { found: 999, .. })
        ));
    }

    #[test]
    fn public_loading_accepts_current_and_legacy_reports() {
        let report = report(&[("a", 100.0)]);
        let mut legacy = serde_json::to_value(&report).unwrap();
        legacy.as_object_mut().unwrap().remove("schema_version");
        let directory = std::env::temp_dir().join(format!(
            "micromeasure-public-loading-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&directory).unwrap();
        let current_path = directory.join("current.json");
        let legacy_path = directory.join("legacy.json");
        fs::write(&current_path, serde_json::to_vec(&report).unwrap()).unwrap();
        fs::write(&legacy_path, serde_json::to_vec(&legacy).unwrap()).unwrap();

        assert_eq!(
            BenchmarkReport::load_from_path(&current_path)
                .unwrap()
                .schema_version,
            REPORT_SCHEMA_VERSION
        );
        assert_eq!(
            BenchmarkReport::load_from_path(&legacy_path)
                .unwrap()
                .schema_version,
            REPORT_SCHEMA_VERSION
        );

        fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn non_comparable_primary_values_remain_structured_and_serializable() {
        let mut current = report(&[("a", f64::NAN)]);
        current.results[0].stats.median_throughput_per_sec = f64::NAN;
        current.results[0].stats.sample_throughput_per_sec.clear();
        let baseline = report(&[("a", 0.0)]);

        let comparison = current
            .compare(&baseline, &ComparisonOptions::default())
            .unwrap();
        let matched = &comparison.matched[0];
        assert_eq!(matched.current.primary.value, None);
        assert_eq!(matched.percent_improvement, None);
        assert!(!matched.stability_warnings.is_empty());
        serde_json::to_vec(&comparison).unwrap();
    }

    #[test]
    fn report_level_mismatches_have_precise_errors() {
        let current = report(&[("a", 100.0)]);
        let mut baseline = report(&[("a", 100.0)]);
        baseline.hostname = "host-b".to_string();
        assert_eq!(
            current
                .compare(&baseline, &ComparisonOptions::default())
                .unwrap_err(),
            ComparisonError::RunnerMismatch {
                current: "host-a".to_string(),
                baseline: "host-b".to_string()
            }
        );

        baseline.hostname = "host-a".to_string();
        baseline.suite = Some("suite-b".to_string());
        assert_eq!(
            current
                .compare(&baseline, &ComparisonOptions::default())
                .unwrap_err(),
            ComparisonError::SuiteMismatch {
                current: "suite-a".to_string(),
                baseline: "suite-b".to_string()
            }
        );
    }

    #[test]
    fn runner_and_environment_must_match_exactly() {
        let mut current = report(&[("a", 100.0)]);
        let mut baseline = report(&[("a", 100.0)]);
        current.hostname = "ephemeral-current".to_string();
        baseline.hostname = "ephemeral-baseline".to_string();
        current.context = ReportContext::new("stable-runner")
            .with_environment("accelerator", "GB300")
            .with_environment("driver", "595.71.05");
        baseline.context = current.context.clone();

        let comparison = current
            .compare(&baseline, &ComparisonOptions::default())
            .unwrap();
        assert!(comparison.environment.exact_match);
        assert_eq!(comparison.environment.current_runner_id, "stable-runner");

        baseline
            .context
            .environment
            .insert("driver".to_string(), "595.80.01".to_string());
        assert!(matches!(
            current
                .compare(&baseline, &ComparisonOptions::default())
                .unwrap_err(),
            ComparisonError::EnvironmentMismatch { .. }
        ));
    }

    #[test]
    fn provenance_does_not_affect_compatibility_and_is_referenced() {
        let mut current = report(&[("a", 100.0)]);
        let mut baseline = report(&[("a", 100.0)]);
        current.context.provenance.insert(
            "commit".to_string(),
            "0123456789abcdef0123456789abcdef01234567".to_string(),
        );
        baseline
            .context
            .provenance
            .insert("commit".to_string(), "different".to_string());

        let comparison = current
            .compare(&baseline, &ComparisonOptions::default())
            .unwrap();
        assert_eq!(
            comparison.current.source_provenance["commit"],
            "0123456789abcdef0123456789abcdef01234567"
        );
        assert_eq!(comparison.baseline.source_provenance["commit"], "different");
    }

    #[test]
    fn environment_override_is_explicit_and_auditable() {
        let mut current = report(&[("a", 100.0)]);
        let mut baseline = report(&[("a", 100.0)]);
        current.context = ReportContext::new("runner-a").with_environment("accelerator", "GB300");
        baseline.context = ReportContext::new("runner-b").with_environment("accelerator", "B300");

        let comparison = current
            .compare(
                &baseline,
                &ComparisonOptions::default()
                    .with_environment_override("controlled cross-runner calibration"),
            )
            .unwrap();
        assert!(!comparison.environment.exact_match);
        assert_eq!(comparison.environment.current_runner_id, "runner-a");
        assert_eq!(comparison.environment.baseline_runner_id, "runner-b");
        assert_eq!(
            comparison
                .environment
                .operator_override
                .as_ref()
                .unwrap()
                .reason,
            "controlled cross-runner calibration"
        );

        assert_eq!(
            current
                .compare(
                    &baseline,
                    &ComparisonOptions::default().with_environment_override("  ")
                )
                .unwrap_err(),
            ComparisonError::InvalidEnvironmentOverride
        );
    }

    #[test]
    fn reports_without_context_remain_comparable_by_hostname() {
        let current = report(&[("a", 100.0)]);
        let baseline = report(&[("a", 100.0)]);
        let mut current_json = serde_json::to_value(current).unwrap();
        let mut baseline_json = serde_json::to_value(baseline).unwrap();
        current_json.as_object_mut().unwrap().remove("context");
        baseline_json.as_object_mut().unwrap().remove("context");
        let current: BenchmarkReport = serde_json::from_value(current_json).unwrap();
        let baseline: BenchmarkReport = serde_json::from_value(baseline_json).unwrap();

        let comparison = current
            .compare(&baseline, &ComparisonOptions::default())
            .unwrap();
        assert_eq!(comparison.environment.current_runner_id, "host-a");
        assert!(comparison.environment.exact_match);
    }

    fn series_fixture(name: &str) -> PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/series")
            .join(name)
    }

    fn assert_approximately(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("expected a finite statistic");
        assert!(
            (actual - expected).abs() < 1e-9,
            "expected {expected}, found {actual}"
        );
    }

    #[test]
    fn python_and_rust_series_fixtures_share_the_comparison_engine() {
        let current_path = series_fixture("python-current.json");
        let baseline_path = series_fixture("rust-baseline.json");
        let current_bytes = fs::read(&current_path).unwrap();
        let baseline_bytes = fs::read(&baseline_path).unwrap();
        let current = ReportDocument::load_from_path(&current_path).unwrap();
        let baseline = ReportDocument::load_from_path(&baseline_path).unwrap();

        assert!(current.as_series().is_some());
        assert!(current.as_native().is_none());
        assert_eq!(
            current.reference().document_type,
            ReportDocumentType::Series
        );
        assert_eq!(
            current.reference().content_digest,
            sha256_digest(&current_bytes)
        );
        assert_eq!(
            baseline.reference().content_digest,
            sha256_digest(&baseline_bytes)
        );
        assert_eq!(
            current.reference().source_provenance["commit"],
            "current-python-commit"
        );

        let comparison =
            compare_reports(&current, &baseline, &ComparisonOptions::default()).unwrap();
        assert_eq!(
            comparison.summary,
            ComparisonSummary {
                matched: 5,
                added: 0,
                removed: 0,
            }
        );

        let latency = comparison
            .matched
            .iter()
            .find(|case| case.identity.name() == "create")
            .unwrap();
        assert_eq!(
            latency.current.primary.measurement,
            MeasurementKind::Latency
        );
        assert_eq!(latency.current.primary.samples, vec![90.0, 100.0, 110.0]);
        assert_eq!(latency.current.primary.value, Some(100.0));
        assert_eq!(latency.current.statistics.p95, Some(109.0));
        assert_eq!(latency.current.statistics.mad, Some(10.0));
        assert_approximately(latency.current.statistics.cv_percent, 8.16496580927726);
        assert_approximately(latency.percent_improvement, 9.090909090909092);
        assert_eq!(latency.current.provenance["image_digest"], "sha256:current");
        assert_eq!(
            latency.baseline.provenance["image_digest"],
            "sha256:baseline"
        );

        let invalid = comparison
            .matched
            .iter()
            .find(|case| case.identity.name() == "quality-score")
            .unwrap();
        assert_eq!(invalid.current.validity.status, ValidityStatus::Invalid);
        assert_eq!(invalid.current.primary.value, None);
        assert_eq!(invalid.percent_improvement, None);
        assert!(
            invalid
                .stability_warnings
                .iter()
                .any(|warning| warning.contains("checksum mismatch"))
        );
    }

    #[test]
    fn series_measurement_direction_controls_improvement() {
        let current =
            ReportDocument::load_from_path(series_fixture("python-current.json")).unwrap();
        let baseline =
            ReportDocument::load_from_path(series_fixture("rust-baseline.json")).unwrap();
        let comparison =
            compare_reports(&current, &baseline, &ComparisonOptions::default()).unwrap();

        let improvement = |name: &str| {
            comparison
                .matched
                .iter()
                .find(|case| case.identity.name() == name)
                .unwrap()
                .percent_improvement
        };
        assert_approximately(improvement("requests"), 20.0);
        assert_approximately(improvement("peak-memory"), 9.523809523809524);
        assert_approximately(improvement("occupancy"), 14.084507042253536);
        assert_eq!(improvement("quality-score"), None);
    }

    #[test]
    fn series_statistics_are_derived_from_raw_samples() {
        let result = SeriesResult::new(
            "group",
            "case",
            MeasurementKind::Latency,
            "ms",
            MeasurementDirection::Lower,
            vec![10.0, 10.0, 10.0, 10.0, 100.0],
        );
        let current = SeriesReport::new(
            "current",
            "suite",
            ReportContext::new("runner"),
            vec![result.clone()],
        );
        let baseline = SeriesReport::new(
            "baseline",
            "suite",
            ReportContext::new("runner"),
            vec![result],
        );
        let comparison = current
            .compare(&baseline, &ComparisonOptions::default())
            .unwrap();
        let snapshot = &comparison.matched[0].current;

        assert_eq!(snapshot.primary.value, Some(10.0));
        assert_approximately(snapshot.statistics.p95, 82.0);
        assert_eq!(snapshot.statistics.mad, Some(0.0));
        assert_eq!(snapshot.statistics.outliers, 1);
        assert_eq!(snapshot.statistics.samples, 5);
        assert_approximately(snapshot.statistics.cv_percent, 128.57142857142858);
    }

    #[test]
    fn dimensions_affect_series_identity_but_provenance_does_not() {
        let current = SeriesReport::load_from_path(series_fixture("python-current.json")).unwrap();
        let mut baseline =
            SeriesReport::load_from_path(series_fixture("rust-baseline.json")).unwrap();
        baseline.results[0]
            .provenance
            .insert("image_digest".to_string(), "sha256:other".to_string());
        assert_eq!(
            current
                .compare(&baseline, &ComparisonOptions::default())
                .unwrap()
                .summary
                .matched,
            5
        );

        baseline.results[0]
            .dimensions
            .insert("model".to_string(), "fixture-v2".to_string());
        assert_eq!(
            current
                .compare(&baseline, &ComparisonOptions::default())
                .unwrap_err(),
            ComparisonError::ResultSetMismatch {
                added: 1,
                removed: 1,
            }
        );
    }

    #[test]
    fn measurement_unit_and_direction_participate_in_series_identity() {
        let current = SeriesReport::load_from_path(series_fixture("python-current.json")).unwrap();
        let baseline = SeriesReport::load_from_path(series_fixture("rust-baseline.json")).unwrap();

        for mutate in [
            |result: &mut SeriesResult| result.measurement = MeasurementKind::Memory,
            |result: &mut SeriesResult| result.unit = "seconds".to_string(),
            |result: &mut SeriesResult| result.direction = MeasurementDirection::Higher,
        ] {
            let mut changed = baseline.clone();
            mutate(&mut changed.results[0]);
            assert_eq!(
                current
                    .compare(&changed, &ComparisonOptions::default())
                    .unwrap_err(),
                ComparisonError::ResultSetMismatch {
                    added: 1,
                    removed: 1,
                }
            );
        }
    }

    #[test]
    fn invalid_series_report_is_retained_but_not_comparable() {
        let current = SeriesReport::load_from_path(series_fixture("python-current.json")).unwrap();
        let baseline = SeriesReport::load_from_path(series_fixture("rust-baseline.json")).unwrap();
        let current = current.with_validity(Validity::invalid("shared setup failed"));

        assert_eq!(
            current
                .compare(&baseline, &ComparisonOptions::default())
                .unwrap_err(),
            ComparisonError::InvalidReport {
                side: ComparisonSide::Current,
                reason: "shared setup failed".to_string(),
            }
        );
    }

    #[test]
    fn series_loading_distinguishes_structure_schema_and_semantics() {
        let missing_validity = br#"{
            "document_type":"micromeasure-series",
            "schema_version":1,
            "timestamp":"now",
            "suite":"suite",
            "context":{"runner_id":"runner","environment":{},"provenance":{}},
            "results":[]
        }"#;
        assert!(matches!(
            parse_series_report(missing_validity, None),
            Err(ReportError::MalformedReport { .. })
        ));

        let future = br#"{"document_type":"micromeasure-series","schema_version":999}"#;
        assert!(matches!(
            parse_series_report(future, None),
            Err(ReportError::UnsupportedSchema { found: 999, .. })
        ));

        let invalid_reason = br#"{
            "document_type":"micromeasure-series",
            "schema_version":1,
            "timestamp":"now",
            "suite":"suite",
            "validity":{"status":"invalid"},
            "context":{"runner_id":"runner","environment":{},"provenance":{}},
            "results":[{
                "group":"g","name":"n","measurement":"custom","unit":"u",
                "direction":"informational","samples":[],"validity":{"status":"valid"}
            }]
        }"#;
        assert!(matches!(
            parse_series_report(invalid_reason, None),
            Err(ReportError::InvalidReport { .. })
        ));

        let unsupported = br#"{"document_type":"other","schema_version":1}"#;
        assert!(matches!(
            parse_report_document(unsupported, None),
            Err(ReportError::UnsupportedDocumentType { .. })
        ));
    }

    #[test]
    fn duplicate_series_identities_are_explicit_errors() {
        let current = SeriesReport::load_from_path(series_fixture("python-current.json")).unwrap();
        let baseline = SeriesReport::load_from_path(series_fixture("rust-baseline.json")).unwrap();
        let mut current = current;
        current.results.push(current.results[0].clone());

        assert!(matches!(
            current
                .compare(&baseline, &ComparisonOptions::default())
                .unwrap_err(),
            ComparisonError::DuplicateIdentity {
                side: ComparisonSide::Current,
                identity: ComparisonCaseIdentity::Series(_),
            }
        ));
    }

    #[test]
    fn native_and_series_documents_use_shared_partial_matching() {
        let native = ReportDocument::from_native(report(&[("create", 100.0)])).unwrap();
        let mut series =
            SeriesReport::load_from_path(series_fixture("rust-baseline.json")).unwrap();
        series.suite = "suite-a".to_string();
        series.context = ReportContext::new("host-a");
        series.results.truncate(1);
        let series = ReportDocument::from_series(series).unwrap();

        let comparison = compare_reports(
            &native,
            &series,
            &ComparisonOptions::default().allow_partial_result_set(true),
        )
        .unwrap();
        assert_eq!(
            comparison.summary,
            ComparisonSummary {
                matched: 0,
                added: 1,
                removed: 1,
            }
        );
        assert_eq!(
            comparison.current.document_type,
            ReportDocumentType::NativeBenchmark
        );
        assert_eq!(
            comparison.baseline.document_type,
            ReportDocumentType::Series
        );
    }

    #[test]
    fn older_comparison_documents_default_new_series_fields() {
        let comparison = report(&[("a", 120.0)])
            .compare(&report(&[("a", 100.0)]), &ComparisonOptions::default())
            .unwrap();
        let mut document = serde_json::to_value(comparison).unwrap();
        let matched = document["matched"].as_array_mut().unwrap();
        for side in ["current", "baseline"] {
            let snapshot = matched[0][side].as_object_mut().unwrap();
            snapshot.remove("validity");
            snapshot.remove("provenance");
            snapshot["primary"]
                .as_object_mut()
                .unwrap()
                .remove("samples");
            snapshot["statistics"]
                .as_object_mut()
                .unwrap()
                .remove("p95");
        }

        let comparison: ComparisonReport = serde_json::from_value(document).unwrap();
        assert_eq!(comparison.matched[0].current.validity, Validity::valid());
        assert!(comparison.matched[0].current.primary.samples.is_empty());
        assert_eq!(comparison.matched[0].current.statistics.p95, None);
    }
}
