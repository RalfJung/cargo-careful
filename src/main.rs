#![allow(clippy::needless_borrow)]

use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{self, Command};

use anyhow::{bail, Context, Result};
use rustc_build_sysroot::{BuildMode, SysrootBuilder, SysrootConfig};
use rustc_version::VersionMeta;

#[macro_use]
mod util;

use util::*;

const CAREFUL_FLAGS: &[&str] = &[
    "-Cdebug-assertions=on",
    "-Zextra-const-ub-checks",
    "-Zstrict-init-checks",
    "--cfg",
    "careful",
];
const STD_FEATURES: &[&str] = &["panic_unwind", "backtrace"];

/// The sanitizer to use when just `-Zcareful-sanitizer` is passed as flag.
const DEFAULT_SANITIZER: &str = "address";

pub fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| OsString::from("cargo")))
}

pub fn rustc() -> Command {
    Command::new(env::var_os("RUSTC").unwrap_or_else(|| OsString::from("rustc")))
}

pub fn rustc_version_info() -> VersionMeta {
    VersionMeta::for_command(rustc()).expect("failed to determine rustc version")
}

// Computes the extra flags that need to be passed to cargo to make it behave like the current
// cargo invocation.
fn cargo_extra_flags() -> Vec<String> {
    let mut flags = Vec::new();
    // `-Zunstable-options` is required by `--config`.
    flags.push("-Zunstable-options".to_string());

    // Forward `--config` flags.
    let config_flag = "--config";
    for arg in get_arg_flag_values(config_flag) {
        flags.push(config_flag.to_string());
        flags.push(arg);
    }

    // Forward `--manifest-path`.
    let manifest_flag = "--manifest-path";
    if let Some(manifest) = get_arg_flag_value(manifest_flag) {
        flags.push(manifest_flag.to_string());
        flags.push(manifest);
    }

    // Forwarding `--target-dir` would make sense, but `cargo metadata` does not support that flag.

    flags
}

pub fn get_rustflags() -> Vec<String> {
    // Highest precedence: the encoded env var.
    if let Ok(rustflags) = env::var("CARGO_ENCODED_RUSTFLAGS") {
        return if rustflags.is_empty() {
            vec![]
        } else {
            rustflags.split('\x1f').map(Into::into).collect()
        };
    }

    // Next: the old var.
    if let Ok(a) = env::var("RUSTFLAGS") {
        // This code is taken from `RUSTFLAGS` handling in cargo.
        return a
            .split(' ')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect();
    }

    // As fallback, ask `cargo config`.
    // FIXME: This does not take into account `target.rustflags`.
    let mut cmd = cargo();
    cmd.args(&["config", "build.rustflags", "--format=json-value"]);
    cmd.args(cargo_extra_flags());
    let output = cmd.output().expect("failed to run `cargo config`");
    if !output.status.success() {
        // This can fail if the variable is not set.
        return vec![];
    }
    serde_json::from_slice(&output.stdout).unwrap()
}

/// Returns whether the given sanitizer is supported on this target.
///
/// # Errors
/// Returns `Err` if there was an error when getting the list of supported sanitizers.
pub fn sanitizer_supported(san: &str, target: &str) -> Result<bool> {
    // To get the list of supported sanitizers, we call `rustc --print target-spec-json`
    // and parse the output.

    let mut cmd = rustc();
    cmd.args([
        "-Z",
        "unstable-options",
        "--print",
        "target-spec-json",
        "--target",
        target,
    ]);
    let output = cmd
        .output()
        .context("rustc --print target-spec-json` failed to run")?;

    let output_str = String::from_utf8(output.stdout)
        .context("`rustc --print target-spec-json` returned invalid UTF-8")?;

    let json: serde_json::Value = serde_json::from_str(&output_str)
        .context("`rustc --print target-spec-json` output is invalid JSON")?;

    let map = json
        .as_object()
        .context("Target spec JSON has unexpected structure")?;

    // The list of supported sanitizers is stored as an array
    // in the "supported-sanitizers" key of the target JSON
    match map.get("supported-sanitizers") {
        Some(serde_json::Value::Array(arr)) => Ok(arr
            .iter()
            .any(|v| matches!(&v, &serde_json::Value::String(s) if s == san))),
        Some(_) => {
            bail!("Contents of \"supported-sanitizers\" key in target spec JSON are of unexpected type")
        }
        None => Ok(false),
    }
}

fn build_sysroot(
    auto: bool,
    target: &str,
    rustc_version: &VersionMeta,
    rustflags: &[String],
    sanitizer: Option<&str>,
) -> PathBuf {
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
            let rustup_src = rustc_build_sysroot::rustc_sysroot_src(rustc())
                .expect("could not determine sysroot source directory");
            if !rustup_src.exists() {
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

    // Determine where to put the sysroot.
    let user_dirs = directories::ProjectDirs::from("de", "ralfj", "cargo-careful").unwrap();
    let mut sysroot_dir: PathBuf = user_dirs.cache_dir().to_owned();

    // From rust/src/bootstrap/config.rs
    // https://github.com/rust-lang/rust/blob/25b5af1b3a0b9e2c0c57b223b2d0e3e203869b2c/src/bootstrap/config.rs#L549-L555
    let no_std = target.contains("-none")
        || target.contains("nvptx")
        || target.contains("switch")
        || target.contains("-uefi");

    if let Some(san) = sanitizer {
        // Use a separate sysroot dir, to get separate caching of builds with and without sanitizer.
        sysroot_dir.push(san);
        eprint!("Preparing a careful sysroot (target: {target}, sanitizer: {san})... ")
    } else {
        eprint!("Preparing a careful sysroot (target: {target})... ")
    }
    if !auto {
        eprintln!();
    }
    let mut builder = SysrootBuilder::new(&sysroot_dir, target)
        .build_mode(BuildMode::Build)
        .rustc_version(rustc_version.clone())
        .cargo({
            let mut cmd = cargo();
            if auto {
                cmd.stdout(process::Stdio::null());
                cmd.stderr(process::Stdio::null());
            }
            cmd
        })
        .sysroot_config(if no_std {
            SysrootConfig::NoStd
        } else {
            SysrootConfig::WithStd {
                std_features: STD_FEATURES.iter().copied().map(Into::into).collect(),
            }
        })
        .rustflags(CAREFUL_FLAGS)
        .rustflags(rustflags);

    if let Some(san) = sanitizer {
        builder = builder.rustflag(format!("-Zsanitizer={san}"));
    }
    builder
        .build_from_source(&rust_src)
        .expect("failed to build sysroot; run `cargo careful setup` to see what went wrong");

    if auto {
        eprintln!("done");
    } else {
        eprintln!("A sysroot is now available in `{}`.", sysroot_dir.display());
    }

    sysroot_dir
}

fn cargo_careful(args: env::Args) {
    let mut args = args.peekable();

    let rustc_version = rustc_version_info();
    let (target, explicit_target) = if let Some(target) = get_arg_flag_value("--target") {
        (target, true)
    } else {
        (rustc_version.host.clone(), false)
    };

    let verbose = num_arg_flag("-v");

    let subcommand = args.next().unwrap_or_else(|| {
        show_error!("`cargo careful` needs to be called with a subcommand (`run`, `test`)");
    });
    // `None` means "just the setup, please".
    let subcommand = match &*subcommand {
        "setup" => None,
        "test" | "t" | "run" | "r" | "build" | "b" | "nextest" => Some(subcommand),
        _ =>
            show_error!(
                "`cargo careful` supports the following subcommands: `run`, `test`, `build`, `nextest`, and `setup`."
            ),
    };

    let mut san_to_try = None;
    let rustflags = get_rustflags();

    // Go through the args to figure out what is for cargo and what is for us.
    let mut cargo_args = Vec::new();
    for arg in args.by_ref() {
        if let Some(careful_arg) = arg.strip_prefix("-Zcareful-") {
            let (key, value): (&str, Option<&str>) = match careful_arg.split_once('=') {
                Some((key, value)) => (key, Some(value)),
                None => (careful_arg, None),
            };
            match (key, value) {
                ("sanitizer", Some(san)) => san_to_try = Some(san.to_owned()),
                ("sanitizer", None) => san_to_try = Some(DEFAULT_SANITIZER.to_owned()),
                _ => show_error!("unsupported careful flag `{}`", arg),
            }
            continue;
        } else if arg == "--" {
            // The rest is definitely not for us.
            break;
        }

        // Forward regular argument.
        cargo_args.push(arg);
    }
    // The rest is for cargo to forward to the binary / test runner.
    cargo_args.push("--".into());
    cargo_args.extend(args);

    let sanitizer = san_to_try.and_then(|san| {
        sanitizer_supported(&san, &target).map_or_else(
            |e| {
                show_error!("failed to get list supported sanitizers: {e}");
            },
            |b| {
                if b {
                    eprintln!("Using sanitizier `{san}`.");
                    Some(san)
                } else {
                    show_error!("sanitizer `{san}` not supported by target `{target}`");
                }
            },
        )
    });

    // Let's get ourselves as sysroot.
    let sysroot = build_sysroot(
        /*auto*/ subcommand.is_some(),
        &target,
        &rustc_version,
        &rustflags,
        sanitizer.as_deref(),
    );
    let subcommand = match subcommand {
        Some(c) => c,
        None => {
            // We just did the setup.
            return;
        }
    };

    // Invoke cargo for the real work.
    let mut flags: Vec<OsString> = rustflags.into_iter().map(Into::into).collect();
    flags.push("--sysroot".into());
    flags.push(sysroot.into());
    flags.extend(CAREFUL_FLAGS.iter().map(Into::into));
    if let Some(san) = sanitizer.as_deref() {
        flags.push(format!("-Zsanitizer={san}").into());
    }

    let mut cmd = cargo();
    cmd.arg(subcommand);

    // Avoids using sanitizers for build scripts and proc macros.
    if !explicit_target && sanitizer.is_some() {
        cmd.args(["--target", target.as_str()]);
    }

    cmd.args(cargo_args);

    // Setup environment. Both rustc and rustdoc need these flags.
    cmd.env(
        "CARGO_ENCODED_RUSTFLAGS",
        rustc_build_sysroot::encode_rustflags(&flags),
    );
    cmd.env(
        "CARGO_ENCODED_RUSTDOCFLAGS",
        rustc_build_sysroot::encode_rustflags(&flags),
    );

    // Leaks are not a memory safety issue, don't detect them by default
    if sanitizer.as_deref() == Some("address") && env::var_os("ASAN_OPTIONS").is_none() {
        cmd.env("ASAN_OPTIONS", "detect_leaks=0");
    }

    // Run it!
    exec(cmd, (verbose > 0).then_some("[cargo-careful] "));
}

fn main() {
    let mut args = env::args();
    // Skip binary name.
    args.next().unwrap();

    let first = args.next().unwrap_or_else(|| {
        show_error!(
            "`cargo-careful` called without first argument; please only invoke this binary through `cargo careful`"
        )
    });
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
