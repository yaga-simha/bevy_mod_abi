#!/usr/bin/env bash
# ==============================================================================
# bevy_abi_mod: End-to-End Verification Runner (delegates to `cargo xtask`)
# ==============================================================================
set -euo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

echo "==> Running full workspace CI checks, fixtures, C99 tests, and test suites..."
cargo xtask ci

echo "==> Running permutation benchmark verification..."
cargo xtask bench --permutation

if [ ! -f "target/benchmark_results.json" ]; then
    echo "ERROR: target/benchmark_results.json was not generated" >&2
    exit 1
fi

echo "==> E2E verification completed."
