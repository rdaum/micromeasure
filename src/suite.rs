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

//! Multi-suite report loading, comparison, and combined rendering.

use crate::{
    ComparisonAnalysis, ComparisonError, ComparisonExitStatus, ComparisonOptions, PolicyError,
    RegressionPolicy, ReportDocument, ReportDocumentType, ReportError, ReportReference,
    compare_reports,
};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    error::Error,
    fmt, fs, io,
    path::{Path, PathBuf},
};

/// JSON schema emitted for a combined suite comparison.
pub const SUITE_ANALYSIS_SCHEMA_VERSION: u32 = 1;

/// Role of an input while loading and validating a report set.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ReportInputRole {
    Current,
    Baseline,
    Validation,
}

impl fmt::Display for ReportInputRole {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Current => "current",
            Self::Baseline => "baseline",
            Self::Validation => "validation",
        })
    }
}

/// Aggregate counts across matched, added, and removed suites.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SuiteComparisonSummary {
    pub matched_suites: usize,
    pub added_suites: usize,
    pub removed_suites: usize,
    pub matched_cases: usize,
    pub added_cases: usize,
    pub removed_cases: usize,
    pub improvements: usize,
    pub regressions: usize,
    pub no_material_change: usize,
    pub informational: usize,
    pub invalid: usize,
    pub inconclusive: usize,
    pub unstable: usize,
    pub blocking: usize,
}

/// Versioned combined result for one file pair or two report directories.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct SuiteComparisonAnalysis {
    pub schema_version: u32,
    pub policy: RegressionPolicy,
    pub comparisons: Vec<ComparisonAnalysis>,
    pub added_suites: Vec<ReportReference>,
    pub removed_suites: Vec<ReportReference>,
    pub summary: SuiteComparisonSummary,
}

impl SuiteComparisonAnalysis {
    pub fn exit_status(&self) -> ComparisonExitStatus {
        if self.summary.blocking > 0 {
            ComparisonExitStatus::RegressionGateFailed
        } else {
            ComparisonExitStatus::Success
        }
    }

    pub fn render_terminal(&self) -> String {
        let mut output = String::new();
        output.push_str("micromeasure suite comparison\n");
        output.push_str(&format!(
            "suites: {} matched, {} added, {} removed\n",
            self.summary.matched_suites, self.summary.added_suites, self.summary.removed_suites
        ));
        output.push_str(&format!(
            "cases: {} matched, {} added, {} removed\n",
            self.summary.matched_cases, self.summary.added_cases, self.summary.removed_cases
        ));
        output.push_str(&format!(
            "policy: {}\n",
            suite_policy_description(&self.policy)
        ));
        output.push_str(&format!(
            "classification: {} improvements, {} regressions, {} unchanged, {} informational, {} invalid, {} inconclusive, {} unstable\n",
            self.summary.improvements,
            self.summary.regressions,
            self.summary.no_material_change,
            self.summary.informational,
            self.summary.invalid,
            self.summary.inconclusive,
            self.summary.unstable,
        ));
        output.push_str(&format!("gate: {}\n", suite_gate_description(self)));
        render_suite_references_terminal(&mut output, "added suites", &self.added_suites);
        render_suite_references_terminal(&mut output, "removed suites", &self.removed_suites);
        for comparison in &self.comparisons {
            output.push('\n');
            output.push_str(&comparison.render_terminal());
        }
        output
    }

    pub fn render_markdown(&self) -> String {
        let mut output = String::new();
        output.push_str("# Benchmark suite comparison\n\n");
        output.push_str(&format!(
            "- Policy: {}\n",
            escape_markdown(&suite_policy_description(&self.policy))
        ));
        output.push_str(&format!(
            "- Gate: **{}**\n\n",
            escape_markdown(&suite_gate_description(self))
        ));
        output.push_str("| Outcome | Count |\n|---|---:|\n");
        output.push_str(&format!(
            "| Matched suites | {} |\n",
            self.summary.matched_suites
        ));
        output.push_str(&format!(
            "| Added suites | {} |\n",
            self.summary.added_suites
        ));
        output.push_str(&format!(
            "| Removed suites | {} |\n",
            self.summary.removed_suites
        ));
        output.push_str(&format!(
            "| Matched cases | {} |\n",
            self.summary.matched_cases
        ));
        output.push_str(&format!("| Added cases | {} |\n", self.summary.added_cases));
        output.push_str(&format!(
            "| Removed cases | {} |\n",
            self.summary.removed_cases
        ));
        output.push_str(&format!(
            "| Improvements | {} |\n",
            self.summary.improvements
        ));
        output.push_str(&format!("| Regressions | {} |\n", self.summary.regressions));
        output.push_str(&format!(
            "| No material change | {} |\n",
            self.summary.no_material_change
        ));
        output.push_str(&format!(
            "| Informational | {} |\n",
            self.summary.informational
        ));
        output.push_str(&format!("| Invalid | {} |\n", self.summary.invalid));
        output.push_str(&format!(
            "| Inconclusive | {} |\n",
            self.summary.inconclusive
        ));
        output.push_str(&format!("| Unstable | {} |\n", self.summary.unstable));
        output.push_str(&format!("| Blocking | {} |\n", self.summary.blocking));
        render_suite_references_markdown(&mut output, "Added suites", &self.added_suites);
        render_suite_references_markdown(&mut output, "Removed suites", &self.removed_suites);
        for comparison in &self.comparisons {
            output.push('\n');
            output.push_str(&comparison.render_markdown());
        }
        output
    }

    pub fn render_json_pretty(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }
}

/// Validate one report file or every JSON report beneath a directory.
///
/// Directory traversal is recursive and deterministic. Non-JSON files and
/// symbolic links are ignored.
pub fn validate_report_input(
    path: impl AsRef<Path>,
) -> Result<Vec<ReportReference>, SuiteComparisonError> {
    let loaded = load_report_input(path.as_ref(), ReportInputRole::Validation)?;
    Ok(loaded
        .documents
        .into_values()
        .map(|loaded| loaded.document.reference().clone())
        .collect())
}

/// Compare one report file pair or two directories of reports.
///
/// Directory inputs are matched by suite. Suites present on only one side are
/// retained as added or removed. Every input report is validated before any
/// suite is compared.
pub fn compare_report_inputs(
    current: impl AsRef<Path>,
    baseline: impl AsRef<Path>,
    options: &ComparisonOptions,
    policy: &RegressionPolicy,
) -> Result<SuiteComparisonAnalysis, SuiteComparisonError> {
    if options
        .environment_override
        .as_ref()
        .is_some_and(|environment_override| environment_override.reason.trim().is_empty())
    {
        return Err(SuiteComparisonError::Comparison {
            suite: None,
            source: Box::new(ComparisonError::InvalidEnvironmentOverride),
        });
    }
    policy
        .validate()
        .map_err(|source| SuiteComparisonError::Policy {
            suite: None,
            source,
        })?;
    let current = load_report_input(current.as_ref(), ReportInputRole::Current)?;
    let baseline = load_report_input(baseline.as_ref(), ReportInputRole::Baseline)?;

    if current.kind == ReportInputKind::File && baseline.kind == ReportInputKind::File {
        let current_suite = current.documents.keys().next().expect("non-empty input");
        let baseline_suite = baseline.documents.keys().next().expect("non-empty input");
        if current_suite != baseline_suite {
            return Err(SuiteComparisonError::Comparison {
                suite: None,
                source: Box::new(ComparisonError::SuiteMismatch {
                    current: current_suite.clone(),
                    baseline: baseline_suite.clone(),
                }),
            });
        }
    }

    let mut comparisons = Vec::new();
    let mut added_suites = Vec::new();
    let mut removed_suites = Vec::new();

    for (suite, current_report) in &current.documents {
        let Some(baseline_report) = baseline.documents.get(suite) else {
            added_suites.push(current_report.document.reference().clone());
            continue;
        };
        let current_type = current_report.document.reference().document_type;
        let baseline_type = baseline_report.document.reference().document_type;
        if current_type != baseline_type {
            return Err(SuiteComparisonError::DocumentTypeMismatch {
                suite: suite.clone(),
                current: current_type,
                baseline: baseline_type,
            });
        }
        let comparison =
            compare_reports(&current_report.document, &baseline_report.document, options).map_err(
                |source| SuiteComparisonError::Comparison {
                    suite: Some(suite.clone()),
                    source: Box::new(source),
                },
            )?;
        let analysis =
            comparison
                .analyze(policy)
                .map_err(|source| SuiteComparisonError::Policy {
                    suite: Some(suite.clone()),
                    source,
                })?;
        comparisons.push(analysis);
    }

    for (suite, baseline_report) in &baseline.documents {
        if !current.documents.contains_key(suite) {
            removed_suites.push(baseline_report.document.reference().clone());
        }
    }

    let summary = summarize(&comparisons, &added_suites, &removed_suites);
    Ok(SuiteComparisonAnalysis {
        schema_version: SUITE_ANALYSIS_SCHEMA_VERSION,
        policy: policy.clone(),
        comparisons,
        added_suites,
        removed_suites,
        summary,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ReportInputKind {
    File,
    Directory,
}

struct LoadedReport {
    path: PathBuf,
    document: ReportDocument,
}

struct LoadedReportInput {
    kind: ReportInputKind,
    documents: BTreeMap<String, LoadedReport>,
}

fn load_report_input(
    path: &Path,
    role: ReportInputRole,
) -> Result<LoadedReportInput, SuiteComparisonError> {
    let metadata = fs::metadata(path).map_err(|source| SuiteComparisonError::InputIo {
        path: path.to_path_buf(),
        source,
    })?;
    let (kind, paths) = if metadata.is_file() {
        (ReportInputKind::File, vec![path.to_path_buf()])
    } else if metadata.is_dir() {
        let mut paths = Vec::new();
        collect_json_paths(path, &mut paths)?;
        paths.sort();
        (ReportInputKind::Directory, paths)
    } else {
        return Err(SuiteComparisonError::UnsupportedInput {
            path: path.to_path_buf(),
        });
    };
    if paths.is_empty() {
        return Err(SuiteComparisonError::NoReports {
            path: path.to_path_buf(),
        });
    }

    let mut loaded = Vec::with_capacity(paths.len());
    for report_path in paths {
        let document =
            ReportDocument::load_from_path(&report_path).map_err(SuiteComparisonError::Report)?;
        document.validate_for_comparison().map_err(|source| {
            SuiteComparisonError::InvalidDocument {
                path: report_path.clone(),
                source: Box::new(source),
            }
        })?;
        let suite = document
            .reference()
            .suite
            .as_ref()
            .expect("comparison validation requires a suite")
            .clone();
        loaded.push((suite, report_path, document));
    }
    loaded.sort_by(|left, right| left.0.cmp(&right.0).then_with(|| left.1.cmp(&right.1)));

    let mut documents: BTreeMap<String, LoadedReport> = BTreeMap::new();
    for (suite, report_path, document) in loaded {
        if let Some(previous) = documents.get(&suite) {
            return Err(SuiteComparisonError::DuplicateSuite {
                role,
                suite,
                first: previous.path.clone(),
                second: report_path,
            });
        }
        documents.insert(
            suite,
            LoadedReport {
                path: report_path,
                document,
            },
        );
    }
    Ok(LoadedReportInput { kind, documents })
}

fn collect_json_paths(
    directory: &Path,
    paths: &mut Vec<PathBuf>,
) -> Result<(), SuiteComparisonError> {
    let entries = fs::read_dir(directory).map_err(|source| SuiteComparisonError::InputIo {
        path: directory.to_path_buf(),
        source,
    })?;
    let mut entries =
        entries
            .collect::<Result<Vec<_>, _>>()
            .map_err(|source| SuiteComparisonError::InputIo {
                path: directory.to_path_buf(),
                source,
            })?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry
            .file_type()
            .map_err(|source| SuiteComparisonError::InputIo {
                path: entry.path(),
                source,
            })?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_json_paths(&entry.path(), paths)?;
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension.eq_ignore_ascii_case("json"))
        {
            paths.push(entry.path());
        }
    }
    Ok(())
}

fn summarize(
    comparisons: &[ComparisonAnalysis],
    added_suites: &[ReportReference],
    removed_suites: &[ReportReference],
) -> SuiteComparisonSummary {
    let mut summary = SuiteComparisonSummary {
        matched_suites: comparisons.len(),
        added_suites: added_suites.len(),
        removed_suites: removed_suites.len(),
        ..SuiteComparisonSummary::default()
    };
    for analysis in comparisons {
        summary.matched_cases += analysis.comparison.summary.matched;
        summary.added_cases += analysis.comparison.summary.added;
        summary.removed_cases += analysis.comparison.summary.removed;
        summary.improvements += analysis.policy.summary.improvements;
        summary.regressions += analysis.policy.summary.regressions;
        summary.no_material_change += analysis.policy.summary.no_material_change;
        summary.informational += analysis.policy.summary.informational;
        summary.invalid += analysis.policy.summary.invalid;
        summary.inconclusive += analysis.policy.summary.inconclusive;
        summary.unstable += analysis.policy.summary.unstable;
        summary.blocking += analysis.policy.summary.blocking;
    }
    summary
}

fn suite_gate_description(analysis: &SuiteComparisonAnalysis) -> String {
    if analysis.summary.blocking > 0 {
        format!("FAIL ({} blocking regressions)", analysis.summary.blocking)
    } else if analysis.policy.fail_on_regression {
        "PASS (gating enabled)".to_string()
    } else {
        "PASS (advisory only)".to_string()
    }
}

fn suite_policy_description(policy: &RegressionPolicy) -> String {
    let mode = if policy.fail_on_regression {
        "gating"
    } else {
        "advisory"
    };
    let cv = policy
        .maximum_cv_percent
        .map(|maximum| format!("{maximum:.2}%"))
        .unwrap_or_else(|| "disabled".to_string());
    let outliers = policy
        .maximum_outlier_fraction
        .map(|maximum| format!("{:.2}%", maximum * 100.0))
        .unwrap_or_else(|| "disabled".to_string());
    format!(
        "{mode}; material change > {:.2}%; max CV {cv}; max outliers {outliers}",
        policy.minimum_change_percent
    )
}

fn render_suite_references_terminal(
    output: &mut String,
    heading: &str,
    references: &[ReportReference],
) {
    if references.is_empty() {
        return;
    }
    output.push_str(&format!("\n{heading}:\n"));
    for reference in references {
        output.push_str(&format!(
            "  {} [{} {}]\n",
            inline_text(reference.suite.as_deref().unwrap_or("<missing>")),
            document_type_name(reference.document_type),
            reference.content_digest
        ));
    }
}

fn render_suite_references_markdown(
    output: &mut String,
    heading: &str,
    references: &[ReportReference],
) {
    if references.is_empty() {
        return;
    }
    output.push_str(&format!("\n## {heading}\n\n"));
    for reference in references {
        output.push_str(&format!(
            "- {} — {} `{}`\n",
            escape_markdown(reference.suite.as_deref().unwrap_or("<missing>")),
            document_type_name(reference.document_type),
            reference.content_digest
        ));
    }
}

fn document_type_name(document_type: ReportDocumentType) -> &'static str {
    match document_type {
        ReportDocumentType::NativeBenchmark => "native_benchmark",
        ReportDocumentType::Series => "series",
    }
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

/// Failure while loading, validating, or comparing report inputs.
#[derive(Debug)]
#[non_exhaustive]
pub enum SuiteComparisonError {
    InputIo {
        path: PathBuf,
        source: io::Error,
    },
    UnsupportedInput {
        path: PathBuf,
    },
    NoReports {
        path: PathBuf,
    },
    Report(ReportError),
    InvalidDocument {
        path: PathBuf,
        source: Box<ComparisonError>,
    },
    DuplicateSuite {
        role: ReportInputRole,
        suite: String,
        first: PathBuf,
        second: PathBuf,
    },
    DocumentTypeMismatch {
        suite: String,
        current: ReportDocumentType,
        baseline: ReportDocumentType,
    },
    Comparison {
        suite: Option<String>,
        source: Box<ComparisonError>,
    },
    Policy {
        suite: Option<String>,
        source: PolicyError,
    },
}

impl fmt::Display for SuiteComparisonError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InputIo { path, source } => {
                write!(formatter, "failed to inspect {}: {source}", path.display())
            }
            Self::UnsupportedInput { path } => {
                write!(
                    formatter,
                    "{} is not a report file or directory",
                    path.display()
                )
            }
            Self::NoReports { path } => {
                write!(formatter, "no JSON reports found under {}", path.display())
            }
            Self::Report(source) => source.fmt(formatter),
            Self::InvalidDocument { path, source } => {
                write!(
                    formatter,
                    "report {} is not comparable: {source}",
                    path.display()
                )
            }
            Self::DuplicateSuite {
                role,
                suite,
                first,
                second,
            } => write!(
                formatter,
                "{role} input contains duplicate suite {suite:?}: {} and {}",
                first.display(),
                second.display()
            ),
            Self::DocumentTypeMismatch {
                suite,
                current,
                baseline,
            } => write!(
                formatter,
                "suite {suite:?} changes document type from {baseline:?} to {current:?}"
            ),
            Self::Comparison { suite, source } => {
                if let Some(suite) = suite {
                    write!(formatter, "failed to compare suite {suite:?}: {source}")
                } else {
                    source.fmt(formatter)
                }
            }
            Self::Policy { suite, source } => {
                if let Some(suite) = suite {
                    write!(formatter, "failed to evaluate suite {suite:?}: {source}")
                } else {
                    source.fmt(formatter)
                }
            }
        }
    }
}

impl Error for SuiteComparisonError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InputIo { source, .. } => Some(source),
            Self::Report(source) => Some(source),
            Self::InvalidDocument { source, .. } | Self::Comparison { source, .. } => {
                Some(source.as_ref())
            }
            Self::Policy { source, .. } => Some(source),
            Self::UnsupportedInput { .. }
            | Self::NoReports { .. }
            | Self::DuplicateSuite { .. }
            | Self::DocumentTypeMismatch { .. } => None,
        }
    }
}
