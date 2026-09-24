package micromeasure

import "core:encoding/json"
import "core:fmt"
import "core:mem"
import "core:mem/virtual"
import "core:os"
import "core:strings"
import "core:testing"
import "core:time"

@(test)
test_zero_mad_outliers :: proc(t: ^testing.T) {
	stats := compute_stats([]f64{10, 10, 10, 10, 100})
	testing.expect_value(t, stats.mad, f64(0))
	testing.expect_value(t, stats.outliers, 1)
	testing.expect_value(t, compute_stats([]f64{10, 10, 10}).outliers, 0)
}

@(test)
test_counter_scaling_uses_interval_deltas :: proc(t: ^testing.T) {
	value, coverage, ok := scale_counter({100, 1000, 500}, {140, 1100, 525})
	testing.expect(t, ok)
	testing.expect_value(t, value, f64(160))
	testing.expect_value(t, coverage, f64(0.25))
	// Zero events is a real observation when the counter ran.
	value, coverage, ok = scale_counter({100, 1000, 500}, {100, 1100, 600})
	testing.expect(t, ok)
	testing.expect_value(t, value, f64(0))
	for after in ([]Counter_Read{{140, 1100, 500}, {140, 1000, 500}, {140, 1100, 700}, {90, 1100, 550}, {140, 900, 550}}) {
		value, coverage, ok = scale_counter({100, 1000, 500}, after)
		testing.expect(t, !ok)
		testing.expect_value(t, value, f64(0))
	}
}

@(private)
fake_counter_set :: proc() -> Counter_Set {
	set := Counter_Set {
		usable      = true,
		ipc_grouped = true,
	}
	set.valid[Counter_Kind.Cycles] = true
	set.valid[Counter_Kind.Instructions] = true
	set.values[Counter_Kind.Cycles] = 120
	set.values[Counter_Kind.Instructions] = 240
	set.coverage[Counter_Kind.Cycles] = 0.5
	set.coverage[Counter_Kind.Instructions] = 0.5
	return set
}

@(test)
test_counter_units_and_partial_failure :: proc(t: ^testing.T) {
	set := fake_counter_set()
	total: Counter_Total
	accumulate_counters(&total, &set, 3)
	accumulate_counters(&total, &set, 3)
	for units in ([]f64{0.25, 1, 20000}) {
		result := Result {
			throughput = throughput_per_op(units, "unit"),
		}
		apply_counters(&result, total)
		testing.expect_value(t, result.counters.cycles, f64(40))
		testing.expect_value(t, result.counters.instructions, f64(80))
		testing.expect(t, result.ipc_available)
		testing.expect_value(t, result.counter_coverage[Counter_Kind.Cycles], f64(0.5))
	}
	// A failed later read invalidates that counter across the whole result.
	set.valid[Counter_Kind.Instructions] = false
	set.values[Counter_Kind.Instructions] = 0
	accumulate_counters(&total, &set, 3)
	result: Result
	apply_counters(&result, total)
	testing.expect(t, !result.counters.has_instructions)
	testing.expect_value(t, result.counters.instructions, f64(0))
	testing.expect(t, result.counters.has_cycles && !result.ipc_available)
	testing.expect_value(t, result.counters.cycles, f64(40))
	total.ipc_grouped = false
	total.valid[Counter_Kind.Instructions] = true
	result = {}
	apply_counters(&result, total)
	testing.expect(t, !result.ipc_available)
}

@(test)
test_memory_observation_contract :: proc(t: ^testing.T) {
	before := Memory_Reading{100, true, .Current_RSS_Delta}
	result: Result
	apply_memory(&result, before, before)
	testing.expect(t, result.memory_available)
	testing.expect_value(t, result.memory, 0)
	apply_memory(&result, before, {50, true, .Current_RSS_Delta})
	testing.expect_value(t, result.memory, -50)
	result = {}
	apply_memory(&result, {100, false, .Current_RSS_Delta}, before)
	testing.expect(t, !result.memory_available)
	apply_memory(&result, before, {100, true, .Peak_RSS_Growth})
	testing.expect(t, !result.memory_available)
	apply_memory(&result, {1000, true, .Peak_RSS_Growth}, {1000, true, .Peak_RSS_Growth})
	testing.expect(t, result.memory_available)
	testing.expect_value(t, result.memory, 0)
}

@(test)
test_memory_parser_zero_missing_and_invalid :: proc(t: ^testing.T) {
	value, ok := parse_status_field_kb("Name:\tprocess\nVmRSS:\t0 kB\n", "VmRSS:")
	testing.expect(t, ok)
	testing.expect_value(t, value, 0)
	for data in ([]string{"VmHWM: 100 kB\n", "VmRSS: -1 kB\n", "VmRSS: junk kB\n", "VmRSS: 5 MB\n"}) {
		value, ok = parse_status_field_kb(data, "VmRSS:")
		testing.expect(t, !ok)
	}
	value, ok = parse_status_field_kb("VmRSS: 1234 kB\n", "VmRSS:")
	testing.expect(t, ok)
	testing.expect_value(t, value, 1234)
}

@(test)
test_calibration_bounds_and_stopping :: proc(t: ^testing.T) {
	testing.expect_value(t, next_chunk(1, 0, 20 * time.Millisecond, 16), 16)
	testing.expect_value(t, next_chunk(1, 1000000, time.Nanosecond, 0), 1)
	testing.expect_value(t, next_chunk(10, 1000, 20 * time.Microsecond, 0), 200)
	testing.expect_value(t, next_chunk(1, 0, time.Duration(max(i64)), 0), MAX_CHUNK)
	config := Config {
		min_samples = 3,
		max_samples = 5,
		noise_cv    = 0.01,
	}
	testing.expect(t, !samples_complete(config, []f64{10, 10}))
	testing.expect(t, samples_complete(config, []f64{10, 10, 10}))
	testing.expect(t, !samples_complete(config, []f64{10, 100, 10}))
	testing.expect(t, samples_complete(config, []f64{10, 100, 10, 100, 10}))
}

@(private)
Hook_State :: struct {
	prepared, ran, cleaned: int,
	active_chunk:           int,
	active_index:           int,
	bad:                    bool,
}

@(private)
prepare_test_chunk :: proc(user: rawptr, chunk, index: int) {
	state := (^Hook_State)(user)
	state.bad = state.bad || state.prepared != state.cleaned
	state.active_chunk = chunk
	state.active_index = index
	state.prepared += 1
}

@(private)
run_test_chunk :: proc(user: rawptr, chunk, index: int) {
	state := (^Hook_State)(user)
	state.bad = state.bad || state.active_chunk != chunk || state.active_index != index
	state.bad = state.bad || state.prepared != state.ran + 1
	state.ran += 1
	// Force reuse of storage which used to hold registered/result names.
	free_all(context.temp_allocator)
	_ = fmt.aprintf(
		"overwrite-temporary-storage-%d-%d",
		chunk,
		index,
		allocator = context.temp_allocator,
	)
}

@(private)
cleanup_test_chunk :: proc(user: rawptr, chunk, index: int) {
	state := (^Hook_State)(user)
	state.bad = state.bad || state.ran != state.cleaned + 1
	state.bad = state.bad || state.active_chunk != chunk || state.active_index != index
	state.cleaned += 1
}

@(private)
zero_test_probe :: proc(_: rawptr) -> Memory_Reading {
	return {100, true, .Custom_Delta}
}

@(test)
test_runner_owns_names_and_respects_hooks :: proc(t: ^testing.T) {
	storage: [4096]u8
	arena: mem.Arena
	mem.arena_init(&arena, storage[:])
	context.temp_allocator = mem.arena_allocator(&arena)
	state: Hook_State
	runner: Runner
	runner_init(
		&runner,
		Config{target_sample = time.Millisecond, min_samples = 2, max_samples = 2},
	)
	defer runner_destroy(&runner)
	g := group(
		&runner,
		strings.clone("group", context.temp_allocator),
		throughput_per_op(0.25, "MiB"),
		.Disabled,
	)
	bench_register(
		g,
		Bench {
			name = strings.clone("body", context.temp_allocator),
			user = &state,
			run = run_test_chunk,
			prepare = prepare_test_chunk,
			cleanup = cleanup_test_chunk,
			memory_probe = zero_test_probe,
			max_chunk = 2,
		},
	)
	free_all(context.temp_allocator)
	runner.filter = strings.clone("body", context.temp_allocator)
	testing.expect_value(t, runner_run(&runner), 1)
	free_all(context.temp_allocator)
	testing.expect_value(t, runner.results[0].name, "group/body")
	testing.expect_value(t, runner.results[0].group, "group")
	testing.expect_value(t, len(runner.results[0].samples), 2)
	testing.expect(t, runner.results[0].chunk_size <= 2)
	testing.expect(t, !runner.results[0].counters_available)
	testing.expect_value(t, runner.results[0].counter_scope, Counter_Scope.Disabled)
	testing.expect(t, runner.results[0].memory_available && runner.results[0].memory == 0)
	testing.expect(t, !state.bad && state.prepared == state.ran && state.ran == state.cleaned)
	testing.expect(t, state.ran >= 3) // Calibration plus two measured invocations.
	text := format_report(&runner)
	defer delete(text)
	testing.expect(t, strings.contains(text, "group/body"))
	testing.expect(t, strings.contains(text, "custom-delta"))
	testing.expect(t, strings.contains(text, "batch-p95"))
	runner.filter = ""
	testing.expect_value(t, runner_run(&runner), 1)
	testing.expect_value(t, len(runner.results), 1) // Replaces previous results.
	runner.filter = "missing"
	testing.expect_value(t, runner_run(&runner), 0)
	testing.expect_value(t, len(runner.results), 0)
}

@(private)
temporary_report_path :: proc(t: ^testing.T) -> string {
	file, err := os.create_temp_file("", "micromeasure-test-*")
	testing.expect(t, err == nil)
	if err != nil {
		return ""
	}
	name := strings.clone(os.name(file))
	os.close(file)
	return name
}

@(test)
test_baseline_and_structured_report_round_trip :: proc(t: ^testing.T) {
	path := temporary_report_path(t)
	if path == "" {
		return
	}
	defer delete(path)
	defer os.remove(path)
	runner: Runner
	runner_init(&runner)
	defer runner_destroy(&runner)
	g := group(&runner, "suite", throughput_per_op(0.25, "MiB"))
	samples := make([]f64, 3)
	copy(samples, []f64{10, 20, 30})
	append(
		&runner.results,
		Result {
			name = strings.clone("suite/body"),
			group = g.name,
			throughput = g.throughput,
			stats = compute_stats(samples),
			samples = samples,
			chunk_size = 8,
			ops_per_second = 1e9 / 20,
			memory_available = true,
			memory_kind = .Current_RSS_Delta,
		},
	)
	testing.expect(t, save_report(path, &runner))
	baseline := load_baseline(path)
	testing.expect_value(t, baseline["suite/body"], f64(20))
	baseline_destroy(&baseline)
	testing.expect(t, baseline == nil)
	provenance := Report_Context {
		suite       = "fixture",
		runner_id   = "test-runner",
		compiler    = "test-compiler",
		machine     = "test-host",
		revision    = "abc",
		build_flags = "-o:speed",
	}
	testing.expect(t, save_json_report(path, &runner, provenance))
	bytes, err := os.read_entire_file(path, context.allocator)
	testing.expect(t, err == nil)
	defer delete(bytes)
	// A growing arena, not a fixed buffer: unmarshal's peak depends on where
	// the arena lands in memory (map storage is 64-byte aligned, and map
	// seeds derive from addresses), which moved a 32 KiB stack buffer across
	// its limit on some runs.
	arena: virtual.Arena
	testing.expect(t, virtual.arena_init_growing(&arena) == nil)
	defer virtual.arena_destroy(&arena)
	doc: Report_Document
	parse_err := json.unmarshal(bytes, &doc, allocator = virtual.arena_allocator(&arena))
	testing.expectf(t, parse_err == nil, "%v", parse_err)
	if parse_err != nil {
		return
	}
	testing.expect_value(t, doc.schema_version, 1)
	testing.expect_value(t, doc.ctx.provenance["machine"], "test-host")
	testing.expect_value(t, doc.document_type, "micromeasure-series")
	testing.expect_value(t, doc.results[0].unit, "ns/op")
	testing.expect_value(t, doc.results[0].samples[2], f64(30))
	testing.expect_value(t, doc.results[1].samples[1], f64(12_500_000))
	testing.expect_value(t, doc.results[len(doc.results) - 1].measurement, "memory")
	testing.expect_value(t, doc.ctx.provenance["min_samples"], "10")
	input := "suite/body\t10\nsuite/body\t20\ninvalid\tNaN\nnegative\t-1\n"
	testing.expect(t, os.write_entire_file(path, transmute([]u8)input) == nil)
	baseline = load_baseline(path)
	defer baseline_destroy(&baseline)
	testing.expect_value(t, len(baseline), 1)
	testing.expect_value(t, baseline["suite/body"], f64(20))
}
