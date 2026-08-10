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

## Counter profiles, scheduling, and fallback

The default `LinuxPerfBackend` requests the full nine-event counter profile.
Many CPUs expose fewer programmable counters than that, so Linux must
permanently multiplex the events: a long sample improves rotation and scaling,
but cannot make the scheduled percentage approach 100% when nine events must
share four or six physical slots.

When cycles, instructions, branches, and branch misses are sufficient, select
the compact four-event profile:

```rust,ignore
use micromeasure::LinuxPerfBackend;

g.backend(|| Box::new(LinuxPerfBackend::new().with_compact_counters()))
    .bench("dispatch", dispatch);
```

The compact profile commonly fits without multiplexing and still provides IPC
and branch-miss statistics. It intentionally omits cache references, cache
misses, L1I misses, and frontend/backend stall counters. Profile selection is
independent of RAPL, so energy-capable runs can use compact counters or no CPU
counters at all:

```rust,ignore
LinuxPerfBackend::new()
    .with_compact_counters()
    .with_rapl_energy();

LinuxPerfBackend::new()
    .without_cpu_counters()
    .with_rapl_energy();
```

Full, compact, and no-counter results have distinct persisted comparison
identities. Select a profile with `with_counter_profile(PmuCounterProfile)`
when configuration needs to be data-driven.

Even when PMU access is available, the kernel may let you open a perf-event **group** that is too large to schedule on the available hardware registers. Such a group reads as zero even though creation and activation succeeded. `LinuxPerfBackend` handles this:

1. During standard benchmark calibration, try a perf-event group covering the full counter set.
2. If creation or activation fails, or the group obtains no usable scheduled window, remember that result and use individual counters for subsequent samples.
3. Micromeasure-managed concurrent workers and external multi-thread scopes start with individual counters. Managed workers are recreated for every sample, and probing an oversized group independently on every external worker would waste the calibration window.
4. Scale each multiplexed value using its own `time_running / time_enabled` values.

The PMU `scheduled` line reports `time_running / time_enabled` as a percentage. Below 100% means the kernel multiplexed the counters because the PMU could not count all of them at once. It does **not** report what fraction of a multi-threaded workload was observed. For individual counters, micromeasure reports the least-scheduled available event as a conservative quality indicator. At least 90% is treated as direct measurement. A 25–90% window with at least 10 ms of counter running time is reported as a usable multiplexed/scaled estimate. A lower percentage or shorter running time produces an unreliability warning.

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

Micromeasure enables the energy counters around each complete sample and
converts the kernel count using its advertised Joule scale. Each available
domain produces three per-sample metrics:

- gross energy per operation in `µJ/op`
- gross energy for the sample in Joules
- average power during the sample in Watts

It also reports a `RAPL aggregate` section. Those values sum energy,
operations, and active measurement time across all contributing samples before
computing aggregate µJ/op and Watts. This preserves per-sample latency and
throughput distributions while giving tiny operations a longer, higher-signal
energy interval. It sums the measured sample windows; it does not include the
unmeasured setup gaps between them.

Very short energy windows can be smaller than the effective RAPL update
resolution. They can therefore produce a zero delta, or an implausible power
spike when one coarse energy increment is divided by a few milliseconds.
Micromeasure warns when an observed RAPL sample is shorter than 10 ms, and when
the aggregate contains less than 100 ms of active measurement. Prefer samples
around 50–200 ms (100 ms is a good starting point) and enough samples to cover
at least one second. For a fixed-chunk benchmark, increase the chunk size or
`max_samples`; `benchmark_duration` remains a target and `max_samples` remains
an explicit upper bound.

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

## System memory bandwidth

Some Intel server processors expose integrated memory-controller counters as
Linux `uncore_imc_*` perf PMUs. The Linux backend automatically probes for
symbolic `cas_count_read` and `cas_count_write` events and collects gross
system memory bandwidth when every advertised target is usable:

```rust,ignore
use micromeasure::LinuxPerfBackend;

g.backend(|| Box::new(LinuxPerfBackend::registered_threads(worker_threads.clone())))
.bench("streaming transform", streaming_transform);
```

An unsupported or permission-restricted automatic probe is quiet and runs
only once per backend. Use `.with_memory_bandwidth()` to make the request
explicit and receive unavailable/partial diagnostics, or
`.without_memory_bandwidth()` to skip discovery entirely.

The option composes with calling-thread, process-thread, registered-thread,
compact-counter, no-CPU-counter, and RAPL modes. IMC events use separate
uncore hardware and do not consume the programmable slots used by the normal
CPU PMU profile.

Micromeasure enumerates every `uncore_imc_*` directory and opens one read/write
event pair for every CPU in that PMU's advertised `cpumask`. On multi-socket
machines those CPUs are the kernel's representative CPUs for each package;
opening the Cartesian product of PMU directories and advertised CPUs covers
every exposed channel/package instance without duplicating the event on every
ordinary CPU. Each event's own sysfs `scale` and `unit` are converted to bytes,
and each raw count is adjusted by its own `time_enabled / time_running` ratio.

A complete sample reports these custom metrics:

- `dram_read_bytes_per_op`, `dram_write_bytes_per_op`, and
  `dram_total_bytes_per_op`
- `dram_read_gib_s`, `dram_write_gib_s`, and `dram_total_gib_s`
- `dram_pmu_scheduled_percent`, using the least-scheduled read or write event
- `dram_imc_coverage_percent`

The counters measure all memory-controller traffic during the sample window:
benchmark workers, other processes, and the kernel. The byte-per-operation
values are therefore gross amortized traffic, not process-exclusive traffic.
Use a quiet machine and samples long enough for the benchmark's traffic to
dominate background activity.

Whole-system byte and bandwidth metrics are emitted only when all advertised
IMC targets return both usable read and write counters. If a PMU, symbolic
event, scale/unit, CPU target, permission, or scheduled window is missing,
micromeasure warns once and continues. Partial samples retain coverage and
scheduling metrics but omit the partial byte sum so it cannot be mistaken for
a system total. A zero-running-time event is unavailable, not zero bandwidth.

[`MemoryBandwidthScope`](https://docs.rs/micromeasure/latest/micromeasure/enum.MemoryBandwidthScope.html)
is persisted as `none`, `system_unavailable`, `system_partial`, or
`system_complete`. Automatic probe misses remain `none`; an explicit request
records `system_unavailable`. These states are distinct comparison identities. Richer
PCM features such as per-channel/rank output, persistent-memory traffic,
partial writes, and theoretical peak bandwidth are outside this portable
symbolic-event mode.

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

The full profile and the low-level `PerfCounters` type collect:

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
