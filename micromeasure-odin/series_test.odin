package micromeasure

import "core:math"
import "core:mem/virtual"
import "core:os"
import "core:strings"
import "core:testing"

@(test)
test_series_preserves_observations_and_units :: proc(t: ^testing.T) {
	arena: virtual.Arena
	testing.expect(t, virtual.arena_init_growing(&arena) == nil)
	defer virtual.arena_destroy(&arena)
	context.allocator = virtual.arena_allocator(&arena)
	result := Result {
		group           = "nested/group",
		name            = "nested/group/body",
		samples         = []f64{30, 10, 20},
		chunk_size      = 8,
		throughput      = throughput_per_op(0.25, "MiB"),
		counter_samples = make([]Counter_Sample, 3),
	}
	latency := series_timing(result, false)
	testing.expect_value(t, latency.name, "body")
	testing.expect_value(t, latency.samples[0], f64(30))
	testing.expect_value(t, latency.samples[1], f64(10))
	testing.expect_value(t, latency.unit, "ns/op")
	throughput := series_timing(result, true)
	testing.expect_value(t, throughput.unit, "MiB/s")
	testing.expect_value(t, throughput.samples[1], f64(25_000_000))
	testing.expect_value(t, throughput.direction, "higher")
	for &sample, i in result.counter_samples {
		sample.valid[Counter_Kind.Cycles] = true
		sample.valid[Counter_Kind.Instructions] = true
		sample.values[Counter_Kind.Cycles] = f64(10 + i)
		sample.values[Counter_Kind.Instructions] = f64(20 + 2 * i)
		sample.coverage[Counter_Kind.Cycles] = 0.5
		sample.coverage[Counter_Kind.Instructions] = 0.5
		sample.ipc_grouped = true
	}
	cycles := series_counter(result, .Cycles, false)
	testing.expect_value(t, cycles.samples[2], f64(12))
	testing.expect_value(t, cycles.unit, "count/op")
	coverage := series_counter(result, .Cycles, true)
	testing.expect_value(t, coverage.samples[0], f64(0.5))
	ipc := series_ipc(result)
	testing.expect_value(t, ipc.samples[1], f64(2))
	// A failure midway must not become zero or a shortened valid series.
	result.counter_samples[1].valid[Counter_Kind.Instructions] = false
	instructions := series_counter(result, .Instructions, false)
	testing.expect_value(t, instructions.validity.status, "invalid")
	testing.expect_value(t, len(instructions.samples), 0)
	testing.expect(t, instructions.validity.reason != "")
	testing.expect_value(t, series_ipc(result).validity.status, "invalid")
	testing.expect_value(t, series_counter(result, .Cycles, false).validity.status, "valid")
	result.counter_samples[0].ipc_grouped = false
	testing.expect_value(t, series_ipc(result).validity.status, "invalid")
	apply_memory(&result, {100, true, .Current_RSS_Delta}, {50, true, .Current_RSS_Delta})
	memory := series_memory(result)
	testing.expect_value(t, len(memory.samples), 1)
	testing.expect_value(t, memory.samples[0], f64(-50))
	testing.expect_value(t, memory.direction, "informational")
	testing.expect_value(t, memory.provenance["before_bytes"], "100")
	result.memory_available = false
	testing.expect_value(t, series_memory(result).validity.status, "invalid")
}

@(test)
test_series_rejects_unrepresentable_samples :: proc(t: ^testing.T) {
	arena: virtual.Arena
	testing.expect(t, virtual.arena_init_growing(&arena) == nil)
	defer virtual.arena_destroy(&arena)
	context.allocator = virtual.arena_allocator(&arena)
	result := Result {
		group      = "g",
		name       = "g/b",
		throughput = throughput_ops(),
	}
	for value in ([]f64{-1, math.QNAN_F64, math.INF_F64}) {
		result.samples = []f64{10, value, 30}
		series := series_timing(result, false)
		testing.expect_value(t, series.validity.status, "invalid")
		testing.expect_value(t, len(series.samples), 0)
	}
	result.samples = []f64{0, 1}
	testing.expect_value(t, series_timing(result, false).validity.status, "valid")
	testing.expect_value(t, series_timing(result, true).validity.status, "invalid")
	result.samples = nil
	testing.expect_value(t, series_timing(result, false).validity.status, "invalid")
}

@(test)
test_series_failed_export_preserves_destination :: proc(t: ^testing.T) {
	path := temporary_report_path(t)
	if path == "" {return}
	defer delete(path)
	defer os.remove(path)
	original := "previous report"
	testing.expect(t, os.write_entire_file(path, transmute([]u8)original) == nil)
	runner: Runner
	runner_init(&runner)
	defer runner_destroy(&runner)
	metadata := Report_Context {
		suite     = "suite",
		runner_id = "runner",
	}
	// A valid report cannot be empty. Explicit correctness failure can be empty.
	testing.expect(t, !save_json_report(path, &runner, metadata))
	bytes, err := os.read_entire_file(path, context.allocator)
	defer delete(bytes)
	testing.expect(t, err == nil)
	testing.expect_value(t, string(bytes), original)
	metadata.invalid_reason = "output checksum mismatch"
	testing.expect(t, save_json_report(path, &runner, metadata))
	metadata.runner_id = " "
	testing.expect(t, !save_json_report(path, &runner, metadata))
	metadata.runner_id = "runner"
	metadata.invalid_reason = ""
	g := group(&runner, "g")
	for _ in 0 ..< 2 {
		samples := make([]f64, 2)
		copy(samples, []f64{1, 2})
		append(
			&runner.results,
			Result {
				group = g.name,
				name = strings.clone("g/b"),
				samples = samples,
				throughput = throughput_ops(),
				counter_scope = .Disabled,
			},
		)
	}
	// Duplicate comparison identities must fail instead of hiding a result.
	testing.expect(t, !save_json_report(path, &runner, metadata))
}

@(private)
series_test_body :: proc(user: rawptr, chunk, _: int) {
	value := (^u64)(user)
	for _ in 0 ..< chunk {value^ = black_box(value^) + 1}
}

@(test)
test_runner_retains_counter_samples_across_early_stop :: proc(t: ^testing.T) {
	runner: Runner
	runner_init(
		&runner,
		{
			target_sample = 1,
			min_samples = 2,
			max_samples = 4,
			noise_cv = 1e6,
			collect_counters = true,
		},
	)
	defer runner_destroy(&runner)
	value: u64
	g := group(&runner, "retained")
	bench_capped(g, "counters", &value, series_test_body, 1)
	for _ in 0 ..< 2 {
		testing.expect_value(t, runner_run(&runner), 1)
		result := runner.results[0]
		testing.expect_value(t, len(result.samples), 2)
		testing.expect_value(t, len(result.counter_samples), 2)
		for kind in Counter_Kind {
			total := f64(0)
			all_valid := true
			coverage := f64(1)
			for sample in result.counter_samples {
				total += sample.values[kind]
				all_valid = all_valid && sample.valid[kind]
				coverage = min(coverage, sample.coverage[kind])
			}
			mean, available := counter_value(result.counters, kind)
			testing.expect_value(t, available, all_valid)
			if available {
				testing.expect_value(t, mean, total / 2)
				testing.expect_value(t, result.counter_coverage[kind], coverage)
			}
		}
	}
}
