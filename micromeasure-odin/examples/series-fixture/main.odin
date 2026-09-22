// Deterministic observations for testing the Rust interchange contract.
package main

import mm "../.."
import "core:os"
import "core:strings"

main :: proc() {
	assert(
		len(os.args) == 2 || len(os.args) == 3,
		"expected output path and optional invalid-metrics mode",
	)
	runner: mm.Runner
	mm.runner_init(&runner)
	defer mm.runner_destroy(&runner)
	g := mm.group(&runner, "fixture", mm.throughput_per_op(0.25, "MiB"), .Disabled)
	samples := make([]f64, 3)
	copy(samples, []f64{80, 88, 96})
	append(
		&runner.results,
		mm.Result {
			group = g.name,
			name = strings.clone("fixture/body"),
			throughput = g.throughput,
			chunk_size = 8,
			samples = samples,
			counter_scope = .Disabled,
			memory_requested = true,
			memory_available = true,
			memory_kind = .Current_RSS_Delta,
			memory = -50,
			memory_before = {100, true, .Current_RSS_Delta},
			memory_after = {50, true, .Current_RSS_Delta},
		},
	)
	if len(os.args) == 3 {
		assert(os.args[2] == "invalid-metrics")
		runner.results[0].counter_scope = .Calling_Thread
		runner.results[0].memory_available = false
	}
	assert(
		mm.save_json_report(
			os.args[1],
			&runner,
			{suite = "interchange-fixture", runner_id = "fixture-runner"},
		),
	)
}
