use std::env;
use std::ffi::{OsStr, OsString};
use std::fs::{self, File};
use std::io::{self, Write};
use std::ops::Not;
use std::path::{Path, PathBuf};
use std::process::{self, Command};

use tempdir::TempDir;

const CAREFUL_FLAGS: &[&str] = &[
    "-Zstrict-init-checks",
    "-Zextra-const-ub-checks",
    "-Cdebug-assertions=on",
];

pub fn show_error(msg: &impl std::fmt::Display) -> ! {
    eprintln!("fatal error: {msg}");
    process::exit(1)
}

macro_rules! show_error {
    ($($tt:tt)*) => { crate::show_error(&format_args!($($tt)*)) };
}

pub fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
}

pub fn rustc() -> Command {
    Command::new(env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")))
}

pub fn rustc_version_info() -> rustc_version::VersionMeta {
    rustc_version::VersionMeta::for_command(rustc()).expect("failed to determine rustc version")
}

/// Execute the `Command`, where possible by replacing the current process with a new process
/// described by the `Command`. Then exit this process with the exit code of the new process.
pub fn exec(mut cmd: Command) -> ! {
    // On non-Unix imitate POSIX exec as closely as we can
    #[cfg(not(unix))]
    {
        let exit_status = cmd.status().expect("failed to run command");
        std::process::exit(exit_status.code().unwrap_or(-1))
    }
    // On Unix targets, actually exec.
    // If exec returns, process setup has failed. This is the same error condition as the expect in
    // the non-Unix case.
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let error = cmd.exec();
        Err(error).expect("failed to run command")
    }
}

fn encode_flags(flags: &[OsString]) -> OsString {
    flags.join(OsStr::new("\x1f"))
}

/// Gets the values of a `--flag`.
pub fn get_arg_flag_values(name: &str) -> impl Iterator<Item = String> + '_ {
    pub struct ArgFlagValueIter<'a> {
        args: Option<env::Args>,
        name: &'a str,
    }

    impl Iterator for ArgFlagValueIter<'_> {
        type Item = String;
        fn next(&mut self) -> Option<String> {
            let Some(args) = self.args.as_mut() else {
                // We already canceled this iterator.
                return None;
            };
            loop {
                let arg = args.next()?;
                if arg == "--" {
                    // Stop searching at `--`.
                    self.args = None;
                    return None;
                }
                // There is a next argument to look at.
                if let Some(suffix) = arg.strip_prefix(self.name) {
                    if suffix.is_empty() {
                        // This argument is exactly `name`; the next one is the value.
                        return args.next();
                    } else if let Some(suffix) = suffix.strip_prefix('=') {
                        // This argument is `name=value`; get the value.
                        return Some(suffix.to_owned());
                    } else {
                        // Some other flag that starts with `name`. Go on looping.
                    }
                } else {
                    // An uninteresting argument, does not start with `name`. Go on looping.
                }
            }
        }
    }

    ArgFlagValueIter {
        args: Some(env::args()),
        name,
    }
}

/// Gets the value of a `--flag`.
pub fn get_arg_flag_value(name: &str) -> Option<String> {
    get_arg_flag_values(name).next()
}

pub fn ask_to_run(mut cmd: Command, ask: bool, text: &str) {
    // Disable interactive prompts in CI (GitHub Actions, Travis, AppVeyor, etc).
    // Azure doesn't set `CI` though (nothing to see here, just Microsoft being Microsoft),
    // so we also check their `TF_BUILD`.
    let is_ci = env::var_os("CI").is_some() || env::var_os("TF_BUILD").is_some();
    if ask && !is_ci {
        let mut buf = String::new();
        print!("I will run `{:?}` to {}. Proceed? [Y/n] ", cmd, text);
        io::stdout().flush().unwrap();
        io::stdin().read_line(&mut buf).unwrap();
        match buf.trim().to_lowercase().as_ref() {
            // Proceed.
            "" | "y" | "yes" => {}
            "n" | "no" => show_error!("aborting as per your request"),
            a => show_error!("invalid answer `{}`", a),
        };
    } else {
        eprintln!("Running `{:?}` to {}.", cmd, text);
    }

    if cmd
        .status()
        .unwrap_or_else(|_| panic!("failed to execute {:?}", cmd))
        .success()
        .not()
    {
        show_error!("failed to {}", text);
    }
}

fn build_sysroot(auto: bool, target: &str) -> PathBuf {
    // Determine where the rust sources are located.  The env var manually setting the source
    // trumps auto-detection.
    let rust_src = std::env::var_os("RUST_LIB_SRC");
    let rust_src = match rust_src {
        Some(path) => {
            let path = PathBuf::from(path);
            // Make path absolute if possible.
            path.canonicalize().unwrap_or(path)
        }
        None => {
            // Check for `rust-src` rustup component.
            let output = rustc()
                .args(["--print", "sysroot"])
                .output()
                .expect("failed to determine sysroot");
            if !output.status.success() {
                show_error!(
                    "Failed to determine sysroot; rustc said:\n{}",
                    String::from_utf8_lossy(&output.stderr).trim_end()
                );
            }
            let sysroot = std::str::from_utf8(&output.stdout).unwrap();
            let sysroot = Path::new(sysroot.trim_end_matches('\n'));
            // Check for `$SYSROOT/lib/rustlib/src/rust/library`; test if that contains `std/Cargo.toml`.
            let rustup_src = sysroot
                .join("lib")
                .join("rustlib")
                .join("src")
                .join("rust")
                .join("library");
            if !rustup_src.join("std").join("Cargo.toml").exists() {
                // Ask the user to install the `rust-src` component, and use that.
                let mut cmd = Command::new("rustup");
                cmd.args(["component", "add", "rust-src"]);
                ask_to_run(
                    cmd,
                    auto,
                    "install the `rust-src` component for the selected toolchain",
                );
            }
            rustup_src
        }
    };
    if !rust_src.join("std").join("Cargo.toml").exists() {
        show_error!(
            "given Rust source directory `{}` does seem to contain a standard library source tree",
            rust_src.display()
        );
    }

    // Determine where to put the sysroot.
    let user_dirs = directories::ProjectDirs::from("de", "ralfj", "cargo-careful").unwrap();
    let sysroot_dir = user_dirs.cache_dir();
    if !sysroot_dir.exists() {
        fs::create_dir_all(sysroot_dir).unwrap();
    }

    // Prepare a workspace for cargo
    let build_dir = std::mem::ManuallyDrop::new(
        TempDir::new("cargo-careful").expect("failed to create tempdir"),
    ); // FIXME
    let lock_file = build_dir.path().join("Cargo.lock");
    let manifest_file = build_dir.path().join("Cargo.toml");
    let lib_file = build_dir.path().join("lib.rs");
    fs::copy(rust_src.parent().unwrap().join("Cargo.lock"), &lock_file)
        .expect("failed to copy lockfile");
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
path = "{rust_src}/std"
[dependencies.test]
path = "{rust_src}/test"

[patch.crates-io.rustc-std-workspace-alloc]
path = "{rust_src}/rustc-std-workspace-alloc"
[patch.crates-io.rustc-std-workspace-core]
path = "{rust_src}/rustc-std-workspace-core"
[patch.crates-io.rustc-std-workspace-std]
path = "{rust_src}/rustc-std-workspace-std"
        "#,
        rust_src = rust_src
            .to_str()
            .expect("Rust source directoy contains non-unicode characters"),
    );
    File::create(&manifest_file)
        .expect("failed to create manifest file")
        .write_all(manifest.as_bytes())
        .expect("failed to write manifest file");
    File::create(&lib_file).expect("failed to create lib file");

    // Run cargo.
    let mut flags = Vec::new();
    flags.extend(CAREFUL_FLAGS.iter().map(Into::into));

    let mut cargo = cargo();
    cargo.arg("build");
    cargo.arg("--manifest-path");
    cargo.arg(&manifest_file);
    cargo.arg("--target");
    cargo.arg(target);
    cargo.env("CARGO_ENCODED_RUSTFLAGS", encode_flags(&flags));
    if cargo
        .status()
        .unwrap_or_else(|_| panic!("failed to execute cargo for sysroot build"))
        .success()
        .not()
    {
        show_error!("sysroot build failed");
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
    fs::create_dir_all(&dst_dir).expect("failed to create destination dir");
    for entry in fs::read_dir(&out_dir).expect("failed to read cargo out dir") {
        let entry = entry.expect("failed to read cargo out dir entry");
        assert!(
            entry.file_type().unwrap().is_file(),
            "cargo out dir must not contain directories"
        );
        let entry = entry.path();
        fs::copy(&entry, dst_dir.join(entry.file_name().unwrap()))
            .expect("failed to copy cargo out file");
    }

    // Cleanup.
    drop(build_dir);

    PathBuf::from(sysroot_dir)
}

fn cargo_careful(mut args: env::Args) {
    let target = get_arg_flag_value("--target").unwrap_or_else(|| rustc_version_info().host);

    let Some(subcommand) = args.next() else {
        show_error!("`cargo careful` needs to be called with a subcommand (`run`, `test`)");
    };
    let subcommand = match &*subcommand {
        "setup" => {
            // Just build the sysroot and be done.
            build_sysroot(/*auto*/ false, &target);
            return;
        }
        "test" | "t" | "run" | "r" | "nextest" => subcommand,
        _ =>
            show_error!(
                "`cargo careful` supports the following subcommands: `run`, `test`, `nextest`, and `setup`."
            ),
    };

    // Let's get ourselves as sysroot.
    let sysroot = build_sysroot(/*auto*/ true, &target);

    // Invoke cargo for the real work.
    let mut flags = Vec::new();
    flags.push("--sysroot".into());
    flags.push(sysroot.into());
    flags.extend(CAREFUL_FLAGS.iter().map(Into::into));

    let mut cmd = cargo();
    cmd.arg(subcommand);
    cmd.args(args);
    cmd.env("CARGO_ENCODED_RUSTFLAGS", encode_flags(&flags));
    exec(cmd);
}

fn main() {
    let mut args = env::args();
    // Skip binary name.
    args.next().unwrap();

    let Some(first) = args.next() else {
        show_error!(
            "`cargo-careful` called without first argument; please only invoke this binary through `cargo careful`"
        )
    };
    match first.as_str() {
        "careful" => {
            // It's us!
            cargo_careful(args);
        }
        _ => {
            show_error!(
                "`cargo-careful` called with bad first argument; please only invoke this binary through `cargo careful`"
            )
        }
    }
}
