#!/bin/bash
set -e

# setup
export RUSTFLAGS="-D warnings"
cargo install --path ..

# test
cargo careful setup
cargo careful build
cargo clean
cargo careful run
cargo careful test
