#!/usr/bin/env bash
set -euo pipefail

binary=${1:?"usage: $0 <ELF binary>"}

if [[ ! -f "$binary" ]]; then
  echo "::error::ELF binary not found: $binary" >&2
  exit 1
fi

elf_type=$(readelf -h "$binary" | awk '$1 == "Type:" { print $2 }')
if [[ "$elf_type" != "DYN" ]]; then
  echo "::error::$binary is not PIE (ELF type is ${elf_type:-unknown}, expected DYN)" >&2
  exit 1
fi

if ! readelf -lW "$binary" | grep -q 'GNU_RELRO'; then
  echo "::error::$binary does not contain a GNU_RELRO program header" >&2
  exit 1
fi

# Linkers may record immediate binding as DT_BIND_NOW, DF_BIND_NOW, or
# DF_1_NOW. Accept the corresponding readelf representations.
if ! readelf -dW "$binary" | grep -Eq 'BIND_NOW|FLAGS(_1)?[^[]*NOW'; then
  echo "::error::$binary does not enable immediate binding (BIND_NOW)" >&2
  exit 1
fi

echo "ELF hardening verified for $binary: PIE and full RELRO"
