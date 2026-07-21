# factory_builder

**File:** `examples/factory_builder.rs`
**Run:**

```sh
cargo run --example factory_builder --release
```

## What it demonstrates

Combining fluent `factory(...)` with `Throughput` configuration. A `BenchContext` holds pre-allocated input, and a `factory` closure overrides the default `T::prepare(...)` constructor. The factory is called once per sample (and during warm-up) to produce a fresh context — same lifecycle as `prepare`, not a cross-sample reuse mechanism.

## Key code

```rust,ignore
struct ParseContext { input: Vec<u8> }

impl BenchContext for ParseContext {
    fn prepare(_chunk_size: usize) -> Self {
        Self { input: vec![b'x'; INPUT_BYTES] }
    }
}

fn parse_config(ctx: &mut ParseContext, chunk_size: usize, _chunk_num: usize) {
    let mut checksum = black_box(0_u64);
    for _ in 0..chunk_size {
        for &byte in &ctx.input {
            checksum = checksum.wrapping_add(black_box(byte as u64));
        }
    }
    black_box(checksum);
}

benchmark_main!(|runner| {
    let factory = || ParseContext { input: vec![b'x'; INPUT_BYTES] };

    runner.group::<ParseContext>("Parser", |g| {
        g.throughput(Throughput::per_operation(INPUT_BYTES as u64, "bytes"))
            .factory(&factory)
            .bench("parse_config", parse_config);
    });
});
```

## What to look for

- `g.factory(&factory)` overrides the default `ParseContext::prepare(...)` constructor. The factory is called once per sample (and during warm-up) to produce a fresh context.
- The example declares both `BenchContext::prepare` and a `factory`. When both are present, the `factory` takes precedence for the registered group. `prepare` is still the trait default and would be used by any group that does not supply a `factory`.
- Throughput is `bytes/s` because each operation processes `INPUT_BYTES` (4096) bytes.
- This is the standard shape for benches that need pre-allocated or pre-parsed input that should not be re-created inside the measured loop.
