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
- The `PMU: scheduled=...` byline is omitted.
- The `possible bottlenecks:` section is suppressed (it is derived from PMU counters).

Throughput and latency statistics are still valid in this mode — they only depend on `Instant::now()` and the operation count.

## Counter scheduling and fallback

Even when PMU access is available, the kernel may let you open a perf-event **group** that is too large to schedule on the available hardware registers. Such a group reads as zero even though creation and activation succeeded. `LinuxPerfBackend` handles this:

1. During standard benchmark calibration, try a perf-event group covering the full counter set.
2. If creation or activation fails, or the group obtains no usable scheduled window, remember that result and use individual counters for subsequent samples.
3. Micromeasure-managed concurrent workers and external multi-thread scopes start with individual counters. Managed workers are recreated for every sample, and probing an oversized group independently on every external worker would waste the calibration window.
4. Scale each multiplexed value using its own `time_running / time_enabled` values.

The PMU `scheduled` line reports `time_running / time_enabled` as a percentage. Below 100% means the kernel multiplexed the counters because the PMU could not count all of them at once. It does **not** report what fraction of a multi-threaded workload was observed. For individual counters, micromeasure reports the least-scheduled available event as a conservative quality indicator. If it is low the runner emits a warning.

Counters with no usable scheduled window are omitted rather than rendered as meaningful zeroes.

## RAPL energy measurement

On supported Intel and AMD processors, Linux exposes Running Average Power
Limit (RAPL) energy estimates as system-wide perf PMUs. Opt an ordinary
benchmark into every package/die-scoped domain exposed by the `power` PMU:

```rust,ignore
use micromeasure::LinuxPerfBackend;

g.backend(|| Box::new(LinuxPerfBackend::new().with_rapl_energy()))
    .bench("parse record", parse_record);
```

Micromeasure enables the energy counters around the complete sample, converts
the kernel count using its advertised Joule scale, and divides by the sample's
operation count. Each available domain produces three metrics:

- gross energy per operation in `µJ/op`
- gross energy for the sample in Joules
- average power during the sample in Watts

Hardware decides which domains exist. `energy-pkg` is the most common;
`energy-cores`, `energy-ram`, `energy-gpu`, and `energy-psys` appear only on
processors that implement them. `with_rapl_core_energy()` requests all of the
package/die domains and additionally sums the per-core `power_core` counters
available on some AMD systems. Because that needs one extra file descriptor
per advertised core, prefer `with_rapl_energy()` unless the core total is
useful.

RAPL is system-wide, not process-attributed. Package energy includes the
benchmark, other processes, kernel work, and the package's baseline idle
energy. The reported `µJ/op` is therefore a gross amortized estimate, not an
exclusive charge to the benchmark process. Use a quiet machine, keep workers
on the intended packages, and choose samples long enough for the workload's
energy delta to dominate counter quantization and background noise.

Unlike calling-thread PMU counters, RAPL naturally includes computation sent
to an existing Rayon or other external worker pool. A normal `bench(...)`
using `with_rapl_energy()` therefore covers the workers' package energy without
thread registration. It still cannot attribute that energy to individual
threads, roles, or even exclusively to the benchmark process.

For a coordinated concurrent group, the backend brackets the complete worker
window:

```rust,ignore
g.backend(|| Box::new(LinuxPerfBackend::new().with_rapl_energy()))
    .sample_duration(Duration::from_millis(100))
    .bench("readers and writers", &workers);
```

CPU PMU counters remain sourced from micromeasure's managed workers. RAPL
energy is reported only on the combined scenario result and is divided by the
total operations across those workers. For heterogeneous roles, interpret
that number only when a combined operation has a useful meaning; Joules per
sample and Watts remain valid regardless.

RAPL events require system-wide perf access, which is usually stricter than
calling-thread PMU access. Expect to need `CAP_PERFMON`, `CAP_SYS_ADMIN`, or a
`kernel.perf_event_paranoid` value below `1`. Missing PMUs or insufficient
permissions produce a one-time warning and the benchmark continues without
energy metrics.

The selected [`EnergyScope`](https://docs.rs/micromeasure/latest/micromeasure/enum.EnergyScope.html)
is persisted independently of `PmuScope`. Runs with no energy, package/die
RAPL, and package-plus-core RAPL are not treated as comparison-compatible.

## Benchmarks that dispatch to existing worker pools

The default [`LinuxPerfBackend`](https://docs.rs/micromeasure/latest/micromeasure/struct.LinuxPerfBackend.html) measures only the calling benchmark thread. If the measured function dispatches its real work to an already initialized Rayon or other worker pool, choose one of the explicit multi-thread scopes.

The simplest option snapshots all threads currently in the process before each measurement window:

```rust,ignore
g.backend(|| Box::new(LinuxPerfBackend::process_threads()))
    .bench("projection", projection_bench);
```

Initialize the pool before the measurement window. Threads created after `begin` are not included. Process-thread scope also counts unrelated runtime or service threads in the benchmark process, so keep the benchmark binary focused.

Each target thread needs one file descriptor per available individual counter. Very large pools can therefore require a higher `RLIMIT_NOFILE`.

For precise targeting, register only the worker threads. Rayon can run a registration closure once on every existing pool worker:

```rust,ignore
use micromeasure::{LinuxPerfBackend, LinuxPerfThreadSet};

let pmu_threads = LinuxPerfThreadSet::new();
pool.broadcast(|_| {
    pmu_threads.register_current();
});

let backend_threads = pmu_threads.clone();
runner.group::<ProjectionContext>("Projection", |g| {
    let backend_threads = backend_threads.clone();
    g.backend(move || {
        Box::new(LinuxPerfBackend::registered_threads(backend_threads.clone()))
    })
    .bench("projection", projection_bench);
});
```

Registered thread IDs are snapshotted before every sample. Stale IDs are ignored when counters cannot be opened. Calling-thread, process-thread, and registered-thread results use distinct measurement labels and are not comparison-compatible.

Linux perf inheritance is not used here: inheritance applies only to threads created after an event is opened, so it does not cover an existing Rayon pool, and it is incompatible with the grouped read format used for atomic counter groups.

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
PMU: scheduled=100.0%
```

- `scheduled=100.0%` — the representative PMU event was always scheduled. Ideal.
- `scheduled=80.0%` (or any value below 100%) — counters were multiplexed and values were scaled. Check the warning when scheduling is low.
- No scheduled line at all — PMU unavailable, timing-only fallback.

For GPU-domain benchmarks, the byline reads `host PMU (orchestration): scheduled=...` to remind you the CPU PMU describes the host thread, not the device. See [GPU Benchmarks](./gpu.md#measurement-domain).
