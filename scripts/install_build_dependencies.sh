#!/bin/bash

is_mac () {
    sw_vers > /dev/null 2>&1
    return $?
}

set -euxo pipefail

cargo install cross

rustup target add x86_64-pc-windows-gnu
rustup target add x86_64-unknown-linux-gnu
rustup target add aarch64-apple-darwin
rustup target add aarch64-unknown-linux-gnu

if is_mac; then
  brew install x86_64-unknown-linux-gnu
  brew install aarch64-unknown-linux-gnu
  brew install mingw-w64
else
  sudo apt update
  sudo apt install -y gcc-mingw-w64
  sudo apt install -y gcc-aarch64-linux-gnu
fi
