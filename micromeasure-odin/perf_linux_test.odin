#+build linux
package micromeasure

import "core:testing"

@(test)
test_failed_counter_io_does_not_report_a_zero_measurement :: proc(t: ^testing.T) {
	set := fake_counter_set()
	// Invalid descriptors make reads and ioctls fail regardless of PMU access.
	for kind in Counter_Kind {
		set.platform.fds[kind] = -1
		set.platform.open[kind] = true
		set.platform.leader[kind] = kind
	}
	set.platform.leader[Counter_Kind.Instructions] = .Cycles
	counters_begin(&set)
	counters_end(&set)
	for kind in Counter_Kind {
		testing.expect(t, !counters_has(&set, kind))
		testing.expect_value(t, set.values[kind], f64(0))
		testing.expect_value(t, set.coverage[kind], f64(0))
	}
	reading, ok := counter_read(-1)
	testing.expect(t, !ok)
}
