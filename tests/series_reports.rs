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

use micromeasure::{
    ComparisonOptions, MeasurementDirection, MeasurementKind, ReportContext, ReportDocument,
    ReportDocumentType, SeriesReport, SeriesResult, Validity, compare_reports,
};
use std::path::{Path, PathBuf};

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/series")
        .join(name)
}

#[test]
fn public_loader_compares_python_and_rust_series_documents() {
    let current = ReportDocument::load_from_path(fixture("python-current.json")).unwrap();
    let baseline = ReportDocument::load_from_path(fixture("rust-baseline.json")).unwrap();
    let comparison = compare_reports(&current, &baseline, &ComparisonOptions::default()).unwrap();

    assert_eq!(comparison.current.document_type, ReportDocumentType::Series);
    assert_eq!(comparison.summary.matched, 5);
    assert_eq!(comparison.summary.added, 0);
    assert_eq!(comparison.summary.removed, 0);
}

#[test]
fn public_rust_builder_produces_valid_comparable_evidence() {
    let baseline_result = SeriesResult::new(
        "lifecycle",
        "create",
        MeasurementKind::Latency,
        "ms",
        MeasurementDirection::Lower,
        vec![100.0, 110.0, 120.0],
    )
    .with_dimension("cache_state", "warm")
    .with_provenance("image_digest", "sha256:baseline");
    let current_result = SeriesResult::new(
        "lifecycle",
        "create",
        MeasurementKind::Latency,
        "ms",
        MeasurementDirection::Lower,
        vec![90.0, 100.0, 110.0],
    )
    .with_dimension("cache_state", "warm")
    .with_provenance("image_digest", "sha256:current")
    .with_validity(Validity::valid());

    let baseline = SeriesReport::new(
        "baseline",
        "suite",
        ReportContext::new("runner"),
        vec![baseline_result],
    );
    let current = SeriesReport::new(
        "current",
        "suite",
        ReportContext::new("runner"),
        vec![current_result],
    );
    current.validate().unwrap();

    let comparison = current
        .compare(&baseline, &ComparisonOptions::default())
        .unwrap();
    assert_eq!(comparison.matched[0].current.primary.value, Some(100.0));
    assert!((comparison.matched[0].percent_improvement.unwrap() - 9.090909090909092).abs() < 1e-9);
}
