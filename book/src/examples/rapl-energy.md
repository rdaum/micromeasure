# rapl_energy

Source: [`examples/rapl_energy.rs`](https://github.com/rdaum/micromeasure/blob/main/examples/rapl_energy.rs)

Run it on Linux with:

```sh
cargo run --release --example rapl_energy
```

This example opts both an ordinary benchmark and a coordinated two-worker
benchmark into system-wide RAPL energy measurement:

```rust,ignore
g.backend(|| Box::new(
    LinuxPerfBackend::new()
        .with_compact_counters()
        .with_rapl_energy()
))
    .bench("single-thread arithmetic", arithmetic);

g.backend(|| Box::new(
    LinuxPerfBackend::new()
        .with_compact_counters()
        .with_rapl_energy()
))
    .sample_duration(Duration::from_millis(50))
    .bench("two-worker arithmetic", &workers);
```

The ordinary result combines the compact calling-thread PMU profile with RAPL metrics.
The concurrent result keeps CPU counters from the managed workers and adds
package-wide energy for their complete coordinated window.

Look for the `RAPL energy` metrics section. Available domains depend on the
processor, but whole-package energy commonly produces:

- `Package energy/op` in `µJ/op`
- `Package energy/sample` in Joules
- `Package power` in Watts

The `RAPL aggregate` section sums the energy deltas, operations, and active
sample time before calculating its energy/op and power values. Prefer that
section when individual samples are short enough for RAPL quantization to be
visible.

These are gross system-wide estimates. Other activity on the measured package
is included, so run energy benchmarks on a quiet machine and use samples long
enough to dominate counter quantization and background noise.
