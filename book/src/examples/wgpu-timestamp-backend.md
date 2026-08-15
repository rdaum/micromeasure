# wgpu_timestamp_backend

**Files:** `examples/wgpu_timestamp_backend.rs`, `examples/support/wgpu_timestamp.rs`
**Run (requires a wgpu-compatible adapter, e.g. Vulkan/DX12/Metal/GLES):**

```sh
cargo run --features wgpu-example --example wgpu_timestamp_backend --release
```

## What it demonstrates

wgpu device-side timestamp timing reported through micromeasure's
operation-reported device-duration contract. There is **no new measurement
backend** here: the application owns its adapter, device, queue, command
encoders, submissions, synchronization, and resource lifetimes, and reports
the measured device duration per sample with
`BenchSampleResult::with_primary_duration`. `OperationReportedDeviceBackend`
supplies the measurement label and the `host_visible_ms` metric.

This exists as a reference integration because wgpu timestamp writes are
encoded as part of the GPU work: a host-side `MeasurementBackend::begin` /
`end` pair cannot place them around an opaque closure the way CUDA events
can.

## Exact timing boundaries

Per sample the application context:

1. creates an application-owned `CommandEncoder`;
2. begins a compute pass whose `ComputePassDescriptor::timestamp_writes`
   come from `TimestampPair::writes()` (one query at pass start, one at pass
   end — `wgpu::Features::TIMESTAMP_QUERY` only; `CommandEncoder::
   write_timestamp` is not used);
3. encodes all four dispatches belonging to the sample in that one pass;
4. ends the pass and appends `TimestampPair::encode_resolve_copy`, which
   records `resolve_query_set` into a `QUERY_RESOLVE | COPY_SRC` buffer and
   copies it into a separate `COPY_DST | MAP_READ` buffer (portable WebGPU
   requires the two-buffer split);
5. submits, then requests the timestamp map (`TimestampPair::map_async`)
   and blocks on its own submission with
   `Device::poll(PollType::Wait { submission_index, .. })` — the exact
   submission, never a global wait;
6. finishes the map with `TimestampPair::finish_read()`, which is
   nonblocking: it performs at most one bounded `Poll` drain and reports
   `MapNotReady` instead of waiting, so the application (not the helper)
   owns all synchronization.

So:

- **`device_elapsed_ms`** (the benchmark's primary duration) spans exactly
  the compute pass: after the pass-start timestamp, before the pass-end
  timestamp. Host encoding, submission, and synchronization are excluded.
- **`host_visible_ms`** (pushed by `OperationReportedDeviceBackend`) spans
  the whole closure: host encoder creation through submission wait and
  timestamp readback.

The runner then uses the primary duration for latency, throughput,
calibration, stability statistics, and persisted raw samples. No host-time
fallback exists anywhere in the sample path: any per-sample GPU or mapping
error panics the benchmark with the error message.

## Workload

One dispatch processes `ELEMENT_COUNT = 1 << 20` `vec4<f32>` elements in a
single 16 MiB working buffer with `INNER_ITERATIONS = 2048` fused
multiply-adds per element. The shader accumulates in place, so each dispatch
depends on the previous one; a sample is a single compute pass containing
`DISPATCHES_PER_SAMPLE = 4` such dispatches. `chunk_size()` is fixed, so the
runner bypasses CPU-style chunk calibration and warms up at the real shape.
Throughput is `Throughput::bytes(32 MiB)` — the full buffer read plus write
back per dispatch; this is a compute-bound kernel, so do not expect peak
memory bandwidth.

## Availability behavior

Timestamp availability is decided once, before any benchmark is registered:

- The example requests `TIMESTAMP_QUERY` only when the selected adapter
  advertises it, and `TimestampPair::new` confirms the device has the
  feature.
- If no adapter exists, or only software/fallback adapters exist, or
  `TIMESTAMP_QUERY` is unsupported, the example prints the reason and exits
  with status 2 (distinct from run failure, which exits 1). It never
  silently substitutes host timing inside the device-timed population.

## Adapter selection and reproducibility

- Software/fallback adapters (`DeviceType::Cpu` — llvmpipe, lavapipe,
  SwiftShader, WARP) are rejected by default. Opt in with
  `MICROMEASURE_WGPU_ALLOW_SOFTWARE=1` (environment flags accept
  `1/true/yes/on` and `0/false/no/off`; unrecognized values are reported and
  treated as disabled).
- On multi-adapter systems selection is deterministic over
  wgpu-observable attributes: adapters are ranked timestamp-capable first,
  then discrete, integrated, virtual, other, with lexicographic tiebreaks
  on name/backend/vendor/device; the ranked list is printed before
  selection. Adapters with identical name/backend/vendor/device IDs are
  indistinguishable through wgpu, so their relative order follows wgpu's
  enumeration order — use `MICROMEASURE_WGPU_ADAPTER` for reproducible
  captures on such systems. Override selection with
  `MICROMEASURE_WGPU_ADAPTER=<index-into-ranked-list>`, `<exact-name>`, or an
  unambiguous `<name-substring>`. An ambiguous selector matching several
  adapters is an error, never a silent choice.
- The selected adapter's name, backend, driver, driver_info, device type,
  timestamp feature state, timestamp period, and the device limits relevant
  to the workload (`max_buffer_size`, `max_storage_buffer_binding_size`,
  `max_compute_workgroups_per_dimension`, `max_compute_workgroup_size_x`,
  `max_compute_invocations_per_workgroup`) are printed to stderr and
  persisted in the report context (`environment` map), along with wgpu
  version, shader/workgroup geometry, dispatch count, buffer sizes,
  bytes-per-dispatch, and the warm-up/sample policy. The report also records
  the micromeasure git commit via the standard report machinery.

## Hardware self-check

```sh
MICROMEASURE_WGPU_HARDWARE_TEST=1 \
cargo run --features wgpu-example --example wgpu_timestamp_backend --release
```

Runs eight samples through the exact timing path and verifies:

- every sample returns a positive device duration;
- device duration does not exceed its enclosing synchronized host-visible
  interval (1 ms documented tolerance for the independent device/host
  clocks and host timer granularity);
- the shader output matches its closed-form growth envelope
  `(out + 1) / (in + 1) ≈ s^(INNER_ITERATIONS · total_dispatches)` computed
  from the f32-rounded shader constants — this rejects NaN, infinities, a
  wrong executed dispatch count, and corrupted output, not just "grew";
- the timing pair survives map/unmap reuse across samples.

Any per-sample GPU error fails the check — there is no fallback path.

## Key code

The timing utility (private to the example — copy it beside your own
operation code; it is intentionally not part of micromeasure's public API):

```rust,ignore
struct TimestampPair {
    device: wgpu::Device,
    query_set: wgpu::QuerySet,     // QueryType::Timestamp, two entries
    resolve: wgpu::Buffer,         // QUERY_RESOLVE | COPY_SRC, 16 bytes
    readback: wgpu::Buffer,        // COPY_DST | MAP_READ, 16 bytes
    timestamp_period_ns: f32,
    // map callback channel
}

impl TimestampPair {
    fn new(device: &wgpu::Device, queue: &wgpu::Queue)
        -> Result<Option<Self>, TimestampError>;
    fn writes(&self) -> wgpu::ComputePassTimestampWrites<'_>;
    fn encode_resolve_copy(&self, encoder: &mut wgpu::CommandEncoder);
    fn map_async(&self);   // call before the application's submission wait
    fn finish_read(&self) -> Result<Duration, TimestampError>;  // nonblocking
}
```

The canonical per-sample flow (in `GpuState::run_sample`):

```rust,ignore
let submission = queue.submit([encoder.finish()]);
timestamps.map_async();                      // map request rides the wait
device.poll(wgpu::PollType::Wait {
    submission_index: Some(submission),      // the application's own wait
    timeout: None,
})?;
let device_elapsed = timestamps.finish_read()?;  // never blocks, never waits
```

The per-sample bench closure:

```rust,ignore
fn timestamped_compute(ctx: &mut WgpuComputeContext, chunk_size: usize, _chunk_num: usize)
    -> BenchSampleResult
{
    let device_elapsed = ctx.state.run_sample(chunk_size)
        .unwrap_or_else(|error| panic!("wgpu sample failed: {error}"));
    BenchSampleResult::operations(chunk_size as u64)
        .with_primary_duration(device_elapsed)
        .push_metric(
            MetricValue::duration_ms("device_elapsed_ms", device_elapsed)
                .with_display_name("Device elapsed time"),
        )
}
```

Registration uses a factory so every warm-up, calibration, and measured
sample context shares the one application-owned device and pipeline:

```rust,ignore
runner.group::<WgpuComputeContext>("wgpu/timestamps", |g| {
    g.throughput(Throughput::bytes(BYTES_PER_DISPATCH))
        .measurement_domain(MeasurementDomain::Gpu)
        .backend(|| Box::new(OperationReportedDeviceBackend::new()))
        .factory(&factory)
        .bench_sample("compute_pass_dispatches", timestamped_compute);
});
```

## What to look for

- The `Measurement` row reads `operation-reported device timing`.
- The latency/throughput columns derive from the device pass duration, not
  the host wall clock.
- The `custom metrics:` table contains `device_elapsed_ms` (bench) and
  `host_visible_ms` (backend). `host_visible_ms` is expected to be a few
  milliseconds larger — encoding, submission, and readback.
- CPU PMU rows are absent: `OperationReportedDeviceBackend` returns
  `PmuCounterProfile::None` and `emits_cpu_diagnostics() == false`, and the
  group is `MeasurementDomain::Gpu`.
- The persisted JSON report's `context.environment` map carries the full
  adapter and workload metadata listed above.

## Limitations this example exhibits

- **Synchronous readback.** Every sample submits, blocks on its own
  submission, and reads back. Correct for a microbenchmark; not an execution
  model recommendation. A production runtime that pipelines samples keeps
  the same `map_async` + `finish_read` split (both never wait on the
  device) while driving its own submission semantics.
- **Pass-boundary timestamps only.** The measured region is one compute
  pass. Per-dispatch query allocation, `write_timestamp` inside encoders,
  render-pass timing, multi-queue correlation, and cross-device timing are
  out of scope — see [GPU Benchmarking Sharp Edges](../gpu-sharp-edges.md).
- **The example owns the wgpu version.** The utility is compiled against
  wgpu 30.0.0 and its types are not part of micromeasure's public API.
  Applications on a different wgpu version should copy the ~200-line support
  module beside their own code rather than import it.
