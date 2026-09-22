// Terminal reports and baseline persistence.
package micromeasure

import "core:fmt"
import "core:math"
import "core:os"
import "core:strconv"
import "core:strings"

// Prints a report of all measured results. When `baseline` holds previous
// nanoseconds-per-operation values keyed by full benchmark name, the report
// adds a delta column.
report :: proc(runner: ^Runner, baseline: map[string]f64 = nil) {
	text := format_report(runner, baseline)
	defer delete(text)
	fmt.print(text)
}

// Returns caller-owned text. The percentage delta is descriptive, not a significance test.
format_report :: proc(runner: ^Runner, baseline: map[string]f64 = nil) -> string {
	output: strings.Builder
	strings.builder_init(&output)
	if len(runner.results) == 0 {
		return strings.clone("no benchmarks ran\n")
	}

	has_baseline := baseline != nil && len(baseline) > 0
	has_counters := false
	has_memory := false
	for result in runner.results {
		if result.counters_available {
			has_counters = true
		}
		if result.memory_available {
			has_memory = true
		}
		if has_counters && has_memory {
			break
		}
	}
	name_width := 20
	for result in runner.results {
		name_width = max(name_width, len(result.name) + 2)
	}

	header: strings.Builder
	strings.builder_init(&header)
	defer strings.builder_destroy(&header)
	write_name(&header, "benchmark", name_width)
	write_field(&header, 11, "%s", "ns/op")
	write_field(&header, 15, "%s", "ops/s")
	write_field(&header, 10, "%s", "batch-p95")
	write_field(&header, 8, "%s", "cv%")
	write_field(&header, 5, "%s", "n")
	write_field(&header, 12, "%s", "throughput")
	if has_counters {
		write_field(&header, 8, "%s", "insn/op")
		write_field(&header, 8, "%s", "cyc/op")
		write_field(&header, 7, "%s", "IPC")
		write_field(&header, 7, "%s", "br/op")
		write_field(&header, 14, "%s", "PMU scope")
		write_field(&header, 8, "%s", "run%")
	}
	if has_memory {
		write_field(&header, 12, "%s", "delta (B)")
		write_field(&header, 20, "%s", "memory kind")
	}
	if has_baseline {
		write_field(&header, 10, "%s", "delta")
	}
	fmt.sbprintf(&output, "%s\n", strings.to_string(header))

	current_group := ""
	for result in runner.results {
		if result.group != current_group {
			current_group = result.group
			fmt.sbprintf(&output, "[%s]\n", current_group)
		}

		line: strings.Builder
		strings.builder_init(&line)
		defer strings.builder_destroy(&line)
		write_name(&line, result.name, name_width)
		write_field(&line, 11, "%.2f", result.stats.median)
		write_field(&line, 15, "%.0f", result.ops_per_second)
		write_field(&line, 10, "%.2f", result.stats.p95)
		write_field(&line, 8, "%.2f%%", result.stats.cv * 100)
		write_field(&line, 5, "%d", len(result.samples))

		throughput := result.throughput.units_per_op * result.ops_per_second
		write_field(&line, 9, "%.4g", throughput)
		fmt.sbprintf(&line, " %s/s", result.throughput.unit)

		if has_counters {
			instructions, has_instructions := counter_value(result.counters, .Instructions)
			cycles, has_cycles := counter_value(result.counters, .Cycles)
			branches, has_branches := counter_value(result.counters, .Branches)
			if has_instructions {
				write_field(&line, 8, "%.1f", instructions)
			} else {
				write_field(&line, 8, "%s", "-")
			}
			if has_cycles {
				write_field(&line, 8, "%.1f", cycles)
			} else {
				write_field(&line, 8, "%s", "-")
			}
			if result.ipc_available && has_instructions && has_cycles && cycles > 0 {
				write_field(&line, 7, "%.2f", instructions / cycles)
			} else {
				write_field(&line, 7, "%s", "-")
			}
			if has_branches {
				write_field(&line, 7, "%.2f", branches)
			} else {
				write_field(&line, 7, "%s", "-")
			}
			write_field(
				&line,
				14,
				"%s",
				"calling-thread" if result.counter_scope == .Calling_Thread else "disabled",
			)
			coverage := f64(1)
			for kind in Counter_Kind {
				if _, valid := counter_value(result.counters, kind); valid {
					coverage = min(coverage, result.counter_coverage[kind])
				}
			}
			if result.counters_available {
				write_field(
					&line,
					8,
					"%.1f",
					100 * coverage,
				)} else {write_field(&line, 8, "%s", "-")
			}
		}

		if has_memory {
			if result.memory_available {
				write_field(&line, 12, "%d", result.memory)
			} else {
				write_field(&line, 12, "%s", "-")
			}
			write_field(
				&line,
				20,
				"%s",
				memory_kind_name(result.memory_kind) if result.memory_available else "-",
			)
		}

		if has_baseline {
			if previous, found := baseline[result.name]; found && previous > 0 {
				delta := (result.stats.median - previous) / previous * 100
				write_field(&line, 10, "%.2f%%", delta)
			} else {
				write_field(&line, 10, "%s", "-")
			}
		}
		fmt.sbprintf(&output, "%s\n", strings.to_string(line))
	}
	strings.write_string(
		&output,
		"Timing percentiles describe batch-average ns/op. CV and baseline deltas are descriptive.\n",
	)
	if has_memory {
		strings.write_string(
			&output,
			"Memory spans warmup, calibration, and samples. RSS deltas are process observations, not allocation costs.\n",
		)
	}
	return strings.to_string(output)
}

// Writes results as a tab-separated baseline file.
//
// Format: <name>\t<ns/op>\t<ops/s>\t<memory-bytes-or->\n
// The memory column is "-" when the benchmark did not record a memory
// observation.
save_report :: proc(path: string, runner: ^Runner) -> bool {
	builder: strings.Builder
	strings.builder_init(&builder)
	defer strings.builder_destroy(&builder)
	for result in runner.results {
		if strings.contains_any(result.name, "\t\r\n") {
			return false
		}
		if result.memory_available {
			fmt.sbprintf(
				&builder,
				"%s\t%.4f\t%.4f\t%d\n",
				result.name,
				result.stats.median,
				result.ops_per_second,
				result.memory,
			)
		} else {
			fmt.sbprintf(
				&builder,
				"%s\t%.4f\t%.4f\t-\n",
				result.name,
				result.stats.median,
				result.ops_per_second,
			)
		}
	}
	content := strings.to_string(builder)
	return os.write_entire_file(path, transmute([]u8)content) == nil
}

// Reads a baseline file written by `save_report`. Returns nil when the file
// is missing or unreadable.
load_baseline :: proc(path: string) -> map[string]f64 {
	data, err := os.read_entire_file(path, context.allocator)
	if err != nil {
		return nil
	}
	defer delete(data)

	baseline := make(map[string]f64)
	lines := strings.split_lines(string(data))
	defer delete(lines)
	for line in lines {
		if line == "" {
			continue
		}
		fields := strings.split(line, "\t")
		defer delete(fields)
		if len(fields) < 2 {
			continue
		}
		value, parse_ok := strconv.parse_f64(fields[1])
		if parse_ok && value > 0 && !math.is_nan(value) && !math.is_inf(value) {
			// The name must outlive the file buffer.
			if _, found := baseline[fields[0]]; found {
				baseline[fields[0]] = value
			} else {
				baseline[strings.clone(fields[0])] = value
			}
		}
	}
	return baseline
}

// Releases both the map and the owned benchmark-name strings.
baseline_destroy :: proc(baseline: ^map[string]f64) {
	for name in baseline^ {
		delete(name, baseline^.allocator)
	}
	delete(baseline^)
	baseline^ = nil
}

@(private)
write_name :: proc(builder: ^strings.Builder, name: string, width: int) {
	strings.write_string(builder, name)
	for _ in 0 ..< max(0, width - len(name)) {
		strings.write_byte(builder, ' ')
	}
}

memory_kind_name :: proc(kind: Memory_Kind) -> string {
	switch kind {
	case .Custom_Delta:
		return "custom-delta"
	case .Current_RSS_Delta:
		return "current-rss-delta"
	case .Peak_RSS_Growth:
		return "peak-rss-growth"
	}
	return "unknown"
}

@(private)
write_field :: proc(builder: ^strings.Builder, width: int, format: string, args: ..any) {
	strings.write_byte(builder, ' ')
	formatted: strings.Builder
	strings.builder_init(&formatted)
	defer strings.builder_destroy(&formatted)
	fmt.sbprintf(&formatted, format, ..args)
	text := strings.to_string(formatted)
	for _ in 0 ..< max(0, width - len(text)) {
		strings.write_byte(builder, ' ')
	}
	strings.write_string(builder, text)
}
