#!/usr/bin/env bash
set -euo pipefail

coverage_output="${COVERAGE_OUTPUT:-lcov.info}"
line_threshold="${COVERAGE_FAIL_UNDER_LINES:-0}"

echo "[coverage] Running workspace coverage."
echo "[coverage] Output: ${coverage_output}"
echo "[coverage] Minimum line coverage: ${line_threshold}%"

cargo llvm-cov \
  --workspace \
  --lcov \
  --output-path "${coverage_output}" \
  --fail-under-lines "${line_threshold}"
