#!/bin/bash
set -e

# setup
cargo install --path ..

# test
cargo careful run
