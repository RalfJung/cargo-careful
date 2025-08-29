use std::env;
use std::ffi::OsString;
use std::path::PathBuf;
use std::process::{self, Command, Stdio};

use anyhow::{anyhow, bail, Context, Result};
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
const STD_FEATURES: &[&str] = &["panic-unwind", "backtrace"];

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

/// Find the path for Apple's Main Thread Checker on the current system.
///
/// This is intended to be used on macOS, but should work on other systems
/// that have something similar to XCode set up.
fn main_thread_checker_path() -> Result<Option<PathBuf>> {
    // Find the Xcode developer directory, usually one of:
    // - /Applications/Xcode.app/Contents/Developer
    // - /Library/Developer/CommandLineTools
    //
    // This could be done by the `apple-sdk` crate, but we avoid the dependency here.
    let output = Command::new("xcode-select")
        .args(["--print-path"])
        .stderr(Stdio::null())
        .output()
        .context("`xcode-select --print-path` failed to run")?;

    if !output.status.success() {
        return Err(anyhow!(
            "got error when running `xcode-select --print-path`:\n{:?}",
            output,
        ));
    }

    let stdout = String::from_utf8(output.stdout)
        .context("`xcode-select --print-path` returned invalid UTF-8")?;
    let developer_dir = PathBuf::from(stdout.trim());

    // Introduced in XCode 9.0, and has not changed location since.
    // <https://developer.apple.com/library/archive/releasenotes/DeveloperTools/RN-Xcode/Chapters/Introduction.html#//apple_ref/doc/uid/TP40001051-CH1-SW974>
    let path = developer_dir.join("usr/lib/libMainThreadChecker.dylib");
    if path.try_exists()? {
        Ok(Some(path))
    } else {
        eprintln!(
            "warn: libMainThreadChecker.dylib could not be found at {}",
            path.display()
        );
        eprintln!("      This usually means you're using the Xcode command line tools, which does not have this capability.");
        Ok(None)
    }
}

/// Get the specific path from the output of an external program.
///
/// E.g. `rustc --print sysroot` or `realpath $file`.
fn get_external_path(mut cmd: Command, args: &[&str]) -> Result<PathBuf> {
    let shell_command = format!(
        "`{cmd} {args}`",
        cmd = cmd.get_program().to_string_lossy(),
        args = args.join(" ")
    );
    let output = cmd
        .args(args)
        .stderr(Stdio::null())
        .output()
        .with_context(|| format!("{shell_command} failed to run"))?;
    if !output.status.success() {
        anyhow::bail!("got error when running {shell_command}");
    }

    let path: PathBuf = String::from_utf8(output.stdout)
        .with_context(|| format!("{shell_command} returned invalid UTF-8"))?
        .trim()
        .into();
    if !path.try_exists()? {
        anyhow::bail!("{} does not exist", path.display());
    }
    Ok(path)
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
    cmd.args(["config", "build.rustflags", "--format=json-value"]);
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
    verbose: usize,
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
    // By default, the output gets captured. But sometimes we want to show it to the user.
    let show_output = verbose > 0 || !auto;
    if show_output {
        eprintln!();
    }
    let mut builder = SysrootBuilder::new(&sysroot_dir, target)
        .build_mode(BuildMode::Build)
        .rustc_version(rustc_version.clone())
        .cargo({
            let mut cmd = cargo();
            if show_output {
                cmd.stdout(process::Stdio::inherit());
                cmd.stderr(process::Stdio::inherit());
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
        // User-provided flags must come after CAREFUL_FLAGS so that they can be overridden.
        .rustflags(CAREFUL_FLAGS)
        .rustflags(rustflags);

    if let Some(san) = sanitizer {
        builder = builder.rustflag(format!("-Zsanitizer={san}"));
    }
    builder
        .build_from_source(&rust_src)
        .expect("failed to build sysroot; run `cargo careful setup` to see what went wrong");

    if sanitizer.is_some() && target.contains("-darwin") {
        // build_sysroot doesn't copy the `librustc-nightly_rt.asan.dylib` for some reason
        // so, let's do it ourselves
        let asan_rt = get_external_path(rustc(), &["+nightly", "--print", "target-libdir"])
            .context("Failed to get target-libdir")
            .unwrap()
            .join("librustc-nightly_rt.asan.dylib");

        // aka `SysrootBuilder::sysroot_target_dir` but that's private
        let target_dir = sysroot_dir.join("lib").join("rustlib").join(target);
        let target_libdir = target_dir.join("lib");

        std::fs::copy(
            &asan_rt,
            target_libdir.join("librustc-nightly_rt.asan.dylib"),
        )
        .with_context(|| {
            format!(
                "failed to copy {src} to {dst}",
                src = asan_rt.display(),
                dst = target_libdir.display(),
            )
        })
        .expect("failed to copy asan_rt");
    }

    if !show_output {
        eprintln!("done");
    } else {
        eprintln!("A sysroot is now available in `{}`.", sysroot_dir.display());
    }

    sysroot_dir
}

fn cargo_careful(args: env::Args) -> Result<()> {
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
        "test" | "t" | "run" | "r" | "build" | "b" => Some(vec![subcommand]),
        "nextest" => {
            // In nextest we have to also forward the main `verb` before things like `--target`.
            let subsubcommand = args.next()
                .unwrap_or_else(|| show_error!("`cargo careful nextest` expects a verb (e.g. `run`)"));
            Some(vec![subcommand, subsubcommand])
        }
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
        verbose,
    );
    let subcommand = match subcommand {
        Some(c) => c,
        None => {
            // We just did the setup.
            return Ok(());
        }
    };

    // Invoke cargo for the real work.
    let mut flags: Vec<OsString> = CAREFUL_FLAGS.iter().map(Into::into).collect();
    // User-provided flags must come after CAREFUL_FLAGS so that they can be overridden.
    flags.extend(rustflags.into_iter().map(Into::into));
    flags.push("--sysroot".into());
    flags.push(sysroot.into());
    if let Some(san) = sanitizer.as_deref() {
        flags.push(format!("-Zsanitizer={san}").into());
    }

    let mut cmd = cargo();
    cmd.args(subcommand);

    // Avoids using sanitizers for build scripts and proc macros.
    if !explicit_target && sanitizer.is_some() {
        cmd.args(["--target", target.as_str()]);
    }

    // Enable Main Thread Checker on macOS targets, as documented here:
    // <https://developer.apple.com/documentation/xcode/diagnosing-memory-thread-and-crash-issues-early#Detect-improper-UI-updates-on-background-threads>
    //
    // On iOS, tvOS and watchOS simulators, the path is somewhere inside the
    // simulator runtime, which is more difficult to find, so we don't do that
    // yet (those target also probably wouldn't run in `cargo-careful` anyway).
    //
    // Note: The main thread checker by default removes itself from
    // `DYLD_INSERT_LIBRARIES` upon load, see `MTC_RESET_INSERT_LIBRARIES`:
    // <https://bryce.co/main-thread-checker-configuration/#mtc_reset_insert_libraries>
    // This means that it is not inherited by child processes, so we have to
    // tell Cargo to set this environment variable for the processes it
    // launches (instead of just setting it for Cargo itself using `cmd.env`).
    //
    // Note: We do this even if the host is not running macOS, even though the
    // environment variable will also be passed to any rustc processes that
    // Cargo spawns (as Cargo doesn't currently have a good way of only
    // specifying environment variables to only the binary being run).
    // This is probably fine though, the environment variable is
    // Apple-specific and will likely be ignored on other hosts.
    if target.contains("-darwin") {
        if let Some(path) = main_thread_checker_path()? {
            cmd.arg("--config");
            // TODO: Quote the path correctly according to toml rules
            cmd.arg(format!("env.DYLD_INSERT_LIBRARIES={path:?}"));
        }
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
    exec(cmd, (verbose > 0).then_some("[cargo-careful] "))
}

fn main() -> Result<()> {
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
            cargo_careful(args)
        }
        _ => {
            show_error!(
                "`cargo-careful` called with bad first argument; please only invoke this binary through `cargo careful`"
            )
        }
    }
}
