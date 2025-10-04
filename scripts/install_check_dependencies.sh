#!/bin/bash

is_mac () {
    sw_vers > /dev/null 2>&1
    return $?
}

set -euxo pipefail

rustup +nightly component add rustfmt
rustup component add clippy
