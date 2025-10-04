#!/usr/bin/env bash
set -euo pipefail

uname_s=$(uname -s)
uname_m=$(uname -m)

if [[ "$uname_s" == "Linux" && "$uname_m" == "x86_64" ]]; then
  exec gcc "$@"
fi

if command -v x86_64-unknown-linux-gnu-gcc >/dev/null 2>&1; then
  exec x86_64-unknown-linux-gnu-gcc "$@"
elif command -v x86_64-linux-gnu-gcc >/dev/null 2>&1; then
  exec x86_64-linux-gnu-gcc "$@"
elif command -v clang >/dev/null 2>&1; then
  exec clang "$@"
else
  exec gcc "$@"
fi
