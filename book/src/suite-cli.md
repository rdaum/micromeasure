# Suite CLI

The separately packaged `micromeasure-cli` executable validates and compares
report artifacts without adding command-line dependencies to library users.
Its binary name is `micromeasure`:

```sh
cargo install micromeasure-cli
micromeasure --help
```

From a source checkout, use `cargo install --path micromeasure-cli`.

The tool is provider-neutral. It does not inspect CI-provider environment
variables, and it does not download baselines, upload artifacts, publish
annotations, or promote reports. A pipeline chooses the inputs and consumes the
generated files.

## Validate reports

Validate one native or external report immediately after producing it:

```sh
micromeasure validate artifacts/scenarios/cold-start.json
```

The command also accepts a directory. It recursively validates every `.json`
file beneath that directory, ignores non-JSON files and symbolic links, and
prints the suite, document type, and durable digest for each report:

```sh
micromeasure validate artifacts/current/
```

Validation checks the document and schema, semantic report invariants,
comparison-ready suite and runner identity, context values, report validity,
and duplicate case identities. A directory containing two documents that claim
the same suite is invalid.

## Compare files or directories

Compare one exact report pair:

```sh
micromeasure compare \
    --baseline artifacts/baseline/basic.json \
    --current artifacts/current/basic.json
```

Or compare two collections:

```sh
micromeasure compare \
    --baseline artifacts/baseline/ \
    --current artifacts/current/ \
    --json-output artifacts/derived/comparison.json \
    --markdown-output artifacts/derived/SUMMARY.md \
    --minimum-change 5
```

The terminal summary is always written to standard output. `--json-output`
writes the complete versioned `SuiteComparisonAnalysis`; `--markdown-output`
writes a combined annotation-ready summary. Output parents are created as
needed, and each file is atomically replaced. Keep derived outputs outside the
input directories so a later recursive validation does not mistake them for
raw evidence.

Directory loading is deterministic:

1. recursively collect `.json` files in lexical path order;
2. load native benchmark and external series documents through the same public
   report loader;
3. validate every document before comparing any suite;
4. reject duplicate suite names instead of selecting one by timestamp,
   filename, or document type;
5. match suites by name and require the document type to remain stable;
6. compare matching suites and retain suites present on only one side as added
   or removed; and
7. aggregate suite, case, policy, stability, and gate counts.

A directory may mix native and external reports when each suite keeps the same
document type across current and baseline. Implicitly composing several report
shards into one suite is not supported.

Matched suites allow added and removed cases by default. Use
`--strict-result-set` to reject a case-set change. Directory comparisons also
retain current-only and baseline-only suites by default; use
`--strict-suite-set` to reject either. Runner identity and the complete
comparison environment must still match exactly. An intentional exception
must carry a visible reason:

```sh
micromeasure compare \
    --baseline baseline/ \
    --current current/ \
    --environment-override "equivalent replacement host"
```

## Policy options

Comparison is advisory unless `--fail-on-regression`, `--fail-on-invalid`, or
both are supplied. The initial defaults match `RegressionPolicy::advisory()`:

- `--minimum-change 5`
- `--maximum-cv 10`
- `--maximum-outlier-fraction 0.10`

With `--fail-on-regression`, a material regression becomes blocking. Stability
findings remain visible but do not become regressions or independently fail the
gate. With `--fail-on-invalid`, a matched current or baseline result carrying
invalid correctness evidence becomes blocking.

## Process status

The executable implements the stable comparison status contract:

| Code | Meaning |
|---:|---|
| `0` | validation/comparison completed and no enabled policy gate failed |
| `1` | comparison completed and an enabled policy gate failed |
| `2` | invocation, input, schema, validation, compatibility, policy, serialization, or output error |

Status `1` still produces terminal, Markdown, and JSON results. It represents a
valid comparison with an unfavorable policy result, not a failure to create
the comparison.

## Pipeline integration

A generic CI step only needs to place trusted baseline and current reports on
disk, run the tool, and pass the Markdown and JSON files to provider-specific
artifact or annotation commands:

```sh
micromeasure validate artifacts/current/
micromeasure compare \
    --baseline artifacts/baseline/ \
    --current artifacts/current/ \
    --json-output artifacts/derived/comparison.json \
    --markdown-output artifacts/derived/SUMMARY.md
```

Baseline selection, trusted-build policy, hardware reservation, artifact
transport, annotation publication, and baseline promotion remain outside
`micromeasure`.
