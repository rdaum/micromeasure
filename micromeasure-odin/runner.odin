// Benchmark registration and measurement.
package micromeasure

import "core:fmt"
import "core:math"
import "core:mem"
import "core:strings"
import "core:time"

MAX_CHUNK :: 1 << 30

// A session owns registration strings and results. Benchmark user state is borrowed.
Runner :: struct {
	config:    Config,
	groups:    [dynamic]^Group,
	filter:    string,
	results:   [dynamic]Result,
	allocator: mem.Allocator,
}

runner_init :: proc(runner: ^Runner, config := DEFAULT_CONFIG, allocator := context.allocator) {
	assert(config.warmup >= 0 && config.target_sample > 0, "invalid benchmark duration")
	assert(
		config.min_samples >= 2 && config.max_samples >= config.min_samples,
		"invalid sample limits",
	)
	assert(
		config.noise_cv >= 0 && !math.is_nan(config.noise_cv) && !math.is_inf(config.noise_cv),
		"invalid noise threshold",
	)
	runner^ = Runner {
		config    = config,
		allocator = allocator,
	}
	runner.groups = make([dynamic]^Group, allocator)
	runner.results = make([dynamic]Result, allocator)
}

// Clears measurements but preserves registration. runner_run calls this automatically.
runner_clear_results :: proc(runner: ^Runner) {
	for result in runner.results {
		delete(result.name, runner.allocator)
		delete(result.samples, runner.allocator)
		delete(result.counter_samples, runner.allocator)
	}
	clear(&runner.results)
}

runner_destroy :: proc(runner: ^Runner) {
	runner_clear_results(runner)
	delete(runner.results)
	for group in runner.groups {
		for bench in group.benches {
			delete(bench.name, runner.allocator)
		}
		delete(group.benches)
		delete(group.name, runner.allocator)
		delete(group.throughput.unit, runner.allocator)
		free(group, runner.allocator)
	}
	delete(runner.groups)
	runner^ = {}
}

group :: proc(
	runner: ^Runner,
	name: string,
	throughput := Throughput{1, "op"},
	counter_scope := Counter_Scope.Calling_Thread,
) -> ^Group {
	assert(
		throughput.units_per_op > 0 &&
		!math.is_inf(throughput.units_per_op) &&
		!math.is_nan(throughput.units_per_op),
		"invalid throughput",
	)
	created := new(Group, runner.allocator)
	created.name = strings.clone(name, runner.allocator)
	created.throughput = throughput
	created.throughput.unit = strings.clone(throughput.unit, runner.allocator)
	created.counter_scope = counter_scope
	created.allocator = runner.allocator
	created.benches = make([dynamic]Bench, runner.allocator)
	append(&runner.groups, created)
	return created
}

// Registers a body and optional untimed per-invocation hooks. Copies the name.
bench_register :: proc(group: ^Group, specification: Bench) {
	assert(specification.run != nil && specification.max_chunk >= 0, "invalid benchmark")
	entry := specification
	entry.name = strings.clone(entry.name, group.allocator)
	append(&group.benches, entry)
}

bench :: proc(group: ^Group, name: string, user: rawptr, run: Bench_Proc) {
	bench_register(group, Bench{name = name, user = user, run = run})
}

bench_capped :: proc(group: ^Group, name: string, user: rawptr, run: Bench_Proc, max_chunk: int) {
	bench_register(group, Bench{name = name, user = user, run = run, max_chunk = max_chunk})
}

// Observes memory before warmup and after the last sample's cleanup.
// The signed delta includes warmup, calibration, hooks, and harness activity.
bench_with_memory :: proc(
	group: ^Group,
	name: string,
	user: rawptr,
	run: Bench_Proc,
	max_chunk: int,
	memory_probe: Memory_Probe,
) {
	bench_register(
		group,
		Bench {
			name = name,
			user = user,
			run = run,
			max_chunk = max_chunk,
			memory_probe = memory_probe,
		},
	)
}

// Replaces prior results. The filter is borrowed for this call only.
runner_run :: proc(runner: ^Runner) -> int {
	runner_clear_results(runner)
	// Own the filter too: a callback can clear the caller's temporary storage.
	filter := strings.clone(runner.filter, runner.allocator)
	defer delete(filter, runner.allocator)
	for group in runner.groups {
		for bench in group.benches {
			full_name := bench_full_name(group.name, bench.name, runner.allocator)
			if filter != "" && !strings.contains(full_name, filter) {
				delete(full_name, runner.allocator)
				continue
			}
			fmt.eprintf("benchmark: %s\n", full_name)
			result := measure_bench(runner, group^, bench, full_name)
			append(&runner.results, result)
		}
	}
	return len(runner.results)
}

@(private)
bench_full_name :: proc(group, name: string, allocator := context.allocator) -> string {
	builder: strings.Builder
	strings.builder_init(&builder, allocator)
	strings.write_string(&builder, group)
	strings.write_string(&builder, "/")
	strings.write_string(&builder, name)
	return strings.to_string(builder)
}

@(private)
sample_ns :: proc(bench: Bench, chunk: int, chunk_num: int, counters: ^Counter_Set = nil) -> i64 {
	if bench.prepare != nil {
		bench.prepare(bench.user, chunk, chunk_num)
	}
	if counters != nil {
		counters_begin(counters)
	}
	start := time.tick_now()
	bench.run(bench.user, chunk, chunk_num)
	end := time.tick_now()
	if counters != nil {
		counters_end(counters)
	}
	if bench.cleanup != nil {
		bench.cleanup(bench.user, chunk, chunk_num)
	}
	return time.duration_nanoseconds(time.tick_diff(start, end))
}

@(private)
next_chunk :: proc(chunk: int, elapsed: i64, target: time.Duration, cap: int) -> int {
	per_op := max(f64(elapsed) / f64(chunk), 0.05)
	limit := min(MAX_CHUNK, cap) if cap > 0 else MAX_CHUNK
	// Clamp before conversion to avoid integer overflow for long targets.
	return int(clamp(f64(time.duration_nanoseconds(target)) / per_op, 1, f64(limit)))
}

@(private)
calibrate_chunk :: proc(runner: ^Runner, bench: Bench) -> int {
	chunk := 1
	for _ in 0 ..< 12 {
		elapsed := sample_ns(bench, chunk, 0)
		desired := next_chunk(chunk, elapsed, runner.config.target_sample, bench.max_chunk)
		if elapsed >= time.duration_nanoseconds(runner.config.target_sample) || desired == chunk {
			return desired
		}
		chunk = desired
		if bench.max_chunk > 0 && chunk >= bench.max_chunk {
			break
		}
	}
	return chunk
}

@(private)
samples_complete :: proc(config: Config, samples: []f64) -> bool {
	if len(samples) >= config.max_samples {
		return true
	}
	if len(samples) < config.min_samples {
		return false
	}
	return coefficient_of_variation(samples) <= config.noise_cv
}

@(private)
apply_memory :: proc(result: ^Result, before, after: Memory_Reading) {
	result.memory_available = false
	result.memory = 0
	result.memory_before = before
	result.memory_after = after
	result.memory_kind = before.kind
	if !before.available ||
	   !after.available ||
	   before.kind != after.kind ||
	   before.bytes < 0 ||
	   after.bytes < 0 {return}
	delta := after.bytes - before.bytes
	if before.kind == .Peak_RSS_Growth && delta < 0 {
		return
	}
	result.memory = delta
	result.memory_available = true
}

@(private)
measure_bench :: proc(runner: ^Runner, group: Group, bench: Bench, full_name: string) -> Result {
	before: Memory_Reading
	if bench.memory_probe != nil {
		before = bench.memory_probe(bench.user)
	}

	warmup_start := time.tick_now()
	for time.tick_diff(warmup_start, time.tick_now()) < runner.config.warmup {
		_ = sample_ns(bench, 1, 0)
	}
	chunk := calibrate_chunk(runner, bench)
	scope := group.counter_scope if runner.config.collect_counters else Counter_Scope.Disabled
	counters: Counter_Set
	if scope == .Calling_Thread {
		counters = counters_open()
	}
	defer counters_close(&counters)
	total: Counter_Total
	per_op := make([dynamic]f64, 0, runner.config.max_samples, runner.allocator)
	defer delete(per_op)
	observations: []Counter_Sample
	if scope == .Calling_Thread {
		observations = make([]Counter_Sample, runner.config.max_samples, runner.allocator)
	}
	defer delete(observations, runner.allocator)
	for sample_index in 0 ..< runner.config.max_samples {
		elapsed := sample_ns(bench, chunk, sample_index, &counters)
		append(&per_op, f64(elapsed) / f64(chunk))
		accumulate_counters(&total, &counters, chunk)
		if scope == .Calling_Thread {
			observation := &observations[sample_index]
			observation.valid = counters.valid
			observation.coverage = counters.coverage
			observation.ipc_grouped = counters.ipc_grouped
			for kind in Counter_Kind {
				observation.values[kind] = counters.values[kind] / f64(chunk)
			}
		}
		if samples_complete(runner.config, per_op[:]) {
			break
		}
	}
	after: Memory_Reading
	if bench.memory_probe != nil {
		after = bench.memory_probe(bench.user)
	}
	stats := compute_stats(per_op[:])
	result := Result {
		group            = group.name,
		name             = full_name,
		throughput       = group.throughput,
		chunk_size       = chunk,
		stats            = stats,
		counter_scope    = scope,
		memory_requested = bench.memory_probe != nil,
	}
	if stats.median > 0 {
		result.ops_per_second = 1e9 / stats.median
	}
	apply_counters(&result, total)
	apply_memory(&result, before, after)
	result.samples = make([]f64, len(per_op), runner.allocator)
	copy(result.samples, per_op[:])
	if observations != nil {
		result.counter_samples = make([]Counter_Sample, len(per_op), runner.allocator)
		copy(result.counter_samples, observations[:len(per_op)])
	}
	return result
}
