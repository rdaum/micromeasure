#+build linux
// Linux user-space PMU collection on the calling thread.
package micromeasure

import "core:mem"
import "core:sys/linux"

counter_hardware_id :: proc(kind: Counter_Kind) -> linux.Perf_Hardware_Id {
	switch kind {
	case .Cycles:
		return .CPU_CYCLES
	case .Instructions:
		return .INSTRUCTIONS
	case .Cache_References:
		return .CACHE_REFERENCES
	case .Cache_Misses:
		return .CACHE_MISSES
	case .Branches:
		return .BRANCH_INSTRUCTIONS
	case .Branch_Misses:
		return .BRANCH_MISSES
	case .Stalled_Frontend:
		return .STALLED_CYCLES_FRONTEND
	case .Stalled_Backend:
		return .STALLED_CYCLES_BACKEND
	}
	return .INSTRUCTIONS
}

@(private)
Counter_Platform :: struct {
	fds:      [COUNTER_COUNT]linux.Fd,
	open:     [COUNTER_COUNT]bool,
	leader:   [COUNTER_COUNT]Counter_Kind,
	before:   [COUNTER_COUNT]Counter_Read,
	begin_ok: [COUNTER_COUNT]bool,
}

@(private)
PERF_IOC_ENABLE :: u32(0x2400)
@(private)
PERF_IOC_DISABLE :: u32(0x2401)
@(private)
PERF_IOC_FLAG_GROUP :: uintptr(1)

counters_open :: proc() -> Counter_Set {
	set: Counter_Set
	p := &set.platform
	for kind in Counter_Kind {
		p.leader[kind] = kind
		group_fd := linux.Fd(-1)
		// IPC requires both events to count the same scheduled intervals.
		if kind == .Instructions && p.open[Counter_Kind.Cycles] {
			group_fd = p.fds[Counter_Kind.Cycles]
			p.leader[kind] = .Cycles
		}
		attr: linux.Perf_Event_Attr
		attr.type = .HARDWARE
		attr.size = u32(size_of(linux.Perf_Event_Attr))
		attr.config.hw = counter_hardware_id(kind)
		attr.flags = {.Exclude_Kernel, .Exclude_HV}
		if group_fd == -1 {
			attr.flags += {.Disabled}
		}
		attr.read_format = {.TOTAL_TIME_ENABLED, .TOTAL_TIME_RUNNING}
		fd, err := linux.perf_event_open(&attr, 0, -1, group_fd, {})
		if err != .NONE {
			continue
		}
		p.fds[kind] = fd
		p.open[kind] = true
		set.usable = true
		if kind == .Instructions && group_fd != -1 {
			set.ipc_grouped = true
		}
	}
	return set
}

counters_close :: proc(set: ^Counter_Set) {
	p := &set.platform
	for kind in Counter_Kind {
		if p.open[kind] {
			linux.close(p.fds[kind])
		}
	}
	set^ = {}
}

@(private)
counter_read :: proc(fd: linux.Fd) -> (Counter_Read, bool) {
	buffer: [3]u64
	bytes := mem.slice_to_bytes(buffer[:])
	// Use the native ABI layout: count, time_enabled, time_running.
	n, err := linux.read(fd, bytes)
	if err != .NONE || n != size_of(Counter_Read) {
		return {}, false
	}
	return Counter_Read{buffer[0], buffer[1], buffer[2]}, true
}

counters_begin :: proc(set: ^Counter_Set) {
	p := &set.platform
	set.values = {}
	set.valid = {}
	set.coverage = {}
	p.begin_ok = {}
	// Snapshot while all groups are disabled. Never reset cumulative time.
	for kind in Counter_Kind {
		if p.open[kind] {
			p.before[kind], p.begin_ok[kind] = counter_read(p.fds[kind])
		}
	}
	for kind in Counter_Kind {
		if !p.open[kind] || p.leader[kind] != kind {
			continue
		}
		ok := linux.ioctl(p.fds[kind], PERF_IOC_ENABLE, PERF_IOC_FLAG_GROUP) == 0
		for member in Counter_Kind {
			if p.leader[member] == kind {
				p.begin_ok[member] = p.begin_ok[member] && ok
			}
		}
	}
}

counters_end :: proc(set: ^Counter_Set) {
	p := &set.platform
	// Disable every group before reading, so reads do not count as workload.
	for kind in Counter_Kind {
		if !p.open[kind] || p.leader[kind] != kind {
			continue
		}
		ok := linux.ioctl(p.fds[kind], PERF_IOC_DISABLE, PERF_IOC_FLAG_GROUP) == 0
		for member in Counter_Kind {
			if p.leader[member] == kind {
				p.begin_ok[member] = p.begin_ok[member] && ok
			}
		}
	}
	for kind in Counter_Kind {
		if !p.open[kind] || !p.begin_ok[kind] {
			continue
		}
		after, ok := counter_read(p.fds[kind])
		if ok {
			set.values[kind], set.coverage[kind], set.valid[kind] = scale_counter(
				p.before[kind],
				after,
			)
		}
	}
}
