#!/bin/bash

set -euxo pipefail

CARGO_ARG=""
if $(sw_vers > /dev/null 2>&1); then
  CARGO_ARG=""
else
  CARGO_ARG="--config target.x86_64-unknown-linux-gnu.linker=\"\""
fi

cargo +nightly fmt --check --all -- --config error_on_line_overflow=true,error_on_unformatted=true
cargo $CARGO_ARG test
