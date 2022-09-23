use std::collections::hash_map::DefaultHasher;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Write};
use std::ops::Not;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use tempdir::TempDir;

#[allow(unused)]
pub enum BuildMode {
    Build,
    Check,
}

pub struct Sysroot {
    sysroot_dir: PathBuf,
    target: String,
}

impl Sysroot {
    pub fn new(sysroot_dir: &Path, target: &str) -> Self {
        Sysroot {
            sysroot_dir: sysroot_dir.to_owned(),
            target: target.to_owned(),
        }
    }

    fn target_dir(&self) -> PathBuf {
        self.sysroot_dir
            .join("lib")
            .join("rustlib")
            .join(&self.target)
    }

    fn hash_file(&self) -> PathBuf {
        self.target_dir().join(".cargo-careful-hash")
    }

    /// Computes the hash for the sysroot, so that we know whether we have to rebuild.
    fn sysroot_compute_hash(&self, src_dir: &Path, rustc_cmd: &impl Fn() -> Command) -> u64 {
        // For now, we just hash in the source dir and rustc commit.
        // Ideally we'd recursively hash the entire folder but that sounds slow?
        let mut hasher = DefaultHasher::new();

        src_dir.hash(&mut hasher);

        let rustc_info = rustc_version::VersionMeta::for_command(rustc_cmd())
            .expect("failed to determine rustc version");
        rustc_info.commit_hash.hash(&mut hasher);

        hasher.finish()
    }

    fn sysroot_read_hash(&self) -> Option<u64> {
        let hash_file = self.hash_file();
        let mut hash = String::new();
        File::open(&hash_file)
            .ok()?
            .read_to_string(&mut hash)
            .ok()?;
        hash.parse().ok()
    }

    pub fn build_from_source(
        &self,
        src_dir: &Path,
        mode: BuildMode,
        rustc_cmd: impl Fn() -> Command,
        cargo_cmd: impl Fn() -> Command,
    ) -> Result<()> {
        // Check if we even need to do anything.
        let cur_hash = self.sysroot_compute_hash(src_dir, &rustc_cmd);
        if self.sysroot_read_hash() == Some(cur_hash) {
            // Already done!
            return Ok(());
        }

        // Make sure we start clean.
        let target_dir = self.target_dir();
        if target_dir.exists() {
            fs::remove_dir_all(&target_dir).context("failed to clean sysroot target dir")?;
        }

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
path = {src_dir_std:?}
[dependencies.test]
path = {src_dir_test:?}

[patch.crates-io.rustc-std-workspace-core]
path = {src_dir_workspace_core:?}
[patch.crates-io.rustc-std-workspace-alloc]
path = {src_dir_workspace_alloc:?}
[patch.crates-io.rustc-std-workspace-std]
path = {src_dir_workspace_std:?}
    "#,
            src_dir_std = src_dir.join("std"),
            src_dir_test = src_dir.join("test"),
            src_dir_workspace_core = src_dir.join("rustc-std-workspace-core"),
            src_dir_workspace_alloc = src_dir.join("rustc-std-workspace-alloc"),
            src_dir_workspace_std = src_dir.join("rustc-std-workspace-std"),
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
        cmd.arg(&self.target);
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
            .join(&self.target)
            .join("release")
            .join("deps");
        let dst_dir = target_dir.join("lib");
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

        // Write the hash file.
        File::create(self.hash_file())
            .context("failed to create hash file")?
            .write_all(cur_hash.to_string().as_bytes())
            .context("failed to write hash file")?;

        // Cleanup.
        drop(build_dir);

        Ok(())
    }
}
