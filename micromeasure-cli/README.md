# micromeasure CLI

`micromeasure-cli` installs the `micromeasure` executable for validating and
comparing native micromeasure reports and portable external sample-series
reports.

```sh
cargo install micromeasure-cli
micromeasure --help
```

The CLI is CI-provider-neutral. It reads local report files or directory trees,
writes terminal, Markdown, and JSON comparisons, and returns stable status
codes. Artifact download, upload, annotations, and baseline promotion remain
the pipeline's responsibility.

See the repository's
[Suite CLI documentation](https://github.com/rdaum/micromeasure/blob/main/book/src/suite-cli.md)
for the report workflow and complete option behavior.
