# throughput_units

**File:** `examples/throughput_units.rs`
**Run:**

```sh
cargo run --example throughput_units --release
```

## What it demonstrates

Declaring throughput with a domain-specific unit so the report renders `lines/s` instead of generic `ops/s`. One measured operation here represents 1000 lines of "compilation".

## Key code

```rust,ignore
const LINES_PER_COMPILATION: usize = 1_000;

fn compile_lines(_ctx: &mut NoContext, chunk_size: usize, _chunk_num: usize) {
    let mut checksum = black_box(0_u64);
    for chunk in 0..chunk_size {
        for line in 0..LINES_PER_COMPILATION {
            checksum = checksum.wrapping_add(black_box((chunk ^ line) as u64));
        }
    }
    black_box(checksum);
}

benchmark_main!(|runner| {
    runner.group::<NoContext>("Compiler", |g| {
        g.throughput(Throughput::per_operation(
            LINES_PER_COMPILATION as u64,
            "lines",
        ))
        .bench("compile_lines", compile_lines);
    });
});
```

## What to look for

- The throughput column shows `M lines/s` (auto-scaled with SI prefixes), not `ops/s`.
- The inner loop does `chunk_size * LINES_PER_COMPILATION` iterations, but the throughput is `(chunk_size * 1000) / duration`, because `Throughput::per_operation(1000, "lines")` multiplies `iterations` (= `chunk_size`) by 1000 before dividing by duration.
- This is the simplest way to report a rate in your domain's unit. If the per-operation amount is shape-dependent and not a static constant, see `operations_per_chunk()` in [Concepts](./concepts.md#benchcontext-and-nocontext).