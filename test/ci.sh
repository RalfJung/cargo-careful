#!/bin/bash
set -e
set -x # so one can see where we are in the script

# setup
export RUSTFLAGS="-D warnings"
cargo install --locked --path ..

# test
cargo careful setup -v
cargo careful build --locked -v
cargo clean
cargo careful run --locked
cargo careful test --locked

# test no-std
pushd test-no_std
cargo careful setup --target x86_64-unknown-none
cargo careful build --target x86_64-unknown-none --locked
cargo clean
popd

# test with sanitizer -- this only works on Linux; macOS and Windows fail with a linker error
if uname -s | grep -q "Linux"; then
    cargo careful run -Zcareful-sanitizer --locked
    cargo careful test -Zcareful-sanitizer --locked
fi

# test Apple's Main Thread Checker
if uname -s | grep -q "Darwin"; then
    pushd test-main_thread_checker
    # Run as normal; this will output warnings, but not fail
    cargo careful run --locked
    # Run with flag that tells the Main Thread Checker to fail
    # See <https://bryce.co/main-thread-checker-configuration/>
    if MTC_CRASH_ON_REPORT=1 cargo careful run --locked
    then
        echo "Main Thread Checker did not crash"
        exit 1
    fi
    cargo clean
    popd
fi

# Debugging
git clone https://github.com/mozilla/neqo --depth 1
cd neqo
cargo careful test --target "$(rustc --version --verbose | grep host: | cut -d' ' -f2)"
