#!/bin/bash
set -e

# setup
export RUSTFLAGS="-D warnings"
cargo install --locked --path ..

# test
cargo careful setup
cargo careful build --locked
cargo clean
cargo careful run --locked
cargo careful test --locked

# test no-std
pushd test-no_std
rustup target add x86_64-unknown-none
cargo careful setup --target x86_64-unknown-none
cargo careful build --target x86_64-unknown-none --locked
cargo clean
popd
