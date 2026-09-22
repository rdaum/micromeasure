#!/usr/bin/env bash
# Checks a copied package with no access to Mica imports.
set -euo pipefail
package_root="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
work_dir="$(mktemp -d "${TMPDIR:-/tmp}/micromeasure-standalone.XXXXXX")"
trap 'rm -rf "${work_dir}"' EXIT
cp -R "${package_root}" "${work_dir}/micromeasure"
odin_bin="${ODIN_BIN:-odin}"
"${odin_bin}" test "${work_dir}/micromeasure" -out:"${work_dir}/tests"
"${odin_bin}" run "${work_dir}/micromeasure/examples/basic" -o:speed \
  -out:"${work_dir}/example" -- "${work_dir}/results.json" "standalone-check"
test -s "${work_dir}/results.json"
