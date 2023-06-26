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

# test no-std
pushd test-no_std
rustup target add x86_64-unknown-none
cargo careful setup --target x86_64-unknown-none
cargo careful build --target x86_64-unknown-none
cargo clean
popd
