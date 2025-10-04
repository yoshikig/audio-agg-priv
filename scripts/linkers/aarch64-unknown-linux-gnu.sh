#!/usr/bin/env bash
set -euo pipefail

uname_s=$(uname -s)
uname_m=$(uname -m)

if [[ "$uname_s" == "Linux" && ( "$uname_m" == "aarch64" || "$uname_m" == "arm64" ) ]]; then
  exec gcc "$@"
fi

if command -v aarch64-unknown-linux-gnu-gcc >/dev/null 2>&1; then
  exec aarch64-unknown-linux-gnu-gcc "$@"
elif command -v aarch64-linux-gnu-gcc >/dev/null 2>&1; then
  exec aarch64-linux-gnu-gcc "$@"
elif command -v clang >/dev/null 2>&1; then
  exec clang "$@"
else
  echo "error: could not find aarch64 GNU linker" >&2
  exit 1
fi
