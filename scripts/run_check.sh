#!/bin/bash

set -euxo pipefail

cargo +nightly fmt --check --all -- --config error_on_line_overflow=true,error_on_unformatted=true
cargo test
