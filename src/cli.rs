use std::ffi::OsString;
use std::path::{Path, PathBuf};

use clap::{Args, CommandFactory, Parser, Subcommand};

use crate::service::{self, ForwardOutcome, ServiceCommand, forward_to_running_instance};
use crate::{config, editor, home_dir, hotkey, log, normalize_dir};

#[derive(Debug, Clone)]
pub struct PickerRequest {
    pub path: PathBuf,
    pub path_explicit: bool,
    pub in_grep: bool,
    pub query: Option<String>,
    /// Print the picks instead of opening them in the editor.
    pub print: bool,
}

pub enum Launch {
    Daemon,
    Picker(PickerRequest),
}

#[derive(Parser)]
#[command(
    version,
    about = "Fast, keyboard-driven file finder for macOS",
    after_help = "Append `help` to any command for its help, such as `fff-gpui grep help`.\n\
        Picks open in your editor, or are printed when stdout is a pipe or a file.\n\
        Set FFF_GPUI_DEBUG=1 to also print logs to stderr.",
    args_conflicts_with_subcommands = true
)]
pub struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
    #[command(flatten)]
    files: FilesArgs,
    // Deprecated flags, hidden and kept for one release so existing Zed tasks keep working.
    #[arg(short = 'o', long = "open", hide = true, value_parser = parse_dir)]
    legacy_open: Option<PathBuf>,
    #[arg(long = "grep", hide = true)]
    legacy_grep: bool,
    #[arg(long = "std-out", hide = true)]
    legacy_stdout: bool,
    #[arg(short = 'd', long = "debug", hide = true)]
    legacy_debug: bool,
}

#[derive(Subcommand)]
enum Command {
    #[command(flatten)]
    Picker(PickerCommand),
    /// Print the picks instead of opening them, even when stdout is a terminal
    Stdout(StdoutArgs),
    /// Open the config file in your editor, or check or reload it
    Config(ConfigArgs),
    /// Manage the background daemon and its launchd service
    #[command(subcommand)]
    Server(ServerCommand),
    /// Print the version
    Version,
}

#[derive(Subcommand)]
enum PickerCommand {
    /// Search file names (the default command)
    Files(FilesArgs),
    /// Search file contents
    Grep(GrepArgs),
}

#[derive(Args)]
struct FilesArgs {
    /// Directory to search; defaults to your home directory
    #[arg(value_parser = parse_dir)]
    path: Option<PathBuf>,
}

#[derive(Args)]
struct GrepArgs {
    /// Directory to search; defaults to your home directory
    #[arg(value_parser = parse_dir)]
    path: Option<PathBuf>,
    /// Text to search for when the picker opens
    query: Option<String>,
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct StdoutArgs {
    #[command(subcommand)]
    picker: Option<PickerCommand>,
    #[command(flatten)]
    files: FilesArgs,
}

#[derive(Args)]
struct ConfigArgs {
    #[command(subcommand)]
    command: Option<ConfigCommand>,
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Check the config file for errors, unknown keys, and invalid values
    Check,
    /// Check the config file and apply it to the running daemon
    Reload,
}

#[derive(Subcommand)]
enum ServerCommand {
    /// Run the daemon in the foreground; the launchd service runs this
    Run,
    /// Install a LaunchAgent that starts the daemon at login and on demand
    Install,
    /// Stop the daemon and remove the LaunchAgent
    Uninstall,
    /// Start the daemon in the background
    Start,
    /// Stop the daemon; with the LaunchAgent, it starts again on the next request
    Stop,
    /// Restart the daemon, for example to pick up a new binary
    Restart,
    /// Show whether the service is installed and the daemon is running
    Status,
    /// Show the daemon's log
    Logs(LogsArgs),
}

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct LogsArgs {
    #[command(subcommand)]
    follow: Option<LogsCommand>,
    /// Number of lines to show
    #[arg(default_value_t = 200)]
    lines: usize,
}

#[derive(Subcommand)]
enum LogsCommand {
    /// Keep printing lines as they are written
    Follow {
        /// Number of earlier lines to show first
        #[arg(default_value_t = 10)]
        lines: usize,
    },
}

fn parse_dir(value: &str) -> Result<PathBuf, String> {
    normalize_dir(PathBuf::from(value)).ok_or_else(|| "not a directory".to_string())
}

// `fff-gpui grep help` means `fff-gpui grep --help`, but only when every word before `help` names
// a subcommand, so `fff-gpui grep . help` still searches for "help".
fn expand_trailing_help(mut args: Vec<OsString>) -> Vec<OsString> {
    let Some((last, words)) = args.split_last_mut() else {
        return args;
    };
    if last != "help" {
        return args;
    }
    let mut command = Cli::command();
    let names_subcommands = words.iter().skip(1).all(|word| {
        let subcommand = word
            .to_str()
            .and_then(|name| command.find_subcommand(name))
            .cloned();
        subcommand.map(|subcommand| command = subcommand).is_some()
    });
    if names_subcommands {
        *last = OsString::from("--help");
    }
    args
}

// Print picks when stdout feeds another program or a file. A launcher that discards output
// (such as skhd) points stdout at /dev/null and should still get the editor.
fn stdout_is_captured() -> bool {
    use std::io::IsTerminal;
    use std::os::fd::AsFd;
    use std::os::unix::fs::MetadataExt;

    let stdout = std::io::stdout();
    if stdout.is_terminal() {
        return false;
    }
    let Ok(fd) = stdout.as_fd().try_clone_to_owned() else {
        return false;
    };
    match (
        std::fs::File::from(fd).metadata(),
        std::fs::metadata("/dev/null"),
    ) {
        (Ok(out), Ok(null)) => out.dev() != null.dev() || out.ino() != null.ino(),
        _ => false,
    }
}

pub fn debug_requested(cli: &Cli) -> bool {
    cli.legacy_debug
        || std::env::var_os("FFF_GPUI_DEBUG").is_some_and(|value| !value.is_empty() && value != "0")
}

// Homebrew's `brew services` job runs a bare `fff-gpui`, which used to start the daemon. Match
// its exact label: children inherit XPC_SERVICE_NAME, and the LaunchAgent's label differs.
fn started_as_brew_service() -> bool {
    std::env::args_os().len() == 1
        && std::env::var("XPC_SERVICE_NAME").is_ok_and(|name| name == "homebrew.mxcl.fff-gpui")
}

fn picker(path: Option<PathBuf>, in_grep: bool, query: Option<String>, print: bool) -> Launch {
    Launch::Picker(PickerRequest {
        path_explicit: path.is_some(),
        path: path.unwrap_or_else(home_dir),
        in_grep,
        query,
        print,
    })
}

/// Runs commands that need no picker or daemon and returns `None` for them.
pub fn launch_from_cli(cli: Cli) -> anyhow::Result<Option<Launch>> {
    let launch = match cli.command {
        None => {
            if cli.legacy_open.is_some() || cli.legacy_grep || cli.legacy_stdout || cli.legacy_debug
            {
                eprintln!(
                    "fff-gpui: --open, --grep, --std-out, and --debug are deprecated; use \
                     `fff-gpui files PATH`, `fff-gpui grep PATH`, `fff-gpui stdout`, or \
                     FFF_GPUI_DEBUG=1"
                );
            }
            let legacy = cli.legacy_open.is_some() || cli.legacy_grep || cli.legacy_stdout;
            if started_as_brew_service() {
                Launch::Daemon
            } else {
                // Legacy invocations keep their old output: the editor unless --std-out.
                let print = if legacy {
                    cli.legacy_stdout
                } else {
                    stdout_is_captured()
                };
                picker(
                    cli.legacy_open.or(cli.files.path),
                    cli.legacy_grep,
                    None,
                    print,
                )
            }
        }
        Some(Command::Picker(PickerCommand::Files(args))) => {
            picker(args.path, false, None, stdout_is_captured())
        }
        Some(Command::Picker(PickerCommand::Grep(args))) => {
            picker(args.path, true, args.query, stdout_is_captured())
        }
        Some(Command::Stdout(args)) => match args.picker {
            None => picker(args.files.path, false, None, true),
            Some(PickerCommand::Files(files)) => picker(files.path, false, None, true),
            Some(PickerCommand::Grep(grep)) => picker(grep.path, true, grep.query, true),
        },
        Some(Command::Config(args)) => {
            run_config_command(args.command)?;
            return Ok(None);
        }
        Some(Command::Server(command)) => return run_server_command(command),
        Some(Command::Version) => {
            println!("fff-gpui {}", env!("CARGO_PKG_VERSION"));
            return Ok(None);
        }
    };
    Ok(Some(launch))
}

fn check_config(path: &Path) -> anyhow::Result<Vec<String>> {
    let loaded = config::load_config_from(path)?;
    let mut problems = loaded.warnings;
    if let Err(err) = hotkey::parse_hotkey(loaded.config.global_keybind.as_deref()) {
        problems.push(err.to_string());
    }
    Ok(problems)
}

fn run_config_command(command: Option<ConfigCommand>) -> anyhow::Result<()> {
    use anyhow::Context as _;

    let path = config::config_path();
    match command {
        None => {
            config::ensure_config_file(&path)?;
            let editor = config::load_config_from(&path)
                .map(|loaded| loaded.config.editor)
                .unwrap_or_default();
            let mut child =
                editor::open_in_editor(&path, None, &editor, editor::EditorLaunchMode::Foreground)?;
            child.wait().context("failed to wait for the editor")?;
        }
        Some(ConfigCommand::Check) => {
            if !path.exists() {
                println!("no config file at {}; using defaults", path.display());
                return Ok(());
            }
            let problems = check_config(&path)?;
            if problems.is_empty() {
                println!("{}: ok", path.display());
            } else {
                for problem in &problems {
                    println!("{}: {problem}", path.display());
                }
                anyhow::bail!("found {} problem(s) in the config", problems.len());
            }
        }
        Some(ConfigCommand::Reload) => {
            for problem in check_config(&path)? {
                eprintln!("fff-gpui: warning: {problem}");
            }
            // Asking a waiting launchd job would start the daemon only to reload it.
            let reloaded = !service::launchd_job_waiting()?
                && matches!(
                    forward_to_running_instance(&ServiceCommand::ReloadConfig)?,
                    ForwardOutcome::Picked(_) | ForwardOutcome::Closed
                );
            if reloaded {
                println!(
                    "reloaded {}; idle_exit_secs changes apply after `fff-gpui server restart`",
                    path.display()
                );
            } else {
                println!(
                    "fff-gpui is not running; it reads {} when it starts",
                    path.display()
                );
            }
        }
    }
    Ok(())
}

fn run_server_command(command: ServerCommand) -> anyhow::Result<Option<Launch>> {
    match command {
        ServerCommand::Run => return Ok(Some(Launch::Daemon)),
        ServerCommand::Logs(args) => match args.follow {
            Some(LogsCommand::Follow { lines }) => log::show_log(lines, true)?,
            None => log::show_log(args.lines, false)?,
        },
        #[cfg(target_os = "macos")]
        ServerCommand::Install => {
            let path = service::install_launch_agent()?;
            println!(
                "installed {}; fff-gpui now starts at login and on demand",
                path.display()
            );
        }
        #[cfg(target_os = "macos")]
        ServerCommand::Uninstall => {
            service::uninstall_launch_agent()?;
            println!("removed the fff-gpui LaunchAgent");
        }
        #[cfg(target_os = "macos")]
        ServerCommand::Start => {
            service::start_service()?;
            println!("fff-gpui is running");
        }
        #[cfg(target_os = "macos")]
        ServerCommand::Stop => {
            if service::stop_service()? {
                println!("stopped fff-gpui");
            } else {
                println!("fff-gpui is not running");
            }
        }
        #[cfg(target_os = "macos")]
        ServerCommand::Restart => {
            service::restart_service()?;
            println!("restarted fff-gpui");
        }
        #[cfg(target_os = "macos")]
        ServerCommand::Status => print!("{}", service::service_status()?),
        #[cfg(not(target_os = "macos"))]
        _ => anyhow::bail!("this command manages a launchd service and only works on macOS"),
    }
    Ok(None)
}

pub fn parse() -> Cli {
    Cli::parse_from(expand_trailing_help(std::env::args_os().collect()))
}
