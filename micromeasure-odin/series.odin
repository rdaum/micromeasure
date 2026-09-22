#+feature dynamic-literals
// Portable raw-sample interchange with the Rust micromeasure CLI.
package micromeasure

import "core:encoding/json"
import "core:fmt"
import "core:math"
import "core:mem/virtual"
import "core:os"
import "core:strings"
import "core:time"

// Strings and maps are borrowed for save_json_report. Suite and runner_id are required.
// Environment participates in comparison matching; provenance does not.
Report_Context :: struct {
	suite, runner_id:                         string,
	environment, provenance:                  map[string]string,
	compiler, build_flags, machine, revision: string,
	// Set when the caller's independent correctness checks failed.
	invalid_reason:                           string,
}

Series_Validity :: struct {
	status: string,
	reason: string `json:"reason,omitempty"`,
}
Series_Context :: struct {
	runner_id:               string,
	environment, provenance: map[string]string,
}
Series_Result :: struct {
	group, name, measurement, unit, direction: string,
	samples:                                   []f64,
	validity:                                  Series_Validity,
	dimensions, provenance:                    map[string]string,
}
// The strict micromeasure-series version 1 wire schema.
Report_Document :: struct {
	document_type:    string,
	schema_version:   int,
	timestamp, suite: string,
	validity:         Series_Validity,
	ctx:              Series_Context `json:"context"`,
	results:          []Series_Result,
}

@(private)
series_nonempty :: proc(value: string) -> bool {
	return len(strings.trim_space(value)) > 0
}

@(private)
series_map_valid :: proc(values: map[string]string) -> bool {
	for key, value in values {
		if !series_nonempty(key) || !series_nonempty(value) {return false}
	}
	return true
}

@(private)
series_finite :: proc(value: f64) -> bool {
	return !math.is_nan(value) && !math.is_inf(value)
}

@(private)
series_base :: proc(result: Result, measurement, unit, direction: string) -> Series_Result {
	name := result.name
	prefix := fmt.aprintf("%s/", result.group)
	if strings.has_prefix(name, prefix) {name = name[len(prefix):]}
	return Series_Result {
		group = result.group,
		name = name,
		measurement = measurement,
		unit = unit,
		direction = direction,
		samples = make([]f64, 0),
		validity = {status = "valid"},
		dimensions = make(map[string]string),
		provenance = map[string]string {
			"chunk_size" = fmt.aprintf("%d", result.chunk_size),
			"timing_sample_count" = fmt.aprintf("%d", len(result.samples)),
		},
	}
}

@(private)
series_invalidate :: proc(series: ^Series_Result, reason: string) {
	series.validity = {
		status = "invalid",
		reason = reason,
	}
	// Do not fill failed reads with zero or present a partial series as complete.
	series.samples = make([]f64, 0)
}

@(private)
series_timing :: proc(result: Result, throughput: bool) -> Series_Result {
	series := series_base(result, "latency", "ns/op", "lower")
	series.dimensions["sample_kind"] = "batch-average"
	series.dimensions["scope"] = "benchmark-body"
	series.dimensions["normalization"] = "harness-operation"
	if throughput {
		series.measurement = "throughput"
		series.unit = fmt.aprintf("%s/s", result.throughput.unit)
		series.direction = "higher"
		series.provenance["units_per_op"] = fmt.aprintf("%v", result.throughput.units_per_op)
		if !series_nonempty(result.throughput.unit) ||
		   !series_finite(result.throughput.units_per_op) ||
		   result.throughput.units_per_op <= 0 {
			series_invalidate(&series, "invalid throughput specification")
			return series
		}
	}
	series.samples = make([]f64, len(result.samples))
	for elapsed, index in result.samples {
		if !series_finite(elapsed) || elapsed < 0 || (throughput && elapsed == 0) {
			series_invalidate(&series, "timing samples cannot represent this metric")
			return series
		}
		value := 1e9 / elapsed * result.throughput.units_per_op if throughput else elapsed
		if !series_finite(value) {
			series_invalidate(&series, "derived throughput is not finite")
			return series
		}
		series.samples[index] = value
	}
	if len(series.samples) == 0 {series_invalidate(&series, "no timing samples")}
	return series
}

@(private)
series_counter :: proc(result: Result, kind: Counter_Kind, coverage: bool) -> Series_Result {
	metric := counter_name(kind)
	unit := "ratio" if coverage else "count/op"
	series := series_base(result, "custom", unit, "informational")
	series.dimensions = map[string]string {
		"metric"        = fmt.aprintf("%s.coverage", metric) if coverage else metric,
		"scope"         = "calling-thread-user",
		"sample_kind"   = "batch-average",
		"normalization" = "harness-operation",
		"scaling"       = "enabled-over-running",
	}
	series.samples = make([]f64, len(result.counter_samples))
	for sample, index in result.counter_samples {
		value := sample.coverage[kind] if coverage else sample.values[kind]
		if !sample.valid[kind] ||
		   !series_finite(value) ||
		   value < 0 ||
		   !series_finite(sample.coverage[kind]) ||
		   sample.coverage[kind] <= 0 ||
		   sample.coverage[kind] > 1 {
			series_invalidate(&series, "counter unavailable or invalid in at least one sample")
			return series
		}
		series.samples[index] = value
	}
	if len(series.samples) == 0 || len(series.samples) != len(result.samples) {
		series_invalidate(&series, "counter observations missing for timing samples")
	}
	return series
}

@(private)
series_ipc :: proc(result: Result) -> Series_Result {
	series := series_base(result, "custom", "instructions/cycle", "informational")
	series.dimensions = map[string]string {
		"metric"      = "ipc",
		"scope"       = "calling-thread-user",
		"sample_kind" = "batch-average",
		"scheduling"  = "grouped-cycles-instructions",
	}
	series.samples = make([]f64, len(result.counter_samples))
	for sample, index in result.counter_samples {
		cycles, instructions :=
			sample.values[Counter_Kind.Cycles], sample.values[Counter_Kind.Instructions]
		if !sample.ipc_grouped ||
		   !sample.valid[Counter_Kind.Cycles] ||
		   !sample.valid[Counter_Kind.Instructions] ||
		   cycles <= 0 ||
		   instructions < 0 ||
		   !series_finite(cycles) ||
		   !series_finite(instructions) ||
		   !series_finite(instructions / cycles) {
			series_invalidate(
				&series,
				"IPC requires valid grouped cycles and instructions in every sample",
			)
			return series
		}
		series.samples[index] = instructions / cycles
	}
	if len(series.samples) == 0 || len(series.samples) != len(result.samples) {
		series_invalidate(&series, "counter observations missing for timing samples")
	}
	return series
}

@(private)
series_memory :: proc(result: Result) -> Series_Result {
	series := series_base(result, "memory", "bytes", "informational")
	series.dimensions = map[string]string {
		"kind"  = memory_kind_name(result.memory_kind),
		"scope" = "whole-benchmark-including-warmup-calibration-and-hooks",
	}
	if result.memory_available {
		series.samples = make([]f64, 1)
		series.samples[0] = f64(result.memory)
		series.provenance["before_bytes"] = fmt.aprintf("%d", result.memory_before.bytes)
		series.provenance["after_bytes"] = fmt.aprintf("%d", result.memory_after.bytes)
	} else {
		series_invalidate(&series, "memory probe unavailable or inconsistent")
	}
	return series
}

// Writes an atomic micromeasure-series v1 report. Returns false for invalid
// identity/context or I/O failure. Unavailable metrics have invalid validity.
// A successful body is assumed correct unless invalid_reason is supplied.
save_json_report :: proc(path: string, runner: ^Runner, metadata: Report_Context) -> bool {
	if !series_nonempty(metadata.suite) ||
	   !series_nonempty(metadata.runner_id) ||
	   !series_map_valid(metadata.environment) ||
	   !series_map_valid(metadata.provenance) ||
	   (metadata.invalid_reason != "" && !series_nonempty(metadata.invalid_reason)) {return false}
	arena: virtual.Arena
	if virtual.arena_init_growing(&arena) != nil {return false}
	defer virtual.arena_destroy(&arena)
	context.allocator = virtual.arena_allocator(&arena)
	timestamp, ok := time.time_to_rfc3339(time.now())
	if !ok {return false}
	document := Report_Document {
		document_type = "micromeasure-series",
		schema_version = 1,
		timestamp = timestamp,
		suite = metadata.suite,
		validity = {status = "valid"},
		ctx = {
			runner_id = metadata.runner_id,
			environment = make(map[string]string),
			provenance = make(map[string]string),
		},
	}
	for key, value in metadata.environment {document.ctx.environment[key] = value}
	for key, value in metadata.provenance {document.ctx.provenance[key] = value}
	provenance := &document.ctx.provenance
	provenance^["producer"] = "odin"
	provenance^["compiler"] = metadata.compiler if metadata.compiler != "" else ODIN_VERSION
	provenance^["operating_system"] = fmt.aprintf("%v", ODIN_OS)
	provenance^["architecture"] = fmt.aprintf("%v", ODIN_ARCH)
	if metadata.build_flags != "" {provenance^["build_flags"] = metadata.build_flags}
	if metadata.machine != "" {provenance^["machine"] = metadata.machine}
	if metadata.revision != "" {provenance^["revision"] = metadata.revision}
	provenance^["warmup_ns"] = fmt.aprintf("%d", time.duration_nanoseconds(runner.config.warmup))
	provenance^["target_sample_ns"] = fmt.aprintf(
		"%d",
		time.duration_nanoseconds(runner.config.target_sample),
	)
	provenance^["min_samples"] = fmt.aprintf("%d", runner.config.min_samples)
	provenance^["max_samples"] = fmt.aprintf("%d", runner.config.max_samples)
	provenance^["noise_cv"] = fmt.aprintf("%v", runner.config.noise_cv)
	provenance^["collect_counters"] = fmt.aprintf("%t", runner.config.collect_counters)
	if !series_map_valid(provenance^) {return false}
	if metadata.invalid_reason != "" {
		document.validity = {
			status = "invalid",
			reason = metadata.invalid_reason,
		}
	} else if len(runner.results) == 0 {return false}
	results := make([dynamic]Series_Result)
	// The runner permits duplicate registrations; the interchange must not.
	identities := make(map[string]bool)
	for result in runner.results {
		latency := series_timing(result, false)
		if !series_nonempty(latency.group) || !series_nonempty(latency.name) {return false}
		identity := fmt.aprintf("%d:%s%s", len(latency.group), latency.group, latency.name)
		if identities[identity] {return false}
		identities[identity] = true
		append(&results, latency, series_timing(result, true))
		if result.counter_scope == .Calling_Thread {
			for kind in Counter_Kind {
				append(
					&results,
					series_counter(result, kind, false),
					series_counter(result, kind, true),
				)
			}
			append(&results, series_ipc(result))
		}
		if result.memory_requested ||
		   result.memory_available {append(&results, series_memory(result))}
	}
	document.results = results[:]
	bytes, err := json.marshal(document, {pretty = true})
	if err != nil {return false}
	return series_write_atomic(path, bytes)
}

@(private)
series_write_atomic :: proc(path: string, bytes: []u8) -> bool {
	file, err := os.create_temp_file(os.dir(path), ".micromeasure-*")
	if err != nil {return false}
	temporary_path := strings.clone(os.name(file))
	defer os.remove(temporary_path)
	closed := false
	defer if !closed {os.close(file)}
	remaining := bytes
	for len(remaining) > 0 {
		written, write_err := os.write(file, remaining)
		if write_err != nil || written <= 0 {return false}
		remaining = remaining[written:]
	}
	if os.sync(file) != nil {return false}
	close_err := os.close(file)
	closed = true
	if close_err != nil {return false}
	return os.rename(temporary_path, path) == nil
}
