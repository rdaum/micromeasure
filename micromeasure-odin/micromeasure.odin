// Package micromeasure is a small microbenchmark harness for Odin.
//
// It follows the measurement model of the Rust `micromeasure` crate:
// hand-written benchmark bodies, warmup, chunk calibration, repeated samples,
// robust statistics, and an optional persisted baseline for comparisons.
//
// A benchmark body receives a chunk size and a chunk number. The body runs
// `chunk_size` operations. The harness calibrates the chunk so that one sample
// is close to the configured target time.
//
// Typical use:
//
//	runner: micromeasure.Runner
//	micromeasure.runner_init(&runner)
//	defer micromeasure.runner_destroy(&runner)
//
//	group := micromeasure.group(&runner, "var/value", micromeasure.throughput_ops())
//	micromeasure.bench(group, "int_construct", state, bench_int_construct)
//
//	micromeasure.runner_run(&runner)
//	micromeasure.report(&runner, nil)
package micromeasure

import "core:time"

// Default measurement configuration.
DEFAULT_CONFIG :: Config {
	warmup           = 200 * time.Millisecond,
	target_sample    = 20 * time.Millisecond,
	min_samples      = 10,
	max_samples      = 40,
	noise_cv         = 0.05,
	collect_counters = true,
}

// QUICK_CONFIG is a shorter configuration for fast feedback.
QUICK_CONFIG :: Config {
	warmup           = 50 * time.Millisecond,
	target_sample    = 5 * time.Millisecond,
	min_samples      = 5,
	max_samples      = 15,
	noise_cv         = 0.10,
	collect_counters = true,
}
