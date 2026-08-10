# Memory bandwidth

Source: [`examples/memory_bandwidth.rs`](https://github.com/rdaum/micromeasure/blob/main/examples/memory_bandwidth.rs)

Run it with:

```sh
cargo run --example memory_bandwidth --release
```

The example updates a 64 MiB buffer and opts into Linux uncore IMC counters:

```rust,ignore
g.backend(|| {
    Box::new(
        LinuxPerfBackend::new()
            .without_cpu_counters(),
    )
})
.bench("64 MiB streaming update", streaming_update);
```

On a machine with compatible `uncore_imc_*` PMUs and system-wide perf access,
the custom metrics table includes gross DRAM read/write/total bytes per
operation and GiB/s. On unsupported or permission-restricted machines, the
benchmark still reports timing and throughput after a one-time warning.

The IMC values include all traffic on the measured memory controllers during
the sample window. They are not process-exclusive counters.
