// Linux process RSS observations. These are not allocation measurements.
package micromeasure

import "core:fmt"
import "core:os"
import "core:strconv"
import "core:strings"

// Parses a nonnegative field in kB. A missing or malformed field is unavailable.
@(private)
parse_status_field_kb :: proc(data, field: string) -> (int, bool) {
	remaining := data
	for line in strings.split_lines_iterator(&remaining) {
		if !strings.has_prefix(line, field) {
			continue
		}
		rest := strings.trim_space(line[len(field):])
		if !strings.has_suffix(rest, "kB") {
			return 0, false
		}
		rest = strings.trim_space(rest[:len(rest) - 2])
		kb, ok := strconv.parse_int(rest)
		if !ok || kb < 0 || kb > max(int) / 1024 {
			return 0, false
		}
		return kb, true
	}
	return 0, false
}

read_status_field_kb :: proc(field: string) -> (int, bool) {
	when ODIN_OS != .Linux {return 0, false}
	data, err := os.read_entire_file("/proc/self/status", context.allocator)
	if err != nil {
		return 0, false
	}
	defer delete(data)
	return parse_status_field_kb(string(data), field)
}

// Process-lifetime high-water mark. Its delta is additional high-water growth.
// Previous allocations can mask later activity, even when a filter is active.
peak_rss_bytes :: proc(_: rawptr = nil) -> Memory_Reading {
	kb, ok := read_status_field_kb("VmHWM:")
	return Memory_Reading{bytes = kb * 1024, available = ok, kind = .Peak_RSS_Growth}
}

// Current process RSS. Its signed delta describes retained resident memory.
// Temporary allocations released between probes are not visible.
current_rss_bytes :: proc(_: rawptr = nil) -> Memory_Reading {
	kb, ok := read_status_field_kb("VmRSS:")
	return Memory_Reading{bytes = kb * 1024, available = ok, kind = .Current_RSS_Delta}
}

// Formats a byte count as a human-readable size (B, KiB, MiB, GiB).
format_bytes :: proc(bytes: int) -> string {
	if bytes < 1024 {
		return fmt.aprintf("%d B", bytes)
	}
	units := []string{"KiB", "MiB", "GiB"}
	value := f64(bytes) / 1024
	unit := "KiB"
	for i in 0 ..< len(units) {
		unit = units[i]
		if value < 1024 || i == len(units) - 1 {
			break
		}
		value /= 1024
	}
	return fmt.aprintf("%.1f %s", value, unit)
}
