use super::{Results, safe_ratio_f64, throughput_ops_per_sec};
use crate::{Alignment, BenchmarkStats, BorderColor, TableFormatter};
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

pub(super) fn benchmark_stats_from_samples(
    summed_results: &Results,
    all_results: &[Results],
    sample_count: usize,
) -> BenchmarkStats {
    let mut results = summed_results.clone();
    results.divide(sample_count as u64);

    let ops_per_sec = safe_ratio_f64(results.iterations as f64, results.duration.as_secs_f64());
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
    let mut throughput_samples = sample_mops_per_sec(all_results);
    throughput_samples.sort_by(|a, b| a.total_cmp(b));
    let median_mops_per_sec = median(&throughput_samples);
    let mut latency_samples = sample_ns_per_op(all_results);
    latency_samples.sort_by(|a, b| a.total_cmp(b));
    let median_ns_per_op = median(&latency_samples);
    let p95_ns_per_op = percentile(&latency_samples, 0.95);
    let mad_ns_per_op = median_absolute_deviation(&latency_samples, median_ns_per_op);
    let outlier_count = tukey_outlier_count(&latency_samples);

    BenchmarkStats {
        mops_per_sec: ops_per_sec / 1_000_000.0,
        median_mops_per_sec,
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
        sample_throughput_mops_per_sec: throughput_samples,
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
    }
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

fn render_stats_table_impl(
    stats: &BenchmarkStats,
    measurement_label: &str,
    border_color: Option<BorderColor>,
    include_latency_rows: bool,
) -> Option<String> {
    let mut table =
        TableFormatter::new(vec!["Stat", "Value", "Stat", "Value"], vec![22, 18, 22, 18])
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
        &colorize_value(&format!("{:.2} Mops/s", stats.mops_per_sec)),
        &colorize_label("Median Throughput"),
        &colorize_value(&format!("{:.2} Mops/s", stats.median_mops_per_sec)),
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

    Some(format!(
        "  PMU: coverage={} avg_running={:.3}s avg_enabled={:.3}s total_running={:.3}s total_enabled={:.3}s",
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

fn sample_mops_per_sec(samples: &[Results]) -> Vec<f64> {
    samples
        .iter()
        .filter_map(throughput_ops_per_sec)
        .map(|v| v / 1_000_000.0)
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
