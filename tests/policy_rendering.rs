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
    ComparisonExitStatus, ComparisonOptions, MeasurementDirection, MeasurementKind,
    RegressionPolicy, ReportContext, SeriesReport, SeriesResult,
};

fn regression_analysis() -> micromeasure::ComparisonAnalysis {
    let context = ReportContext::new("runner-a")
        .with_environment("hardware", "test-host")
        .with_provenance("commit", "current");
    let current = SeriesReport::new(
        "2026-07-27T14:00:00Z",
        "render-suite",
        context,
        vec![
            SeriesResult::new(
                "request|path",
                "latency\np50",
                MeasurementKind::Latency,
                "ms",
                MeasurementDirection::Lower,
                vec![110.0, 115.0, 120.0],
            )
            .with_provenance("image", "sha256:current"),
        ],
    );
    let baseline = SeriesReport::new(
        "2026-07-26T14:00:00Z",
        "render-suite",
        ReportContext::new("runner-a")
            .with_environment("hardware", "test-host")
            .with_provenance("commit", "baseline"),
        vec![
            SeriesResult::new(
                "request|path",
                "latency\np50",
                MeasurementKind::Latency,
                "ms",
                MeasurementDirection::Lower,
                vec![100.0, 100.0, 100.0],
            )
            .with_provenance("image", "sha256:baseline"),
        ],
    );

    let comparison = current
        .compare(&baseline, &ComparisonOptions::default())
        .unwrap();
    comparison
        .analyze(
            &RegressionPolicy::gating()
                .with_maximum_cv_percent(Some(2.0))
                .with_maximum_outlier_fraction(None),
        )
        .unwrap()
}

#[test]
fn renderers_expose_the_same_gating_decision() {
    let analysis = regression_analysis();
    assert_eq!(
        analysis.exit_status(),
        ComparisonExitStatus::RegressionGateFailed
    );
    assert_eq!(analysis.exit_status().code(), 1);

    let terminal = analysis.render_terminal();
    assert!(terminal.contains("gate: FAIL (1 blocking regressions)"));
    assert!(terminal.contains("REGRESSION"));

    assert_eq!(
        analysis.render_markdown().trim_end(),
        include_str!("fixtures/rendering/comparison.md").trim_end()
    );
    assert_eq!(
        analysis.render_json_pretty().unwrap().trim_end(),
        include_str!("fixtures/rendering/comparison-analysis.json").trim_end()
    );
    let restored: micromeasure::ComparisonAnalysis =
        serde_json::from_str(&analysis.render_json_pretty().unwrap()).unwrap();
    assert_eq!(restored, analysis);
}

#[test]
fn renderers_include_partial_result_sets() {
    let context = ReportContext::new("runner-a").with_environment("hardware", "test-host");
    let result = |name: &str| {
        SeriesResult::new(
            "group",
            name,
            MeasurementKind::Throughput,
            "items/s",
            MeasurementDirection::Higher,
            vec![10.0, 11.0, 12.0],
        )
    };
    let current = SeriesReport::new(
        "current",
        "partial-suite",
        context.clone(),
        vec![result("shared"), result("added")],
    );
    let baseline = SeriesReport::new(
        "baseline",
        "partial-suite",
        context,
        vec![result("shared"), result("removed")],
    );
    let comparison = current
        .compare(
            &baseline,
            &ComparisonOptions::default().allow_partial_result_set(true),
        )
        .unwrap();
    let analysis = comparison.analyze(&RegressionPolicy::default()).unwrap();

    let terminal = analysis.render_terminal();
    assert!(terminal.contains("\nadded:\n  group/added"));
    assert!(terminal.contains("\nremoved:\n  group/removed"));

    let markdown = analysis.render_markdown();
    assert!(markdown.contains("\n### Added cases\n\n- group/added"));
    assert!(markdown.contains("\n### Removed cases\n\n- group/removed"));
}
