use super::{Results, Throughput, safe_ratio_f64, throughput_ops_per_sec};
use crate::bench::backend::{MetricFormat, MetricValue};
use crate::session::MetricSummary;
use crate::{Alignment, BenchmarkStats, BorderColor, MeasurementDomain, TableFormatter};
use std::io::IsTerminal;

pub(super) fn colorize_label(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }
    let color = if text.contains("Throughput") {
        "32"
    } else if text.contains("Latency") || text == "P95" || text == "MAD" {
        "33"
    } else {
        "36"
    };
    format!("\x1b[{color}m{text}\x1b[0m")
}

pub(super) fn colorize_value(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }
    format!("\x1b[97m{text}\x1b[0m")
}

pub(super) fn colorize_section_heading(text: &str) -> String {
    if !std::io::stdout().is_terminal() {
        return text.to_string();
    }
    format!("\x1b[1;96m{text}\x1b[0m")
}

#[allow(clippy::too_many_arguments)]
pub(super) fn benchmark_stats_from_samples(
    summed_results: &Results,
    all_results: &[Results],
    sample_count: usize,
    throughput: &Throughput,
    measurement_domain: MeasurementDomain,
    measurement_label: &str,
    emits_cpu_diagnostics: bool,
    per_sample_metrics: &[Vec<MetricValue>],
) -> BenchmarkStats {
    let mut results = summed_results.clone();
    results.divide(sample_count as u64);

    let throughput_per_sec =
        throughput.rate_for_operations(results.iterations, results.duration.as_secs_f64());
    let ns_per_op = safe_ratio_f64(
        results.duration.as_nanos() as f64,
        results.iterations as f64,
    );
    let cycles_per_op = safe_ratio_f64(results.cycles as f64, results.iterations as f64);
    let instructions_per_op =
        safe_ratio_f64(results.instructions as f64, results.iterations as f64);
    let ipc = safe_ratio_f64(results.instructions as f64, results.cycles as f64);
    let cache_references_per_op =
        safe_ratio_f64(results.cache_references as f64, results.iterations as f64);
    let l1i_misses_per_op = safe_ratio_f64(results.l1i_misses as f64, results.iterations as f64);
    let branches_per_op = safe_ratio_f64(results.branches as f64, results.iterations as f64);
    let branch_miss_rate =
        safe_ratio_f64(results.branch_misses as f64, results.branches as f64) * 100.0;
    let branch_misses_per_op =
        safe_ratio_f64(results.branch_misses as f64, results.iterations as f64);
    let cache_misses_per_op =
        safe_ratio_f64(results.cache_misses as f64, results.iterations as f64);
    let cache_miss_percent =
        safe_ratio_f64(results.cache_misses as f64, results.cache_references as f64) * 100.0;
    let frontend_stall_cycles_per_op = safe_ratio_f64(
        results.stalled_cycles_frontend as f64,
        results.iterations as f64,
    );
    let frontend_stall_percent = safe_ratio_f64(
        results.stalled_cycles_frontend as f64,
        results.cycles as f64,
    ) * 100.0;
    let backend_stall_cycles_per_op = safe_ratio_f64(
        results.stalled_cycles_backend as f64,
        results.iterations as f64,
    );
    let backend_stall_percent =
        safe_ratio_f64(results.stalled_cycles_backend as f64, results.cycles as f64) * 100.0;
    let cv_percent = coefficient_of_variation_percent(all_results);
    let mut throughput_samples = sample_throughput_per_sec(all_results, throughput);
    throughput_samples.sort_by(|a, b| a.total_cmp(b));
    let median_throughput_per_sec = median(&throughput_samples);
    let mut latency_samples = sample_ns_per_op(all_results);
    latency_samples.sort_by(|a, b| a.total_cmp(b));
    let median_ns_per_op = median(&latency_samples);
    let p95_ns_per_op = percentile(&latency_samples, 0.95);
    let mad_ns_per_op = median_absolute_deviation(&latency_samples, median_ns_per_op);
    let outlier_count = tukey_outlier_count(&latency_samples);

    BenchmarkStats {
        throughput: throughput.clone(),
        throughput_per_sec,
        median_throughput_per_sec,
        ns_per_op,
        median_ns_per_op,
        p95_ns_per_op,
        mad_ns_per_op,
        cycles_per_op,
        instructions_per_op,
        ipc,
        cache_references_per_op,
        l1i_misses_per_op,
        branches_per_op,
        branch_miss_rate,
        branch_misses_per_op,
        cache_misses_per_op,
        cache_miss_percent,
        frontend_stall_cycles_per_op,
        frontend_stall_percent,
        backend_stall_cycles_per_op,
        backend_stall_percent,
        cv_percent,
        outlier_count,
        samples: sample_count,
        operations: results.iterations,
        total_duration_sec: summed_results.duration.as_secs_f64(),
        sample_throughput_per_sec: throughput_samples,
        sample_latency_ns_per_op: latency_samples,
        has_cycles: results.has_cycles,
        has_instructions: results.has_instructions,
        has_cache_references: results.has_cache_references,
        has_l1i_misses: results.has_l1i_misses,
        has_branches: results.has_branches,
        has_branch_misses: results.has_branch_misses,
        has_cache_misses: results.has_cache_misses,
        has_stalled_cycles_frontend: results.has_stalled_cycles_frontend,
        has_stalled_cycles_backend: results.has_stalled_cycles_backend,
        pmu_time_enabled_ns: results.pmu_time_enabled_ns,
        pmu_time_running_ns: results.pmu_time_running_ns,
        measurement_domain,
        measurement_label: measurement_label.to_string(),
        emits_cpu_diagnostics,
        metrics: aggregate_metrics(per_sample_metrics),
    }
}

/// Aggregate per-sample `Vec<MetricValue>` into one [`MetricSummary`] per
/// `(section, name, unit)` pair, computing mean / median / p95 / min / max.
///
/// Grouping by `(section, name, unit)` means a metric reported in two different
/// units (e.g. `"time_ms"` and `"time_us"`) is treated as two distinct
/// summaries, which matches how a reader would scan a metrics table.
///
/// Samples that did not report a particular metric are simply excluded
/// from that metric's value list — `samples` in the summary reflects how
/// many samples actually contributed. This handles intermittent metrics
/// (e.g. CUDA event timing that occasionally fails to record).
///
/// Within a single sample, if the same `(section, name, unit)` appears multiple
/// times only the last value is kept — the typical case is one value per
/// metric per sample.
pub(super) fn aggregate_metrics(per_sample: &[Vec<MetricValue>]) -> Vec<MetricSummary> {
    use std::collections::HashMap;

    /// Last-writer-wins accumulator: maps `(name, unit)` to the values seen
    /// across samples, plus first-insertion order index for stable output.
    /// `display_name` and `format` are captured from the first-seen
    /// `MetricValue` and preserved through aggregation.
    struct Acc {
        values: Vec<f64>,
        order: usize,
        display_name: &'static str,
        format: MetricFormat,
    }

    let mut by_key: HashMap<(&'static str, &'static str, &'static str), Acc> = HashMap::new();
    let mut next_order = 0usize;

    for metrics in per_sample {
        // Dedupe within a sample (last-writer-wins) while preserving
        // push order. A BTreeMap would silently reorder metrics
        // lexicographically, which breaks the first-seen ordering
        // contract — a bench pushing `tflops` before `cuda_event_ms`
        // would see them reordered alphabetically in the output table.
        type SampleEntry = (
            (&'static str, &'static str, &'static str),
            (f64, &'static str, MetricFormat),
        );
        let mut seen_in_sample: Vec<SampleEntry> = Vec::new();
        let mut seen_keys: std::collections::HashSet<(&'static str, &'static str, &'static str)> =
            std::collections::HashSet::new();
        for m in metrics {
            // NaN / inf are silently dropped — they would propagate through
            // percentile computation and produce nonsensical summaries. This
            // matches the existing `safe_ratio_f64` fallback convention.
            if !m.value.is_finite() {
                continue;
            }
            let key = (m.section, m.name, m.unit);
            if seen_keys.insert(key) {
                seen_in_sample.push((key, (m.value, m.display_name, m.format)));
            } else {
                // Last-writer-wins: update the value for this key.
                if let Some((_, existing)) = seen_in_sample.iter_mut().find(|(k, _)| *k == key) {
                    *existing = (m.value, m.display_name, m.format);
                }
            }
        }
        for (key, (value, display_name, format)) in seen_in_sample {
            let acc = by_key.entry(key).or_insert_with(|| {
                let order = next_order;
                next_order += 1;
                Acc {
                    values: Vec::new(),
                    order,
                    display_name,
                    format,
                }
            });
            acc.values.push(value);
        }
    }

    let mut summaries: Vec<(usize, MetricSummary)> = by_key
        .into_iter()
        .map(|((section, name, unit), acc)| {
            let mut sorted = acc.values.clone();
            sorted.sort_by(|a, b| a.total_cmp(b));
            let n = sorted.len();
            let mean = if n == 0 {
                0.0
            } else {
                sorted.iter().sum::<f64>() / n as f64
            };
            let median = median(&sorted);
            let p95 = percentile(&sorted, 0.95);
            let min = sorted.first().copied().unwrap_or(0.0);
            let max = sorted.last().copied().unwrap_or(0.0);
            (
                acc.order,
                MetricSummary {
                    name: name.to_string(),
                    unit: unit.to_string(),
                    section: section.to_string(),
                    display_name: acc.display_name.to_string(),
                    format: acc.format,
                    mean,
                    median,
                    p95,
                    min,
                    max,
                    samples: n,
                },
            )
        })
        .collect();

    // Stable order by first insertion across all samples so the output
    // table is deterministic and matches the order the benchmark author
    // declared the metrics.
    summaries.sort_by_key(|(order, _)| *order);
    summaries.into_iter().map(|(_, s)| s).collect()
}

pub(super) fn render_stats_table(
    stats: &BenchmarkStats,
    measurement_label: &str,
    border_color: Option<BorderColor>,
) -> Option<String> {
    render_stats_table_impl(stats, measurement_label, border_color, true)
}

pub(super) fn render_combined_stats_table(
    stats: &BenchmarkStats,
    measurement_label: &str,
    border_color: Option<BorderColor>,
) -> Option<String> {
    render_stats_table_impl(stats, measurement_label, border_color, false)
}

/// Render the `custom metrics:` table beneath the standard stats table.
/// No-op when `stats.metrics` is empty, so callers (both `bench` and
/// `bench_sample` paths) can invoke this unconditionally.
pub(super) fn render_custom_metrics(stats: &BenchmarkStats) {
    if stats.metrics.is_empty() {
        return;
    }

    println!("  custom metrics:");
    let show_section = stats.metrics.iter().any(|m| !m.section.is_empty());
    let mut headers = vec!["Metric", "Mean", "Median", "P95", "Min", "Max", "Unit", "N"];
    let mut widths = vec![22, 12, 12, 12, 12, 12, 10, 5];
    let mut alignments = vec![
        Alignment::Left,
        Alignment::Right,
        Alignment::Right,
        Alignment::Right,
        Alignment::Right,
        Alignment::Right,
        Alignment::Left,
        Alignment::Right,
    ];
    if show_section {
        headers.insert(0, "Section");
        widths.insert(0, 18);
        alignments.insert(0, Alignment::Left);
    }
    let mut table = TableFormatter::new(headers, widths).with_alignments(alignments);

    for m in &stats.metrics {
        let label = if m.display_name.is_empty() {
            &m.name
        } else {
            &m.display_name
        };
        let label = colorize_label(label);
        let mean = colorize_value(&format_metric_value(m.mean, m.format));
        let median = colorize_value(&format_metric_value(m.median, m.format));
        let p95 = colorize_value(&format_metric_value(m.p95, m.format));
        let min = colorize_value(&format_metric_value(m.min, m.format));
        let max = colorize_value(&format_metric_value(m.max, m.format));
        let samples = m.samples.to_string();
        let mut row = vec![
            label.as_str(),
            mean.as_str(),
            median.as_str(),
            p95.as_str(),
            min.as_str(),
            max.as_str(),
            m.unit.as_str(),
            samples.as_str(),
        ];
        if show_section {
            row.insert(0, m.section.as_str());
        }
        table.add_row(row);
    }

    table.print();
}

fn format_metric_value(value: f64, format: MetricFormat) -> String {
    if !value.is_finite() {
        return "n/a".to_string();
    }
    match format {
        MetricFormat::Integer => {
            // Round to nearest integer, no decimal places, no scientific
            // notation. Use i64 formatting so IDs and counts render as
            // `0`, `3`, `42` — never `1.500e0` or `0.006`.
            format!("{}", value.round() as i64)
        }
        MetricFormat::Number => {
            if value == 0.0 {
                return "0".to_string();
            }
            let abs = value.abs();
            if !(0.001..1000.0).contains(&abs) {
                format!("{value:.3e}")
            } else {
                format!("{value:.3}")
            }
        }
    }
}

fn render_stats_table_impl(
    stats: &BenchmarkStats,
    measurement_label: &str,
    border_color: Option<BorderColor>,
    include_latency_rows: bool,
) -> Option<String> {
    let mut table =
        TableFormatter::new(vec!["Stat", "Value", "Stat", "Value"], vec![22, 28, 22, 28])
            .with_alignments(vec![
                Alignment::Left,
                Alignment::Right,
                Alignment::Left,
                Alignment::Right,
            ])
            .with_group_split_after(1);
    if let Some(border_color) = border_color {
        table = table.with_border_color(border_color);
    }

    if include_latency_rows {
        add_full_stat_rows(&mut table, stats, measurement_label);
    } else {
        add_combined_stat_rows(&mut table, stats, measurement_label);
    }

    add_pmu_rows(&mut table, stats);
    table.print();
    pmu_byline(stats)
}

fn add_full_stat_rows(table: &mut TableFormatter, stats: &BenchmarkStats, measurement_label: &str) {
    table.add_row(vec![
        &colorize_label("Throughput"),
        &colorize_value(&stats.throughput.format_rate(stats.throughput_per_sec)),
        &colorize_label("Median Throughput"),
        &colorize_value(
            &stats
                .throughput
                .format_rate(stats.median_throughput_per_sec),
        ),
    ]);
    table.add_row(vec![
        &colorize_label("Mean Latency"),
        &colorize_value(&format!("{:.2} ns/op", stats.ns_per_op)),
        &colorize_label("Median Latency"),
        &colorize_value(&format!("{:.2} ns/op", stats.median_ns_per_op)),
    ]);
    table.add_row(vec![
        &colorize_label("P95 Latency"),
        &colorize_value(&format!("{:.2} ns/op", stats.p95_ns_per_op)),
        &colorize_label("MAD Latency"),
        &colorize_value(&format!("{:.2} ns/op", stats.mad_ns_per_op)),
    ]);
    table.add_row(vec![
        &colorize_label("Samples"),
        &colorize_value(&stats.samples.to_string()),
        &colorize_label("Outliers"),
        &colorize_value(&stats.outlier_count.to_string()),
    ]);
    table.add_row(vec![
        &colorize_label("Operations"),
        &colorize_value(&stats.operations.to_string()),
        &colorize_label("Total Duration"),
        &colorize_value(&format!("{:.3}s", stats.total_duration_sec)),
    ]);
    table.add_row(vec![
        &colorize_label("Coefficient Var."),
        &colorize_value(&format!("{:.2}%", stats.cv_percent)),
        &colorize_label("Measurement"),
        &colorize_value(measurement_label),
    ]);
}

fn add_combined_stat_rows(
    table: &mut TableFormatter,
    stats: &BenchmarkStats,
    measurement_label: &str,
) {
    table.add_row(vec![
        &colorize_label("Samples"),
        &colorize_value(&stats.samples.to_string()),
        &colorize_label("Operations"),
        &colorize_value(&stats.operations.to_string()),
    ]);
    table.add_row(vec![
        &colorize_label("Total Duration"),
        &colorize_value(&format!("{:.3}s", stats.total_duration_sec)),
        &colorize_label("Measurement"),
        &colorize_value(measurement_label),
    ]);
}

fn add_pmu_rows(table: &mut TableFormatter, stats: &BenchmarkStats) {
    if stats.has_cycles || stats.has_instructions || stats.has_branches {
        let left_label = if stats.has_cycles {
            "Cycles / op"
        } else if stats.has_instructions {
            "Instructions / op"
        } else {
            "Branches / op"
        };
        let left_value = if stats.has_cycles {
            format!("{:.1}", stats.cycles_per_op)
        } else if stats.has_instructions {
            format!("{:.1}", stats.instructions_per_op)
        } else {
            format!("{:.1}", stats.branches_per_op)
        };
        let right_label = if stats.has_cycles && stats.has_instructions {
            "IPC"
        } else if stats.has_instructions {
            "Instructions / op"
        } else if stats.has_branches {
            "Branches / op"
        } else {
            ""
        };
        let right_value = if stats.has_cycles && stats.has_instructions {
            format!("{:.3}", stats.ipc)
        } else if stats.has_instructions {
            format!("{:.1}", stats.instructions_per_op)
        } else if stats.has_branches {
            format!("{:.1}", stats.branches_per_op)
        } else {
            String::new()
        };
        table.add_row(vec![
            &colorize_label(left_label),
            &colorize_value(&left_value),
            &colorize_label(right_label),
            &colorize_value(&right_value),
        ]);
    }

    if stats.has_cycles && stats.has_branches {
        table.add_row(vec![
            &colorize_label("Branches / op"),
            &colorize_value(&format!("{:.1}", stats.branches_per_op)),
            "",
            "",
        ]);
    }

    if stats.has_branches && stats.has_branch_misses {
        table.add_row(vec![
            &colorize_label("Branch Miss Rate"),
            &colorize_value(&format!("{:.4}%", stats.branch_miss_rate)),
            &colorize_label("Branch Misses / op"),
            &colorize_value(&format!("{:.4}", stats.branch_misses_per_op)),
        ]);
    }

    if stats.has_cache_references && stats.has_cache_misses {
        table.add_row(vec![
            &colorize_label("Cache Refs / op"),
            &colorize_value(&format!("{:.4}", stats.cache_references_per_op)),
            &colorize_label("Cache Miss Rate"),
            &colorize_value(&format!("{:.2}%", stats.cache_miss_percent)),
        ]);
        table.add_row(vec![
            &colorize_label("Cache Misses / op"),
            &colorize_value(&format!("{:.4}", stats.cache_misses_per_op)),
            "",
            "",
        ]);
    } else if stats.has_cache_misses {
        table.add_row(vec![
            &colorize_label("Cache Misses / op"),
            &colorize_value(&format!("{:.4}", stats.cache_misses_per_op)),
            "",
            "",
        ]);
    }

    if stats.has_l1i_misses {
        table.add_row(vec![
            &colorize_label("L1I Misses / op"),
            &colorize_value(&format!("{:.4}", stats.l1i_misses_per_op)),
            "",
            "",
        ]);
    }

    if stats.has_cycles && stats.has_stalled_cycles_frontend {
        table.add_row(vec![
            &colorize_label("Frontend Stall / op"),
            &colorize_value(&format!("{:.4}", stats.frontend_stall_cycles_per_op)),
            &colorize_label("Frontend Stall %"),
            &colorize_value(&format!("{:.2}%", stats.frontend_stall_percent)),
        ]);
    }

    if stats.has_cycles && stats.has_stalled_cycles_backend {
        table.add_row(vec![
            &colorize_label("Backend Stall / op"),
            &colorize_value(&format!("{:.4}", stats.backend_stall_cycles_per_op)),
            &colorize_label("Backend Stall %"),
            &colorize_value(&format!("{:.2}%", stats.backend_stall_percent)),
        ]);
    }
}

fn pmu_byline(stats: &BenchmarkStats) -> Option<String> {
    let has_perf_counters = stats.has_cycles
        || stats.has_instructions
        || stats.has_cache_references
        || stats.has_l1i_misses
        || stats.has_branches
        || stats.has_branch_misses
        || stats.has_cache_misses
        || stats.has_stalled_cycles_frontend
        || stats.has_stalled_cycles_backend;
    if !has_perf_counters {
        return None;
    }

    // On a GPU benchmark the CPU PMU counters describe the host thread
    // driving CUDA, not the measured kernel. Keep the data visible for
    // launch/sync overhead analysis, but relabel it so it is not read as
    // a description of GPU behaviour.
    let label = match stats.measurement_domain {
        MeasurementDomain::Cpu => "PMU",
        MeasurementDomain::Gpu => "host PMU (orchestration)",
        MeasurementDomain::Mixed => "host PMU (mixed workload)",
    };

    Some(format!(
        "  {label}: coverage={} avg_running={:.3}s avg_enabled={:.3}s total_running={:.3}s total_enabled={:.3}s",
        colorize_value(&format!(
            "{:.1}%",
            safe_ratio_f64(
                stats.pmu_time_running_ns as f64,
                stats.pmu_time_enabled_ns as f64
            ) * 100.0
        )),
        stats.pmu_time_running_ns as f64 / 1_000_000_000.0,
        stats.pmu_time_enabled_ns as f64 / 1_000_000_000.0,
        stats.pmu_time_running_ns as f64 / 1_000_000_000.0 * stats.samples as f64,
        stats.pmu_time_enabled_ns as f64 / 1_000_000_000.0 * stats.samples as f64,
    ))
}

fn coefficient_of_variation_percent(samples: &[Results]) -> f64 {
    let throughputs: Vec<f64> = samples.iter().filter_map(throughput_ops_per_sec).collect();
    if throughputs.is_empty() {
        return 0.0;
    }

    let mean = throughputs.iter().sum::<f64>() / throughputs.len() as f64;
    if mean <= f64::EPSILON || !mean.is_finite() {
        return 0.0;
    }

    let variance = throughputs
        .iter()
        .map(|&throughput| (throughput - mean).powi(2))
        .sum::<f64>()
        / throughputs.len() as f64;

    if !variance.is_finite() || variance < 0.0 {
        return 0.0;
    }

    (variance.sqrt() / mean) * 100.0
}

fn sample_throughput_per_sec(samples: &[Results], throughput: &Throughput) -> Vec<f64> {
    samples
        .iter()
        .filter_map(throughput_ops_per_sec)
        .map(|ops_per_sec| ops_per_sec * throughput.amount_per_operation() as f64)
        .collect()
}

fn sample_ns_per_op(samples: &[Results]) -> Vec<f64> {
    samples
        .iter()
        .filter_map(|sample| {
            if sample.iterations == 0 {
                return None;
            }

            let ns = safe_ratio_f64(sample.duration.as_nanos() as f64, sample.iterations as f64);
            ns.is_finite().then_some(ns)
        })
        .collect()
}

pub(super) fn percentile(sorted_values: &[f64], percentile: f64) -> f64 {
    if sorted_values.is_empty() {
        return 0.0;
    }

    let percentile = percentile.clamp(0.0, 1.0);
    let last_index = sorted_values.len() - 1;
    let position = percentile * last_index as f64;
    let lower = position.floor() as usize;
    let upper = position.ceil() as usize;
    if lower == upper {
        return sorted_values[lower];
    }

    let weight = position - lower as f64;
    sorted_values[lower] * (1.0 - weight) + sorted_values[upper] * weight
}

pub(super) fn median(sorted_values: &[f64]) -> f64 {
    percentile(sorted_values, 0.5)
}

pub(super) fn median_absolute_deviation(values: &[f64], median_value: f64) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    let mut deviations: Vec<f64> = values
        .iter()
        .map(|value| (value - median_value).abs())
        .collect();
    deviations.sort_by(|a, b| a.total_cmp(b));
    median(&deviations)
}

pub(super) fn tukey_outlier_count(sorted_values: &[f64]) -> usize {
    if sorted_values.len() < 4 {
        return 0;
    }

    let q1 = percentile(sorted_values, 0.25);
    let q3 = percentile(sorted_values, 0.75);
    let iqr = q3 - q1;
    let lower = q1 - 1.5 * iqr;
    let upper = q3 + 1.5 * iqr;
    sorted_values
        .iter()
        .filter(|value| **value < lower || **value > upper)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bench::backend::MetricValue;

    #[test]
    fn aggregate_metrics_handles_empty_input() {
        assert!(aggregate_metrics(&[]).is_empty());
        assert!(aggregate_metrics(&[Vec::new(), Vec::new()]).is_empty());
    }

    #[test]
    fn aggregate_metrics_groups_by_name_and_unit() {
        let per_sample = vec![
            vec![
                MetricValue::new("cuda_event_ms", 1.0, "ms"),
                MetricValue::new("tflops", 10.0, "TFLOP/s"),
            ],
            vec![
                MetricValue::new("cuda_event_ms", 3.0, "ms"),
                MetricValue::new("tflops", 20.0, "TFLOP/s"),
            ],
        ];
        let summary = aggregate_metrics(&per_sample);
        assert_eq!(summary.len(), 2);
        // Stable, first-insertion order: cuda_event_ms appears first.
        assert_eq!(summary[0].name, "cuda_event_ms");
        assert_eq!(summary[0].unit, "ms");
        assert_eq!(summary[0].samples, 2);
        assert_eq!(summary[0].mean, 2.0);
        assert_eq!(summary[0].min, 1.0);
        assert_eq!(summary[0].max, 3.0);
        assert_eq!(summary[1].name, "tflops");
        assert_eq!(summary[1].unit, "TFLOP/s");
        assert_eq!(summary[1].mean, 15.0);
    }

    #[test]
    fn aggregate_metrics_treats_distinct_units_as_distinct() {
        let per_sample = vec![vec![
            MetricValue::new("time", 1.0, "ms"),
            MetricValue::new("time", 1000.0, "us"),
        ]];
        let summary = aggregate_metrics(&per_sample);
        assert_eq!(summary.len(), 2);
        assert!(summary.iter().any(|m| m.unit == "ms" && m.mean == 1.0));
        assert!(summary.iter().any(|m| m.unit == "us" && m.mean == 1000.0));
    }

    #[test]
    fn aggregate_metrics_handles_intermittent_reports() {
        // Sample 0 reports the metric; sample 1 does not; sample 2 reports
        // a different value. The summary should reflect 2 contributing
        // samples, not 3.
        let per_sample = vec![
            vec![MetricValue::new("cuda_event_ms", 1.0, "ms")],
            Vec::new(),
            vec![MetricValue::new("cuda_event_ms", 3.0, "ms")],
        ];
        let summary = aggregate_metrics(&per_sample);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].samples, 2);
        assert_eq!(summary[0].mean, 2.0);
    }

    #[test]
    fn aggregate_metrics_drops_non_finite_values() {
        let per_sample = vec![vec![
            MetricValue::new("x", 1.0, "u"),
            MetricValue::new("x", f64::NAN, "u"),
            MetricValue::new("x", f64::INFINITY, "u"),
        ]];
        // Only the finite value is kept; samples == 1.
        let summary = aggregate_metrics(&per_sample);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].samples, 1);
        assert_eq!(summary[0].mean, 1.0);
    }

    #[test]
    fn aggregate_metrics_last_writer_wins_within_sample() {
        let per_sample = vec![vec![
            MetricValue::new("x", 1.0, "u"),
            MetricValue::new("x", 5.0, "u"),
        ]];
        let summary = aggregate_metrics(&per_sample);
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].samples, 1);
        assert_eq!(summary[0].mean, 5.0);
    }

    #[test]
    fn aggregate_metrics_preserves_push_order_not_lexicographic() {
        // Push metrics in an order where push order differs from
        // lexicographic order. If aggregation used a BTreeMap (as it
        // once did), the output would be reordered to `alpha, zeta`.
        // The fix uses a Vec + HashSet for dedup, so push order wins:
        // `zeta` first, then `alpha`.
        let per_sample = vec![vec![
            MetricValue::new("zeta", 1.0, "u"),
            MetricValue::new("alpha", 2.0, "u"),
        ]];
        let summary = aggregate_metrics(&per_sample);
        assert_eq!(summary.len(), 2);
        // Push order: zeta was pushed first, so it appears first.
        assert_eq!(
            summary[0].name, "zeta",
            "expected push-order, not lexicographic"
        );
        assert_eq!(summary[1].name, "alpha");
    }
}
