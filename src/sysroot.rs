use std::fs::{self, File};
use std::io::Write;
use std::ops::Not;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result};
use tempdir::TempDir;

#[allow(unused)]
pub enum BuildMode {
    Build,
    Check,
}

pub fn build_sysroot(
    target: &str,
    mode: BuildMode,
    src_dir: &Path,
    sysroot_dir: &Path,
    cargo_cmd: impl Fn() -> Command,
) -> Result<()> {
    // Prepare a workspace for cargo
    let build_dir = TempDir::new("cargo-careful").context("failed to create tempdir")?;
    let lock_file = build_dir.path().join("Cargo.lock");
    let manifest_file = build_dir.path().join("Cargo.toml");
    let lib_file = build_dir.path().join("lib.rs");
    fs::copy(
        src_dir
            .parent()
            .expect("src_dir must have a parent")
            .join("Cargo.lock"),
        &lock_file,
    )
    .context("failed to copy lockfile")?;
    let manifest = format!(
        r#"
[package]
authors = ["The Rust Project Developers"]
name = "sysroot"
version = "0.0.0"

[lib]
path = "lib.rs"

[dependencies.std]
features = ["panic_unwind", "backtrace"]
path = "{src_dir}/std"
[dependencies.test]
path = "{src_dir}/test"

[patch.crates-io.rustc-std-workspace-core]
path = "{src_dir}/rustc-std-workspace-core"
[patch.crates-io.rustc-std-workspace-alloc]
path = "{src_dir}/rustc-std-workspace-alloc"
[patch.crates-io.rustc-std-workspace-std]
path = "{src_dir}/rustc-std-workspace-std"
    "#,
        src_dir = src_dir
            .to_str()
            .context("rust source directoy contains non-unicode characters")?,
    );
    File::create(&manifest_file)
        .context("failed to create manifest file")?
        .write_all(manifest.as_bytes())
        .context("failed to write manifest file")?;
    File::create(&lib_file).context("failed to create lib file")?;

    // Run cargo.
    let mut cmd = cargo_cmd();
    cmd.arg(match mode {
        BuildMode::Build => "build",
        BuildMode::Check => "check",
    });
    cmd.arg("--release");
    cmd.arg("--manifest-path");
    cmd.arg(&manifest_file);
    cmd.arg("--target");
    cmd.arg(target);
    // Make sure the results end up where we expect them.
    cmd.env("CARGO_TARGET_DIR", build_dir.path().join("target"));
    // To avoid metadata conflicts, we need to inject some custom data into the crate hash.
    // bootstrap does the same at
    // <https://github.com/rust-lang/rust/blob/c8e12cc8bf0de646234524924f39c85d9f3c7c37/src/bootstrap/builder.rs#L1613>.
    cmd.env("__CARGO_DEFAULT_LIB_METADATA", "cargo-careful");

    if cmd
        .status()
        .unwrap_or_else(|_| panic!("failed to execute cargo for sysroot build"))
        .success()
        .not()
    {
        anyhow::bail!("sysroot build failed");
    }

    // Copy the output.
    let out_dir = build_dir
        .path()
        .join("target")
        .join(target)
        .join("debug")
        .join("deps");
    let dst_dir = sysroot_dir
        .join("lib")
        .join("rustlib")
        .join(target)
        .join("lib");
    fs::create_dir_all(&dst_dir).context("failed to create destination dir")?;
    for entry in fs::read_dir(&out_dir).context("failed to read cargo out dir")? {
        let entry = entry.context("failed to read cargo out dir entry")?;
        assert!(
            entry.file_type().unwrap().is_file(),
            "cargo out dir must not contain directories"
        );
        let entry = entry.path();
        fs::copy(&entry, dst_dir.join(entry.file_name().unwrap()))
            .context("failed to copy cargo out file")?;
    }

    // Cleanup.
    drop(build_dir);

    Ok(())
}
