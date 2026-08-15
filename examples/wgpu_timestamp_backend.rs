// Copyright 2026 Ryan Daum
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Demonstrates wgpu device-side timestamp timing through micromeasure's
//! operation-reported device-duration contract.
//!
//! The example owns its adapter, device, queue, command encoders,
//! submissions, synchronization, and resource lifetimes. Per sample it:
//!
//! 1. creates an application-owned `CommandEncoder`;
//! 2. begins a compute pass with `TimestampPair::writes` as the pass's
//!    `timestamp_writes` (requiring `wgpu::Features::TIMESTAMP_QUERY` only —
//!    `CommandEncoder::write_timestamp` is deliberately not used);
//! 3. encodes all dispatches belonging to the sample;
//! 4. ends the pass and appends `TimestampPair::encode_resolve_copy`;
//! 5. submits, requests the timestamp map, and synchronizes on its own
//!    submission (`Device::poll` on the `SubmissionIndex`);
//! 6. finishes the map with a bounded nonblocking read, validates, and
//!    converts the timestamp pair; and
//! 7. returns `BenchSampleResult::operations(n).with_primary_duration(...)`
//!    plus a `device_elapsed_ms` metric.
//!
//! `OperationReportedDeviceBackend` supplies the measurement label and the
//! `host_visible_ms` metric; the runner replaces the provisional host
//! duration with `primary_duration` for latency, throughput, calibration,
//! stability statistics, and persisted samples.
//!
//! Run on a machine with a wgpu-compatible adapter:
//!
//! ```sh
//! cargo run --features wgpu-example --example wgpu_timestamp_backend --release
//! ```
//!
//! Adapter selection is deterministic over wgpu-observable attributes:
//! timestamp-capable adapters rank above the rest, discrete above
//! integrated above virtual above other, with lexicographic tiebreaks.
//! Adapters with identical name/backend/vendor/device IDs are
//! indistinguishable through wgpu, so their relative order follows wgpu's
//! enumeration order; use `MICROMEASURE_WGPU_ADAPTER` for reproducible
//! captures on such systems. Software/fallback adapters (e.g. llvmpipe,
//! lavapipe, SwiftShader, WARP) are rejected by default; opt in with
//! `MICROMEASURE_WGPU_ALLOW_SOFTWARE=1`. On multi-adapter systems, select
//! explicitly with `MICROMEASURE_WGPU_ADAPTER=<index-into-ranked-list>`,
//! `<exact-name>`, or an unambiguous `<name-substring>`; ambiguous
//! selectors are an error. Environment flags accept
//! `1/true/yes/on` and `0/false/no/off`; unrecognized values are reported
//! and treated as disabled.
//!
//! Hardware self-check (no benchmark statistics, fails on any per-sample GPU
//! error instead of falling back to host timing):
//!
//! ```sh
//! MICROMEASURE_WGPU_HARDWARE_TEST=1 \
//! cargo run --features wgpu-example --example wgpu_timestamp_backend --release
//! ```

use std::env;
use std::process;
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use micromeasure::{
    BenchContext, BenchSampleResult, BenchmarkMainOptions, MeasurementDomain, MetricValue,
    OperationReportedDeviceBackend, ReportContext, Throughput,
    benchmark_options_with_default_suite, run_benchmark_main,
};

#[path = "support/wgpu_timestamp.rs"]
mod wgpu_timestamp;

use wgpu_timestamp::TimestampPair;

/// Pinned wgpu release. Keep in sync with the `wgpu` dependency in
/// `Cargo.toml`; wgpu does not expose its version at compile time.
const WGPU_VERSION: &str = "30.0";

const ALLOW_SOFTWARE_ENV: &str = "MICROMEASURE_WGPU_ALLOW_SOFTWARE";
const ADAPTER_SELECTOR_ENV: &str = "MICROMEASURE_WGPU_ADAPTER";
const HARDWARE_TEST_ENV: &str = "MICROMEASURE_WGPU_HARDWARE_TEST";

/// Compute-work geometry. One dispatch processes `ELEMENT_COUNT` `vec4<f32>`
/// elements with `INNER_ITERATIONS` fused multiply-adds each; the sample is a
/// single compute pass containing `DISPATCHES_PER_SAMPLE` dispatches.
const WORKGROUP_SIZE: u32 = 64;
const ELEMENT_COUNT: u32 = 1 << 20;
const INNER_ITERATIONS: u32 = 2048;
const DISPATCHES_PER_SAMPLE: usize = 4;
const WORKGROUP_COUNT: u32 = ELEMENT_COUNT / WORKGROUP_SIZE;

/// Bytes per element: one `vec4<f32>`.
const ELEMENT_BYTES: u64 = 16;
const BUFFER_BYTES: u64 = ELEMENT_COUNT as u64 * ELEMENT_BYTES;

/// Bytes moved per dispatch: the full working buffer is read and written
/// back in place, so each dispatch depends on the previous one.
const BYTES_PER_DISPATCH: u64 = BUFFER_BYTES * 2;

/// No adapter, software-only adapters, or an adapter without
/// `TIMESTAMP_QUERY`: the device-timed benchmark is unavailable. Distinct
/// exit code so scripts can tell "could not run" from "ran and failed".
const EXIT_UNAVAILABLE: i32 = 2;

struct AdapterMeta {
    name: String,
    backend: wgpu::Backend,
    device_type: wgpu::DeviceType,
    driver: String,
    driver_info: String,
    timestamp_query_advertised: bool,
    timestamp_period_ns: f32,
    /// Device limits relevant to the workload's buffers and dispatches.
    limits: wgpu::Limits,
}

/// Everything the application owns: device, queue, pipeline, buffers, and the
/// timestamp pair built from that same device and queue.
struct GpuState {
    device: wgpu::Device,
    queue: wgpu::Queue,
    pipeline: wgpu::ComputePipeline,
    bind_group: wgpu::BindGroup,
    /// The working buffer the shader accumulates in place, one element per
    /// invocation. `COPY_DST` for initialization, `COPY_SRC` so verification
    /// can stage it.
    data: wgpu::Buffer,
    /// `COPY_DST | MAP_READ` staging buffer for portable readback. wgpu does
    /// not permit `MAP_READ` on storage buffers without the non-portable
    /// `MAPPABLE_PRIMARY_BUFFERS` extension, so verification copies the
    /// working buffer here first.
    staging: wgpu::Buffer,
    timestamps: TimestampPair,
    meta: AdapterMeta,
}

impl GpuState {
    /// Encode one sample: a single compute pass with `dispatches` dispatches,
    /// bounded by timestamp writes, resolve, and copy. Submits and
    /// synchronizes the application's own submission, then reads the device
    /// elapsed time. No host-time fallback exists: any GPU-side failure is
    /// returned as an error.
    fn run_sample(&self, dispatches: usize) -> Result<Duration, String> {
        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("micromeasure wgpu sample"),
            });
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("micromeasure wgpu timed pass"),
                timestamp_writes: Some(self.timestamps.writes()),
            });
            pass.set_pipeline(&self.pipeline);
            pass.set_bind_group(0, &self.bind_group, &[]);
            for _ in 0..dispatches {
                pass.dispatch_workgroups(WORKGROUP_COUNT, 1, 1);
            }
        }
        self.timestamps.encode_resolve_copy(&mut encoder);
        let submission = self.queue.submit([encoder.finish()]);
        self.timestamps.map_async();
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map_err(|error| format!("device poll failed: {error}"))?;
        self.timestamps
            .finish_read()
            .map_err(|error| error.to_string())
    }

    /// Validate that the shader produced an observable result: copy the
    /// working buffer into the staging buffer, then verify that every checked
    /// element matches the expected growth envelope.
    ///
    /// The shader applies `acc = fma(acc, s, s - 1)` `INNER_ITERATIONS` times
    /// per dispatch with `s` the f32 value of `1.0000001`, which has the
    /// exact closed form `x_m = (x_0 + 1) * s^m - 1` for every element,
    /// independent of its input value. With `total_dispatches` accumulated
    /// dispatches the growth factor `(out + 1) / (in + 1)` must equal `s^m`
    /// within a small tolerance for f32 rounding. The envelope rejects NaN,
    /// infinities, a wrong dispatch count, and corrupted output.
    fn verify_output(&self, total_dispatches: u32) -> Result<(), String> {
        const GROWTH_TOLERANCE: f64 = 0.02;

        let scale = f64::from(1.000_000_1_f32);
        let iterations = u64::from(INNER_ITERATIONS) * u64::from(total_dispatches);
        let expected_growth = scale.powi(iterations as i32);
        let tolerance = (expected_growth - 1.0) * GROWTH_TOLERANCE;
        let growth_min = expected_growth - tolerance;
        let growth_max = expected_growth + tolerance;

        let mut encoder = self
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("micromeasure wgpu verify copy"),
            });
        encoder.copy_buffer_to_buffer(&self.data, 0, &self.staging, 0, BUFFER_BYTES);
        let submission = self.queue.submit([encoder.finish()]);
        self.device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map_err(|error| format!("device poll failed: {error}"))?;
        let slice = self.staging.slice(..);
        let (sender, receiver) = mpsc::sync_channel(1);
        slice.map_async(wgpu::MapMode::Read, move |result| {
            let _ = sender.send(result);
        });
        self.device
            .poll(wgpu::PollType::Poll)
            .map_err(|error| format!("device poll failed: {error}"))?;
        match receiver.try_recv() {
            Ok(Ok(())) => {}
            Ok(Err(error)) => return Err(format!("output buffer mapping failed: {error}")),
            Err(_) => return Err("output buffer map callback never arrived".to_string()),
        }
        let mapped = slice
            .get_mapped_range()
            .map_err(|error| format!("output buffer mapped range failed: {error}"))?;
        let indices = [0_u32, 1, 1024, ELEMENT_COUNT - 1];
        for index in indices {
            let offset = index as usize * ELEMENT_BYTES as usize;
            let output_x = f32::from_le_bytes(
                mapped[offset..offset + 4]
                    .try_into()
                    .expect("four output bytes"),
            );
            let input_x = index as f32;
            let growth = (f64::from(output_x) + 1.0) / (f64::from(input_x) + 1.0);
            if !output_x.is_finite()
                || !growth.is_finite()
                || !(growth_min..=growth_max).contains(&growth)
            {
                return Err(format!(
                    "shader output element {index} outside growth envelope: \
                     input {input_x}, output {output_x}, growth factor {growth} \
                     (expected {expected_growth})"
                ));
            }
        }
        drop(mapped);
        self.staging.unmap();
        Ok(())
    }
}

/// Per-sample context. The runner's factory supplies a fresh context (a
/// cloned handle to the shared application-owned GPU state) for each
/// warm-up, calibration, and measured sample.
struct WgpuComputeContext {
    state: Arc<GpuState>,
}

impl BenchContext for WgpuComputeContext {
    fn prepare(_chunk_size: usize) -> Self {
        unreachable!("this benchmark registers a factory; prepare is never called")
    }

    fn chunk_size() -> Option<usize> {
        Some(DISPATCHES_PER_SAMPLE)
    }
}

fn timestamped_compute(
    ctx: &mut WgpuComputeContext,
    chunk_size: usize,
    _chunk_num: usize,
) -> BenchSampleResult {
    let device_elapsed = ctx
        .state
        .run_sample(chunk_size)
        .unwrap_or_else(|error| panic!("wgpu sample failed: {error}"));
    BenchSampleResult::operations(chunk_size as u64)
        .with_primary_duration(device_elapsed)
        .push_metric(
            MetricValue::duration_ms("device_elapsed_ms", device_elapsed)
                .with_display_name("Device elapsed time"),
        )
}

fn shader_source() -> String {
    format!(
        "\
@group(0) @binding(0) var<storage, read_write> data: array<vec4<f32>>;

const SCALE: f32 = 1.0000001;
const OFFSET: f32 = SCALE - 1.0;

@compute @workgroup_size({WORKGROUP_SIZE})
fn main(@builtin(global_invocation_id) gid: vec3<u32>) {{
    let i = gid.x;
    if (i >= arrayLength(&data)) {{
        return;
    }}
    var acc = data[i];
    for (var k = 0u; k < {INNER_ITERATIONS}u; k = k + 1u) {{
        acc = fma(acc, vec4<f32>(SCALE), vec4<f32>(OFFSET));
    }}
    data[i] = acc;
}}
"
    )
}

fn initial_data() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(BUFFER_BYTES as usize);
    for index in 0..ELEMENT_COUNT {
        let value = [
            index as f32,
            (index + 1) as f32,
            (index + 2) as f32,
            (index + 3) as f32,
        ];
        for component in value {
            bytes.extend_from_slice(&component.to_le_bytes());
        }
    }
    bytes
}

fn adapter_names<'a>(adapters: impl IntoIterator<Item = &'a wgpu::Adapter>) -> String {
    adapters
        .into_iter()
        .map(|adapter| adapter.get_info().name)
        .collect::<Vec<_>>()
        .join(", ")
}

/// Parse an environment flag strictly: `1`/`true`/`yes`/`on` enable,
/// `0`/`false`/`no`/`off` (and empty) disable. Any other nonempty value is
/// reported and treated as disabled rather than silently enabled.
fn env_flag(name: &str) -> bool {
    match env::var(name) {
        Ok(value) => match value.trim().to_ascii_lowercase().as_str() {
            "1" | "true" | "yes" | "on" => true,
            "" | "0" | "false" | "no" | "off" => false,
            other => {
                eprintln!(
                    "warning: ignoring unrecognized value '{other}' for {name}; \
                     use 1/true/yes/on or 0/false/no/off"
                );
                false
            }
        },
        Err(_) => false,
    }
}

/// Stable, deterministic ordering over wgpu-observable attributes. Lower
/// sorts first: timestamp-capable before not, then device-type tier
/// (discrete < integrated < virtual < other < cpu), then a lexicographic
/// tiebreak on name/backend/vendor/device.
///
/// This is not strictly total: two physically distinct adapters with
/// identical name, backend, vendor, and device IDs are indistinguishable
/// through wgpu's `AdapterInfo`, so their relative order falls back to
/// wgpu's enumeration order. That is an unavoidable wgpu limitation; on
/// such systems use `MICROMEASURE_WGPU_ADAPTER` to force a selection and
/// record which adapter the capture used.
fn adapter_sort_key(
    adapter: &wgpu::Adapter,
    timestamp_capable: bool,
) -> (u8, u8, String, String, u32, u32) {
    let info = adapter.get_info();
    let type_tier = match info.device_type {
        wgpu::DeviceType::DiscreteGpu => 0,
        wgpu::DeviceType::IntegratedGpu => 1,
        wgpu::DeviceType::VirtualGpu => 2,
        wgpu::DeviceType::Other => 3,
        wgpu::DeviceType::Cpu => 4,
    };
    (
        u8::from(!timestamp_capable),
        type_tier,
        info.name.clone(),
        format!("{:?}", info.backend),
        info.vendor,
        info.device,
    )
}

fn ranked_adapters(adapters: &[wgpu::Adapter], allow_software: bool) -> Vec<&wgpu::Adapter> {
    let mut ranked: Vec<&wgpu::Adapter> = adapters
        .iter()
        .filter(|adapter| allow_software || adapter.get_info().device_type != wgpu::DeviceType::Cpu)
        .collect();
    ranked.sort_by_key(|adapter| {
        adapter_sort_key(
            adapter,
            adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY),
        )
    });
    ranked
}

/// Deterministic adapter selection.
///
/// - An explicit `MICROMEASURE_WGPU_ADAPTER` (index into the ranked list, or
///   a name) wins. A name must match exactly one adapter: an ambiguous
///   substring match is an error, never a silent choice.
/// - Otherwise the highest-ranked adapter is used: timestamp-capable before
///   not, discrete before integrated before virtual before other, with
///   lexicographic tiebreaks over wgpu-observable attributes.
///   Software/fallback adapters (`DeviceType::Cpu`) are excluded unless
///   `MICROMEASURE_WGPU_ALLOW_SOFTWARE` opts in.
///
/// The ranking is deterministic across runs, but adapters that wgpu reports
/// with identical name/backend/vendor/device IDs cannot be told apart
/// (see [`adapter_sort_key`]); their relative order is wgpu's enumeration
/// order. For reproducible captures on such systems, pass an explicit
/// `MICROMEASURE_WGPU_ADAPTER`.
fn select_adapter<'a>(
    adapters: &'a [wgpu::Adapter],
    selector: Option<&str>,
    allow_software: bool,
) -> Result<&'a wgpu::Adapter, String> {
    let ranked = ranked_adapters(adapters, allow_software);
    if ranked.is_empty() {
        return Err(format!(
            "only software/fallback adapters are available ({}); \
             set {ALLOW_SOFTWARE_ENV}=1 to opt in, or {ADAPTER_SELECTOR_ENV} to pick an adapter",
            adapter_names(adapters.iter())
        ));
    }
    if let Some(selector) = selector {
        if let Ok(index) = selector.parse::<usize>() {
            return ranked.get(index).copied().ok_or_else(|| {
                format!(
                    "adapter index {index} out of range ({}) ranked adapters: {}",
                    ranked.len(),
                    adapter_names(ranked.iter().copied())
                )
            });
        }
        let needle = selector.trim().to_ascii_lowercase();
        let exact: Vec<&wgpu::Adapter> = ranked
            .iter()
            .copied()
            .filter(|adapter| adapter.get_info().name.trim().to_ascii_lowercase() == needle)
            .collect();
        if let [adapter] = exact.as_slice() {
            return Ok(*adapter);
        }
        if exact.len() > 1 {
            return Err(format!(
                "adapter selector '{selector}' matches multiple adapters ({exact}); \
                 use an unambiguous name",
                exact = adapter_names(exact.iter().copied())
            ));
        }
        let matches: Vec<&wgpu::Adapter> = ranked
            .iter()
            .copied()
            .filter(|adapter| {
                adapter
                    .get_info()
                    .name
                    .to_ascii_lowercase()
                    .contains(&needle)
            })
            .collect();
        match matches.as_slice() {
            [adapter] => Ok(*adapter),
            [] => Err(format!(
                "no adapter matches '{selector}'; available: {}",
                adapter_names(ranked.iter().copied())
            )),
            _ => Err(format!(
                "adapter selector '{selector}' is ambiguous; matches: {}",
                adapter_names(matches.iter().copied())
            )),
        }
    } else {
        Ok(ranked[0])
    }
}

/// Setup failure modes. `Unavailable` means the device-timed benchmark
/// cannot exist on this machine (no adapter, software-only adapters); the
/// process reports it with the reserved unavailability exit code. `Failed`
/// means something that should have worked did not.
enum SetupError {
    Unavailable(String),
    Failed(String),
}

/// Adapter/device/timestamp preflight. Runs before any benchmark is
/// registered so timestamp availability is decided before warm-up or
/// sampling. Returns `Ok(None)` when timestamp queries are unavailable —
/// the caller reports benchmark unavailability, never host-timing fallback.
fn setup() -> Result<Option<Arc<GpuState>>, SetupError> {
    let allow_software = env_flag(ALLOW_SOFTWARE_ENV);
    let selector = env::var(ADAPTER_SELECTOR_ENV).ok();

    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    let adapters = pollster::block_on(instance.enumerate_adapters(wgpu::Backends::all()));
    if adapters.is_empty() {
        return Err(SetupError::Unavailable(
            "no wgpu adapters found on this machine".to_string(),
        ));
    }

    let ranked = ranked_adapters(&adapters, allow_software);
    if ranked.is_empty() {
        // Adapters exist but every one is software/fallback: benchmark
        // unavailability (exit 2), not a setup failure (exit 1).
        return Err(SetupError::Unavailable(format!(
            "only software/fallback adapters are available ({}); \
             set {ALLOW_SOFTWARE_ENV}=1 to opt in, or {ADAPTER_SELECTOR_ENV} to pick an adapter",
            adapter_names(adapters.iter())
        )));
    }
    eprintln!("wgpu adapters (ranked, first will be used):");
    for (position, adapter) in ranked.iter().enumerate() {
        let info = adapter.get_info();
        eprintln!(
            "  [{position}] {} ({:?}, {:?}){}",
            info.name,
            info.backend,
            info.device_type,
            if adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY) {
                ", TIMESTAMP_QUERY"
            } else {
                ""
            }
        );
    }
    let adapter = select_adapter(&adapters, selector.as_deref(), allow_software)
        .map_err(SetupError::Failed)?;
    let info = adapter.get_info();

    eprintln!("wgpu adapter metadata:");
    eprintln!("  name: {}", info.name);
    eprintln!("  backend: {:?}", info.backend);
    eprintln!("  device_type: {:?}", info.device_type);
    eprintln!("  driver: {}", info.driver);
    eprintln!("  driver_info: {}", info.driver_info);

    let timestamp_query_advertised = adapter.features().contains(wgpu::Features::TIMESTAMP_QUERY);
    eprintln!(
        "  timestamp_query: {}",
        if timestamp_query_advertised {
            "advertised; requesting"
        } else {
            "not advertised"
        }
    );
    let mut requested_features = wgpu::Features::empty();
    if timestamp_query_advertised {
        requested_features |= wgpu::Features::TIMESTAMP_QUERY;
    }

    let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
        label: Some("micromeasure wgpu_timestamp_backend"),
        required_features: requested_features,
        required_limits: wgpu::Limits::default(),
        experimental_features: wgpu::ExperimentalFeatures::disabled(),
        memory_hints: wgpu::MemoryHints::default(),
        trace: wgpu::Trace::Off,
    }))
    .map_err(|error| SetupError::Failed(format!("device request failed: {error}")))?;

    let timestamps = TimestampPair::new(&device, &queue)
        .map_err(|error| SetupError::Failed(error.to_string()))?;
    let Some(timestamps) = timestamps else {
        eprintln!(
            "this adapter does not advertise TIMESTAMP_QUERY; \
             the device-timed benchmark is unavailable here"
        );
        return Ok(None);
    };
    eprintln!("  timestamp_period_ns: {}", queue.get_timestamp_period());

    let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
        label: Some("micromeasure wgpu compute shader"),
        source: wgpu::ShaderSource::Wgsl(shader_source().into()),
    });
    let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
        label: Some("micromeasure wgpu storage layout"),
        entries: &[wgpu::BindGroupLayoutEntry {
            binding: 0,
            visibility: wgpu::ShaderStages::COMPUTE,
            ty: wgpu::BindingType::Buffer {
                ty: wgpu::BufferBindingType::Storage { read_only: false },
                has_dynamic_offset: false,
                min_binding_size: None,
            },
            count: None,
        }],
    });
    let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
        label: Some("micromeasure wgpu pipeline layout"),
        bind_group_layouts: &[Some(&bind_group_layout)],
        immediate_size: 0,
    });
    let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
        label: Some("micromeasure wgpu compute pipeline"),
        layout: Some(&pipeline_layout),
        module: &shader,
        entry_point: Some("main"),
        compilation_options: Default::default(),
        cache: None,
    });
    let data = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("micromeasure wgpu data"),
        size: BUFFER_BYTES,
        usage: wgpu::BufferUsages::STORAGE
            | wgpu::BufferUsages::COPY_DST
            | wgpu::BufferUsages::COPY_SRC,
        mapped_at_creation: false,
    });
    let staging = device.create_buffer(&wgpu::BufferDescriptor {
        label: Some("micromeasure wgpu staging"),
        size: BUFFER_BYTES,
        usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
        mapped_at_creation: false,
    });
    queue.write_buffer(&data, 0, &initial_data());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("micromeasure wgpu bind group"),
        layout: &bind_group_layout,
        entries: &[wgpu::BindGroupEntry {
            binding: 0,
            resource: data.as_entire_binding(),
        }],
    });
    let timestamp_period_ns = queue.get_timestamp_period();
    let limits = device.limits();
    eprintln!(
        "  limits: max_buffer_size={}, max_storage_buffer_binding_size={}, \
         max_compute_workgroups_per_dimension={}, max_compute_workgroup_size_x={}, \
         max_compute_invocations_per_workgroup={}",
        limits.max_buffer_size,
        limits.max_storage_buffer_binding_size,
        limits.max_compute_workgroups_per_dimension,
        limits.max_compute_workgroup_size_x,
        limits.max_compute_invocations_per_workgroup
    );

    Ok(Some(Arc::new(GpuState {
        device,
        queue,
        pipeline,
        bind_group,
        data,
        staging,
        timestamps,
        meta: AdapterMeta {
            name: info.name,
            backend: info.backend,
            device_type: info.device_type,
            driver: info.driver,
            driver_info: info.driver_info,
            timestamp_query_advertised,
            timestamp_period_ns,
            limits,
        },
    })))
}

fn build_report_context(state: &GpuState, options: &BenchmarkMainOptions) -> ReportContext {
    let meta = &state.meta;
    let runtime = &options.runtime;
    let mut context = ReportContext::new(format!(
        "micromeasure wgpu_timestamp_backend example {}",
        env!("CARGO_PKG_VERSION")
    ))
    .with_environment("adapter_name", meta.name.clone())
    .with_environment("adapter_backend", format!("{:?}", meta.backend))
    .with_environment("adapter_device_type", format!("{:?}", meta.device_type))
    .with_environment("wgpu_version", WGPU_VERSION.to_string())
    .with_environment(
        "timestamp_query",
        if meta.timestamp_query_advertised {
            "advertised + requested"
        } else {
            "unsupported"
        }
        .to_string(),
    )
    .with_environment(
        "timestamp_period_ns",
        format!("{}", meta.timestamp_period_ns),
    )
    .with_environment("workgroup_size", WORKGROUP_SIZE.to_string())
    .with_environment("workgroup_count", WORKGROUP_COUNT.to_string())
    .with_environment("element_count", ELEMENT_COUNT.to_string())
    .with_environment("inner_iterations", INNER_ITERATIONS.to_string())
    .with_environment("dispatches_per_sample", DISPATCHES_PER_SAMPLE.to_string())
    .with_environment("buffer_bytes", BUFFER_BYTES.to_string())
    .with_environment("bytes_per_dispatch", BYTES_PER_DISPATCH.to_string())
    .with_environment("max_buffer_size", meta.limits.max_buffer_size.to_string())
    .with_environment(
        "max_storage_buffer_binding_size",
        meta.limits.max_storage_buffer_binding_size.to_string(),
    )
    .with_environment(
        "max_compute_workgroups_per_dimension",
        meta.limits.max_compute_workgroups_per_dimension.to_string(),
    )
    .with_environment(
        "max_compute_workgroup_size_x",
        meta.limits.max_compute_workgroup_size_x.to_string(),
    )
    .with_environment(
        "max_compute_invocations_per_workgroup",
        meta.limits
            .max_compute_invocations_per_workgroup
            .to_string(),
    )
    .with_environment(
        "warmup_duration_ms",
        runtime.warm_up_duration.as_millis().to_string(),
    )
    .with_environment(
        "benchmark_duration_ms",
        runtime.benchmark_duration.as_millis().to_string(),
    )
    .with_environment("min_samples", runtime.min_samples.to_string())
    .with_environment("max_samples", runtime.max_samples.to_string());
    if !meta.driver.is_empty() {
        context = context.with_environment("adapter_driver", meta.driver.clone());
    }
    if !meta.driver_info.is_empty() {
        context = context.with_environment("adapter_driver_info", meta.driver_info.clone());
    }
    context
}

/// Hardware self-check enabled by `MICROMEASURE_WGPU_HARDWARE_TEST=1`.
///
/// Runs real compute work through the timing path and verifies that: every
/// sample executes nonzero work and returns a positive device duration; the
/// device duration does not exceed its enclosing synchronized host-visible
/// interval (allowing a small documented tolerance for the two independent
/// clocks and host timer granularity); the shader produces an observable
/// result matching its closed-form growth envelope (which also rejects NaN
/// and verifies the executed dispatch count); and the timing pair survives
/// map/unmap reuse across samples. Any per-sample GPU error fails the check
/// — there is no host-time fallback.
fn hardware_selfcheck(state: &GpuState) -> Result<(), String> {
    const SELFCHECK_SAMPLES: usize = 8;
    /// Device and host durations come from independent clocks; allow a small
    /// interval so the check catches gross misconfiguration (e.g. a wrong
    /// timestamp period) without failing on clock drift or timer granularity.
    const HOST_INTERVAL_TOLERANCE: Duration = Duration::from_millis(1);

    eprintln!("running wgpu timestamp hardware self-check ({SELFCHECK_SAMPLES} samples)");
    for sample in 0..SELFCHECK_SAMPLES {
        let mut encoder = state
            .device
            .create_command_encoder(&wgpu::CommandEncoderDescriptor {
                label: Some("micromeasure wgpu self-check"),
            });
        let host_start = Instant::now();
        {
            let mut pass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor {
                label: Some("micromeasure wgpu self-check pass"),
                timestamp_writes: Some(state.timestamps.writes()),
            });
            pass.set_pipeline(&state.pipeline);
            pass.set_bind_group(0, &state.bind_group, &[]);
            for _ in 0..DISPATCHES_PER_SAMPLE {
                pass.dispatch_workgroups(WORKGROUP_COUNT, 1, 1);
            }
        }
        state.timestamps.encode_resolve_copy(&mut encoder);
        let submission = state.queue.submit([encoder.finish()]);
        state.timestamps.map_async();
        state
            .device
            .poll(wgpu::PollType::Wait {
                submission_index: Some(submission),
                timeout: None,
            })
            .map_err(|error| format!("device poll failed: {error}"))?;
        let device_elapsed = state
            .timestamps
            .finish_read()
            .map_err(|error| error.to_string())?;
        let host_elapsed = host_start.elapsed();
        if device_elapsed.is_zero() {
            return Err(format!("sample {sample}: zero device duration"));
        }
        if device_elapsed > host_elapsed + HOST_INTERVAL_TOLERANCE {
            return Err(format!(
                "sample {sample}: device duration {device_elapsed:?} exceeds \
                 host-visible interval {host_elapsed:?} + tolerance"
            ));
        }
        eprintln!(
            "  sample {sample}: device {:.3} ms, host-visible {:.3} ms",
            device_elapsed.as_secs_f64() * 1000.0,
            host_elapsed.as_secs_f64() * 1000.0
        );
    }
    state.verify_output((SELFCHECK_SAMPLES * DISPATCHES_PER_SAMPLE) as u32)?;
    eprintln!("hardware self-check passed");
    Ok(())
}

fn main() {
    let state = match setup() {
        Ok(Some(state)) => state,
        Ok(None) => {
            eprintln!(
                "device-timed benchmark unavailable: the selected adapter does not \
                 advertise TIMESTAMP_QUERY"
            );
            process::exit(EXIT_UNAVAILABLE);
        }
        Err(SetupError::Unavailable(reason)) => {
            eprintln!("device-timed benchmark unavailable: {reason}");
            process::exit(EXIT_UNAVAILABLE);
        }
        Err(SetupError::Failed(error)) => {
            eprintln!("wgpu setup failed: {error}");
            process::exit(1);
        }
    };

    if env_flag(HARDWARE_TEST_ENV) {
        match hardware_selfcheck(&state) {
            Ok(()) => process::exit(0),
            Err(error) => {
                eprintln!("hardware self-check FAILED: {error}");
                process::exit(1);
            }
        }
    }

    let options = benchmark_options_with_default_suite(
        BenchmarkMainOptions {
            report_context: Some(build_report_context(
                &state,
                &BenchmarkMainOptions::default(),
            )),
            ..BenchmarkMainOptions::default()
        },
        "wgpu_timestamp",
    );

    let _ = run_benchmark_main(options, |runner| {
        let factory = {
            let state = Arc::clone(&state);
            move || WgpuComputeContext {
                state: Arc::clone(&state),
            }
        };
        runner.group::<WgpuComputeContext>("wgpu/timestamps", |g| {
            g.throughput(Throughput::bytes(BYTES_PER_DISPATCH))
                .measurement_domain(MeasurementDomain::Gpu)
                .backend(|| Box::new(OperationReportedDeviceBackend::new()))
                .factory(&factory)
                .bench_sample("compute_pass_dispatches", timestamped_compute);
        });
    });
}
