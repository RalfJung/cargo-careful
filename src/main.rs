use std::env;
use std::ffi::OsString;
use std::fmt::Write as _;
use std::io::{self, Write};
use std::ops::Not;
use std::path::PathBuf;
use std::process::{self, Command};

use cargo_metadata::{Metadata, MetadataCommand};
use rustc_build_sysroot::{BuildMode, Sysroot, SysrootConfig};
use rustc_version::VersionMeta;

const CAREFUL_FLAGS: &[&str] = &[
    "-Zstrict-init-checks",
    "-Zextra-const-ub-checks",
    "-Cdebug-assertions=on",
];
const STD_FEATURES: &[&str] = &["panic_unwind", "backtrace"];

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
                panic!("unsetting env vars not current supported by debug printing");
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

pub fn get_cargo_metadata() -> Metadata {
    // This will honor the `CARGO` env var the same way our `cargo()` does.
    MetadataCommand::new()
        .no_deps()
        .other_options(cargo_extra_flags())
        .exec()
        .unwrap()
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

fn build_sysroot(auto: bool, target: &str, rustc_version: &VersionMeta) -> PathBuf {
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
    let sysroot_dir = user_dirs.cache_dir();

    // Do the build.
    eprint!("Preparing a careful sysroot (target: {target})... ");
    if !auto {
        eprintln!();
    }
    Sysroot::new(sysroot_dir, target)
        .build_from_source(
            &rust_src,
            BuildMode::Build,
            SysrootConfig::WithStd {
                std_features: STD_FEATURES,
            },
            rustc_version,
            || {
                let mut flags = Vec::new();
                flags.extend(CAREFUL_FLAGS.iter().map(Into::into));

                let mut cmd = cargo();
                if auto {
                    cmd.stdout(process::Stdio::null());
                    cmd.stderr(process::Stdio::null());
                }

                (cmd, flags)
            },
        )
        .expect("failed to build sysroot; run `cargo careful setup` to see what went wrong");
    if auto {
        eprintln!("done");
    } else {
        eprintln!("A sysroot is now available in `{}`.", sysroot_dir.display());
    }

    PathBuf::from(sysroot_dir)
}

fn cargo_careful(mut args: env::Args) {
    let rustc_version = rustc_version_info();
    let cargo_metadata = get_cargo_metadata();
    let target = get_arg_flag_value("--target").unwrap_or_else(|| rustc_version.host.clone());
    let verbose = num_arg_flag("-v");

    let subcommand = args.next().unwrap_or_else(|| {
        show_error!("`cargo careful` needs to be called with a subcommand (`run`, `test`)");
    });
    let subcommand = match &*subcommand {
        "setup" => {
            // Just build the sysroot and be done.
            build_sysroot(/*auto*/ false, &target, &rustc_version);
            return;
        }
        "test" | "t" | "run" | "r" | "nextest" => subcommand,
        _ =>
            show_error!(
                "`cargo careful` supports the following subcommands: `run`, `test`, `nextest`, and `setup`."
            ),
    };

    // Let's get ourselves as sysroot.
    let sysroot = build_sysroot(/*auto*/ true, &target, &rustc_version);

    // target-dir handling. cargo does not make this easy...
    if get_arg_flag_value("--target-dir").is_some() {
        // Cargo does not support overwriting CLI falgs with later CLI flags, and `cargo metadata`
        // does not support `--target-dir`, so handling this properly rewquires some annoying
        // surgery. Let's just not support this for now.
        show_error!("`cargo careful` currently does not support the `--target-dir` flag");
    }
    let mut target_dir = cargo_metadata.target_directory;
    // Set `--target-dir` to `careful` inside the original target directory.
    target_dir.push("careful");

    // Invoke cargo for the real work.
    let mut flags = Vec::new();
    flags.push("--sysroot".into());
    flags.push(sysroot.into());
    flags.extend(CAREFUL_FLAGS.iter().map(Into::into));

    let mut cmd = cargo();
    cmd.arg(subcommand);
    // Forward user-provided cargo flags until `--`.
    cmd.args((&mut args).take_while(|arg| arg != "--"));
    // Inject our own flags.
    cmd.arg("--target-dir").arg(target_dir);
    // Forward remaining user-provided flags.
    cmd.arg("--").args(args);
    // Set up environment.
    cmd.env(
        "CARGO_ENCODED_RUSTFLAGS",
        rustc_build_sysroot::encode_rustflags(flags),
    );
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
