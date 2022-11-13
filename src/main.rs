use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::ops::Not;
use std::path::PathBuf;
use std::process::{self, Command};

use anyhow::{bail, Context, Result};
use rustc_build_sysroot::{BuildMode, SysrootBuilder, SysrootConfig};
use rustc_version::VersionMeta;

const CAREFUL_FLAGS: &[&str] = &[
    "-Cdebug-assertions=on",
    "-Zextra-const-ub-checks",
    "-Zrandomize-layout",
    "-Zstrict-init-checks",
];
const STD_FEATURES: &[&str] = &["panic_unwind", "backtrace"];

/// The sanitizer to use when just `-Zcareful-sanitizer` is passed as flag.
const DEFAULT_SANITIZER: &str = "address";

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

pub fn rustc_version_info() -> VersionMeta {
    VersionMeta::for_command(rustc()).expect("failed to determine rustc version")
}

/// Execute the `Command`, where possible by replacing the current process with a new process
/// described by the `Command`. Then exit this process with the exit code of the new process.
///
/// If `verbose` is `Some(prefix)`, print the prefix followed by the command to invoke.
pub fn exec(mut cmd: Command, verbose: Option<&str>) -> ! {
    if let Some(prefix) = verbose {
        let mut out = String::from(prefix);
        for (var, val) in cmd.get_envs() {
            if let Some(val) = val {
                write!(out, "{}={:?} ", var.to_string_lossy(), val).unwrap();
            } else {
                // Existing env vars are always in quotes, so `<deleted>` cannot be confused with an
                // env var set to the value `"<deleted>"`.
                write!(out, "{}=<deleted> ", var.to_string_lossy()).unwrap();
            }
        }
        write!(out, "{cmd:?}").unwrap();
        eprintln!("{out}");
    }
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

/// Gets the values of a `--flag`.
pub fn get_arg_flag_values(name: &str) -> impl Iterator<Item = String> + '_ {
    pub struct ArgFlagValueIter<'a> {
        args: Option<env::Args>,
        name: &'a str,
    }

    impl Iterator for ArgFlagValueIter<'_> {
        type Item = String;
        fn next(&mut self) -> Option<String> {
            let args = self.args.as_mut()?;
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

/// Determines how many times a `--flag` is present.
pub fn num_arg_flag(name: &str) -> usize {
    env::args()
        .take_while(|val| val != "--")
        .filter(|val| val == name)
        .count()
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

/// Returns whether the given sanitizer is supported on this target.
///
/// # Errors
/// Returns `Err` if there was an error when geutting the list of supported sanitizers.
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
        .sysroot_config(SysrootConfig::WithStd {
            std_features: STD_FEATURES.iter().copied().map(Into::into).collect(),
        })
        .rustflags(CAREFUL_FLAGS);

    if let Some(san) = sanitizer {
        builder = builder.rustflag(format!("-Zsanitizer={}", san));
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
    let target = get_arg_flag_value("--target").unwrap_or_else(|| rustc_version.host.clone());
    let verbose = num_arg_flag("-v");

    let subcommand = args.next().unwrap_or_else(|| {
        show_error!("`cargo careful` needs to be called with a subcommand (`run`, `test`)");
    });
    // `None` means "just the setup, please".
    let subcommand = match &*subcommand {
        "setup" => None,
        "test" | "t" | "run" | "r" | "nextest" => Some(subcommand),
        _ =>
            show_error!(
                "`cargo careful` supports the following subcommands: `run`, `test`, `nextest`, and `setup`."
            ),
    };

    let mut san_to_try = None;

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
    let mut flags = Vec::new();
    flags.push("--sysroot".into());
    flags.push(sysroot.into());
    flags.extend(CAREFUL_FLAGS.iter().map(Into::into));
    if let Some(san) = sanitizer.as_deref() {
        flags.push(format!("-Zsanitizer={}", san).into());
    }

    let mut cmd = cargo();
    cmd.arg(subcommand);
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
