// Tests for the microbenchmark harness. The statistics and the counter
// bookkeeping are deterministic. Additional contract tests cover the runner,
// memory observations, lifetime rules, and report persistence.
package micromeasure

import "core:testing"

@(test)
test_stats_median_and_percentile :: proc(t: ^testing.T) {
	samples := []f64{5, 1, 3, 2, 4}
	stats := compute_stats(samples)
	testing.expect_value(t, stats.median, f64(3))
	testing.expect_value(t, stats.min, f64(1))
	testing.expect_value(t, stats.max, f64(5))
	testing.expect_value(t, stats.mean, f64(3))
}

@(test)
test_stats_single_sample_has_zero_stddev :: proc(t: ^testing.T) {
	stats := compute_stats([]f64{7})
	testing.expect_value(t, stats.median, f64(7))
	testing.expect_value(t, stats.stddev, f64(0))
	testing.expect_value(t, stats.cv, f64(0))
}

@(test)
test_stats_empty_is_zero :: proc(t: ^testing.T) {
	stats := compute_stats(nil)
	testing.expect_value(t, stats.median, f64(0))
	testing.expect_value(t, stats.outliers, 0)
}

@(test)
test_percentile_nearest_rank :: proc(t: ^testing.T) {
	sorted := []f64{1, 2, 3, 4, 5, 6, 7, 8, 9, 10}
	testing.expect_value(t, percentile_sorted(sorted, 0.5), f64(5))
	testing.expect_value(t, percentile_sorted(sorted, 0.95), f64(10))
	testing.expect_value(t, percentile_sorted(sorted, 0.0), f64(1))
}

@(test)
test_coefficient_of_variation :: proc(t: ^testing.T) {
	// All-equal samples have no variation.
	testing.expect_value(t, coefficient_of_variation([]f64{4, 4, 4}), f64(0))
	// A single sample cannot estimate variation, so it reports 1.
	testing.expect_value(t, coefficient_of_variation([]f64{4}), f64(1))
}

@(test)
test_counter_value_maps_kinds :: proc(t: ^testing.T) {
	counters := Counters {
		cycles           = 9.5,
		instructions     = 58.0,
		has_cycles       = true,
		has_instructions = false,
	}
	cycles, has_cycles := counter_value(counters, .Cycles)
	testing.expect_value(t, cycles, f64(9.5))
	testing.expect(t, has_cycles)
	instructions, has_instructions := counter_value(counters, .Instructions)
	testing.expect_value(t, instructions, f64(58.0))
	testing.expect(t, !has_instructions)
}

@(test)
test_counters_open_reports_consistently :: proc(t: ^testing.T) {
	// Whether the kernel grants perf access is environment-dependent, so this
	// only checks that open/begin/end/close are well behaved either way.
	set := counters_open()
	defer counters_close(&set)
	counters_begin(&set)
	counters_end(&set)
	// Unavailable counters must report zero, never a stale value.
	if !set.usable {
		for kind in Counter_Kind {
			testing.expect_value(t, set.values[kind], f64(0))
		}
	}
}

@(test)
test_read_status_field_kb_parses_vmhwm :: proc(t: ^testing.T) {
	// The value is environment-dependent (the kernel must expose /proc/self/
	// status), so this only checks the parse path: a non-negative integer, and
	// zero when the file is unreadable.
	hwm, _ := read_status_field_kb("VmHWM:")
	testing.expect(t, hwm >= 0)
	rss, _ := read_status_field_kb("VmRSS:")
	testing.expect(t, rss >= 0)
	// A missing field must report zero, not a parse error.
	value, ok := read_status_field_kb("NoSuchField:")
	testing.expect_value(t, value, 0)
	testing.expect(t, !ok)
}

@(test)
test_peak_rss_bytes_is_non_negative :: proc(t: ^testing.T) {
	// VmHWM is in kilobytes; the byte value is the field times 1024.
	testing.expect(t, peak_rss_bytes().bytes >= 0)
}
