## Benchmark comparison: render-suite

- Current: `series` `sha256:5f80133053bc64ee320ede633a2c4ae496c54bfb3050cc10c4dea96c60aad043`
- Baseline: `series` `sha256:ec6b9da91b45400c5db49ba129ee3263831255756432f53c9f77e57ac9181217`
- Environment: exact match on runner runner-a
- Policy: gating; material change &gt; 5.00%; max CV 2.00%; max outliers disabled
- Gate: **FAIL (1 blocking regressions)**

| Outcome | Count |
|---|---:|
| Matched | 1 |
| Added | 0 |
| Removed | 0 |
| Improvements | 0 |
| Regressions | 1 |
| No material change | 0 |
| Informational | 0 |
| Invalid | 0 |
| Inconclusive | 0 |
| Unstable | 1 |

| Case | Measurement | Baseline | Current | Improvement | Classification | Findings |
|---|---|---:|---:|---:|---|---|
| request\|path/latency p50 | latency ms | 100.0000 ms | 115.0000 ms | -15.00% | regression | current CV 3.55% exceeds 2.00% |
