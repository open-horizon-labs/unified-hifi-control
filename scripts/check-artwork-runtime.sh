#!/usr/bin/env bash
# The ONNX reference backend is test/developer-only. A normal server dependency
# graph must never retain its import/translation stack.
set -euo pipefail
runtime_tree="$(cargo tree --locked --edges normal --no-default-features --features server --prefix none)"
if printf '%s\n' "$runtime_tree" | grep -E '^((zensally-tract|tract-(onnx|onnx-opl|hir|transformers|pulse|pulse-opl)) )'; then
  echo 'ONNX import stack leaked into the shipping artwork runtime' >&2
  exit 1
fi
printf '%s\n' 'Shipping artwork runtime excludes ONNX import and translation crates.'
