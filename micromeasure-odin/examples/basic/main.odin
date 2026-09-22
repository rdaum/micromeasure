package main

import mm "../.."
import "core:os"

State :: struct {
	input, output: u64,
}

body :: proc(user: rawptr, chunk, _: int) {
	state := (^State)(user)
	value := mm.black_box(state.input)
	for _ in 0 ..< chunk {
		value = value * 1664525 + 1013904223
	}
	state.output = mm.black_box(value)
}

main :: proc() {
	state := State {
		input = 42,
	}
	runner: mm.Runner
	mm.runner_init(&runner, mm.QUICK_CONFIG)
	defer mm.runner_destroy(&runner)
	group := mm.group(&runner, "example")
	mm.bench(group, "integer-recurrence", &state, body)
	mm.runner_run(&runner)
	mm.report(&runner)
	if len(os.args) > 1 {
		assert(len(os.args) > 2, "JSON export requires a runner ID as the second argument")
		assert(
			mm.save_json_report(
				os.args[1],
				&runner,
				{suite = "example", runner_id = os.args[2], build_flags = "-o:speed"},
			),
		)
	}
}
