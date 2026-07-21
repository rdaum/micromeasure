# cuda_event_backend

**File:** `examples/cuda_event_backend.rs`
**Run (requires CUDA toolkit + GPU):**

```sh
cargo run --example cuda_event_backend --features cuda --release
```

## What it demonstrates

The feature-gated built-in `CudaEventBackend` with real default-stream GPU work. The benchmark body enqueues `cudaMemsetAsync` operations on the default stream; the backend records CUDA events around each sample and uses device elapsed time for latency/throughput, while also reporting host overhead.

This is the only example in the crate that links CUDA (`cudart`). Behind the `cuda` feature, `build.rs` searches `CUDA_HOME` / `CUDA_PATH` / `/usr/local/cuda*` for the library directory.

## Key code

```rust,ignore
#[link(name = "cudart")]
unsafe extern "C" {
    fn cudaMalloc(ptr: *mut *mut c_void, size: usize) -> CudaErrorCode;
    fn cudaFree(ptr: *mut c_void) -> CudaErrorCode;
    fn cudaMemsetAsync(
        ptr: *mut c_void,
        value: i32,
        count: usize,
        stream: CudaStreamHandle,
    ) -> CudaErrorCode;
}

struct CudaMemsetBench { device_buffer: *mut c_void }

impl BenchContext for CudaMemsetBench {
    fn prepare(_chunk_size: usize) -> Self {
        let mut device_buffer = null_mut();
        unsafe { check_cuda("cudaMalloc", cudaMalloc(&mut device_buffer, BYTES_PER_OP as usize)); }
        Self { device_buffer }
    }

    fn chunk_size() -> Option<usize> { Some(MEMSET_OPS_PER_SAMPLE) }
}

impl Drop for CudaMemsetBench {
    fn drop(&mut self) {
        if !self.device_buffer.is_null() {
            unsafe { let _ = cudaFree(self.device_buffer); }
        }
    }
}

fn memset_default_stream(ctx: &mut CudaMemsetBench, chunk_size: usize, chunk_num: usize) -> BenchSampleResult {
    for op in 0..chunk_size {
        let pattern = ((chunk_num + op) & 0xff) as i32;
        unsafe {
            check_cuda("cudaMemsetAsync", cudaMemsetAsync(
                ctx.device_buffer, pattern, BYTES_PER_OP as usize, null_mut(),
            ));
        }
    }
    BenchSampleResult::operations(chunk_size as u64)
}

benchmark_main!(|runner| {
    runner.group::<CudaMemsetBench>("CUDA/events", |g| {
        g.throughput(Throughput::bytes(BYTES_PER_OP))
            .measurement_domain(MeasurementDomain::Gpu)
            .backend(|| Box::new(CudaEventBackend::new(BYTES_PER_OP, FLOPS_PER_OP).unwrap()))
            .bench_sample("memset_async_default_stream", memset_default_stream);
    });
});
```

## What to look for

- `CudaEventBackend::new(BYTES_PER_OP, FLOPS_PER_OP)` — the backend is constructed with per-operation bytes and FLOPs. With `bytes_per_op > 0` it pushes `gpu_gib_s`; with `flops_per_op > 0` it pushes `gpu_tflops`. Here `FLOPS_PER_OP = 0`, so only bandwidth is reported.
- The `Measurement` row reads `timing + CUDA events`.
- `results.duration` is set to the **device event elapsed time** (from `cudaEventElapsedTime` between the start and stop events), so the latency/throughput columns describe the GPU work, not host orchestration.
- The `custom metrics:` table contains: `cuda_event_ms`, `host_overhead_ms`, and `gpu_gib_s` (since `bytes_per_op > 0`).
- `MeasurementDomain::Gpu` suppresses CPU-PMU diagnostics; `CudaEventBackend::emits_cpu_diagnostics() == false` reinforces that.
- `chunk_size() -> Some(MEMSET_OPS_PER_SAMPLE)` (16) bypasses calibration — GPU work, fixed shape.
- The `Drop` impl frees the device buffer. `prepare` allocates it once per sample; the context is dropped at the end of each sample.

## Limitations this example exhibits

- Default stream only. `cudaMemsetAsync(..., null_mut())` enqueues on the default stream; `CudaEventBackend` records on the default stream. Multi-stream overlap is invisible — see [GPU Benchmarking Sharp Edges](../gpu-sharp-edges.md#cudaeventbackend-uses-the-default-stream-only).
- On a CUDA error, the backend records `cuda_error_code` as a metric instead of crashing the run. Check the `custom metrics:` table for a `CUDA error` row if results look wrong.
- `cudaEventElapsedTime` returns `f32` with ~0.5 us resolution; very small kernels will show noise in `cuda_event_ms`.
