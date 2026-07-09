# Linux PMU Setup

`micromeasure` is strongly Linux-specific for its headline feature: direct integration with Linux perf events and PMU counters. The timing side is portable, but the most important measurements — instructions retired, branch instructions, branch misses, cache misses, frontend/backend stalls — only work when the kernel exposes perf events to your process.

## What you need

The crate links `perf-event2` on Linux. At runtime, the question is whether your process can actually open a perf event, which is governed by `kernel.perf_event_paranoid`.

### Check the current setting

```sh
cat /proc/sys/kernel/perf_event_paranoid
```

Common values:

| Value | Meaning |
|---|---|
| `-1` or `0` | broad access |
| `1` or `2` | common developer-friendly settings |
| `3` or `4` | often too restrictive for useful PMU access in normal user sessions |

### Lower it temporarily (until reboot)

```sh
sudo sysctl kernel.perf_event_paranoid=2
```

### Make it persistent

```sh
echo 'kernel.perf_event_paranoid=2' | sudo tee /etc/sysctl.d/99-micromeasure.conf
sudo sysctl --system
```

### Other requirements

Depending on your environment, you may also need one of:

- `CAP_PERFMON`
- `CAP_SYS_ADMIN`
- a container/runtime configuration that allows `perf_event_open`

This matters in containers, CI environments, and some locked-down distributions where the kernel setting alone is not enough.

## What happens when PMU is unavailable

The crate degrades gracefully. When `perf_event_open` fails, the runner falls back to **timing-only** measurement and tells you it has done so:

- The stats table will not have `instructions/op`, `branches/op`, `cache misses/op`, or stall counters.
- The `Measurement` row reads `timing only` instead of `timing + PMU`.
- The `host PMU (perf event group): coverage=...` byline is omitted.
- The `possible bottlenecks:` section is suppressed (it is derived from PMU counters).

Throughput and latency statistics are still valid in this mode — they only depend on `Instant::now()` and the operation count.

## The two-level perf fallback

Even when PMU access is available, the kernel may not let you open a perf-event **group** (multiple counters in one leader) but will let you open **individual** counters. `LinuxPerfBackend` handles this:

1. Try to open a perf-event group covering the full counter set.
2. If that fails, fall back to opening individual counters one at a time.
3. Whatever counters it gets, it records them and scales multiplexed values using `time_running / time_enabled` from the perf event's `time_enabled`/`time_running` fields.

The PMU coverage line reports `time_running / time_enabled` as a percentage. Below 100% means the kernel multiplexed the counters (because the PMU can't count all of them at once on your CPU) and the values were scaled. If coverage is low the runner emits a warning.

## Which counters are collected

`PerfCounters` collects:

- cycles
- instructions
- cache references
- L1 instruction cache misses
- branches
- branch misses
- cache misses
- stalled cycles frontend
- stalled cycles backend

The `has_*` flags on `Results` record which were actually available; the stats table only renders rows for counters that were collected.

## CPU pinning

On Linux the runner pins the measuring thread to a detected performance core (via `detect_performance_cores`). This prevents the kernel from silently migrating the benchmark to a different core mid-sample, which would invalidate cache state and produce nonsense PMU numbers. The pinning behaviour can be disabled if you have a reason to — see the affinity module source for the opt-out.

If performance-core detection fails (no `cpuinfo_cur_freq`, no `/sys/devices/cpu_*`, etc.) the runner continues without pinning and prints a one-time warning.

## macOS / Windows / other

On non-Linux targets the crate builds without the perf dependency and uses `WallClockBackend` as the platform default. You get timing and throughput only. If your primary goal is portable benchmarking across platforms, [Criterion](https://docs.rs/criterion) is usually the better fit — see [micromeasure vs Criterion](./vs-criterion.md).

## Verifying PMU is working

After running a benchmark, look for:

```text
host PMU (perf event group): coverage=100.0%
```

- `coverage=100.0%` — full PMU group active, no multiplexing. Ideal.
- `coverage=80.0%` (or any value < 100%) — counters were multiplexed; values were scaled. Numbers are still useful but check the warning.
- No coverage line at all — PMU unavailable, timing-only fallback.

For GPU-domain benchmarks, the byline reads `host PMU (orchestration): coverage=...` to remind you the CPU PMU describes the host thread, not the device. See [GPU Benchmarks](./gpu.md#measurement-domain).