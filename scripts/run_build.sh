#!/bin/bash

set -euo pipefail

cargo build
cargo build --release

cargo build --features cpal
cargo build --release --features cpal

cargo build --release --bin udp_reciever --target=x86_64-unknown-linux-gnu
cargo build --release --bin udp_sender --target x86_64-pc-windows-gnu

