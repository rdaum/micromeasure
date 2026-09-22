// Portable counter accounting. Platform files implement collection.
package micromeasure

Counter_Kind :: enum {
	Cycles,
	Instructions,
	Cache_References,
	Cache_Misses,
	Branches,
	Branch_Misses,
	Stalled_Frontend,
	Stalled_Backend,
}

COUNTER_COUNT :: len(Counter_Kind)

counter_name :: proc(kind: Counter_Kind) -> string {
	switch kind {
	case .Cycles:
		return "cycles"
	case .Instructions:
		return "instructions"
	case .Cache_References:
		return "cache_references"
	case .Cache_Misses:
		return "cache_misses"
	case .Branches:
		return "branches"
	case .Branch_Misses:
		return "branch_misses"
	case .Stalled_Frontend:
		return "stalled_cycles_frontend"
	case .Stalled_Backend:
		return "stalled_cycles_backend"
	}
	return "unknown"
}

// Kernel values are cumulative. Sampling uses deltas for all three fields.
Counter_Read :: struct {
	count, enabled, running: u64,
}
Counter_Set :: struct {
	values:      [COUNTER_COUNT]f64,
	valid:       [COUNTER_COUNT]bool,
	coverage:    [COUNTER_COUNT]f64,
	usable:      bool,
	ipc_grouped: bool,
	platform:    Counter_Platform,
}

counters_has :: proc(set: ^Counter_Set, kind: Counter_Kind) -> bool {
	return set.valid[kind]
}

@(private)
scale_counter :: proc(before, after: Counter_Read) -> (value, coverage: f64, valid: bool) {
	if after.count < before.count ||
	   after.enabled < before.enabled ||
	   after.running < before.running {
		return 0, 0, false
	}
	enabled := after.enabled - before.enabled
	running := after.running - before.running
	if enabled == 0 || running == 0 || running > enabled {
		return 0, 0, false
	}
	coverage = f64(running) / f64(enabled)
	return f64(after.count - before.count) / coverage, coverage, true
}

@(private)
Counter_Total :: struct {
	values:      [COUNTER_COUNT]f64,
	valid:       [COUNTER_COUNT]bool,
	coverage:    [COUNTER_COUNT]f64,
	samples:     int,
	operations:  u64,
	ipc_grouped: bool,
}

@(private)
accumulate_counters :: proc(total: ^Counter_Total, set: ^Counter_Set, operations: int) {
	for kind in Counter_Kind {
		if total.samples == 0 {
			total.valid[kind] = set.valid[kind]
			total.coverage[kind] = set.coverage[kind]
		} else {
			total.valid[kind] = total.valid[kind] && set.valid[kind]
			total.coverage[kind] = min(total.coverage[kind], set.coverage[kind])
		}
		total.values[kind] += set.values[kind]
	}
	total.ipc_grouped = set.ipc_grouped && (total.samples == 0 || total.ipc_grouped)
	total.samples += 1
	total.operations += u64(operations)
}

@(private)
apply_counters :: proc(result: ^Result, total: Counter_Total) {
	if total.operations == 0 {
		return
	}
	values := total.values
	for kind in Counter_Kind {
		if total.valid[kind] {
			values[kind] /= f64(total.operations)
			result.counters_available = true
			result.counter_coverage[kind] = total.coverage[kind]
		} else {
			values[kind] = 0
		}
	}
	result.counters = Counters {
		cycles                  = values[Counter_Kind.Cycles],
		instructions            = values[Counter_Kind.Instructions],
		cache_references        = values[Counter_Kind.Cache_References],
		cache_misses            = values[Counter_Kind.Cache_Misses],
		branches                = values[Counter_Kind.Branches],
		branch_misses           = values[Counter_Kind.Branch_Misses],
		stalled_cycles_frontend = values[Counter_Kind.Stalled_Frontend],
		stalled_cycles_backend  = values[Counter_Kind.Stalled_Backend],
		has_cycles              = total.valid[Counter_Kind.Cycles],
		has_instructions        = total.valid[Counter_Kind.Instructions],
		has_cache_references    = total.valid[Counter_Kind.Cache_References],
		has_cache_misses        = total.valid[Counter_Kind.Cache_Misses],
		has_branches            = total.valid[Counter_Kind.Branches],
		has_branch_misses       = total.valid[Counter_Kind.Branch_Misses],
		has_stalled_frontend    = total.valid[Counter_Kind.Stalled_Frontend],
		has_stalled_backend     = total.valid[Counter_Kind.Stalled_Backend],
	}
	result.ipc_available =
		total.ipc_grouped && result.counters.has_cycles && result.counters.has_instructions
}
