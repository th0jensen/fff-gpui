#![allow(unexpected_cfgs)]

mod assets;
mod config;
mod editor;
mod hotkey;
mod log;
mod menubar;
mod path_shortening;
mod picker;
mod preview;
mod service;
mod text_field;
mod theme;

use std::cell::Cell;
use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use assets::{Assets, FontAssets};
use clap::{Args, CommandFactory, Parser, Subcommand};
use fff_search::path_utils::expand_tilde;
use global_hotkey::GlobalHotKeyManager;
use gpui::prelude::*;
use gpui::*;
use tracing::{debug, info};

use config::AppConfig;
use picker::{
    CyclePreviousQuery, FffPicker, OpenSelected, PickerSharedState, PreviewScrollDown,
    PreviewScrollUp, Quit, SelectNext, SelectPrev, ShiftTab, SwitchFiles, SwitchGrep,
    ToggleSelectAll, ToggleSelected,
};
use service::{
    CommandEnvelope, ForwardOutcome, ServiceCommand, forward_to_running_instance, start_listener,
};
use text_field::{
    FieldBackspace, FieldCopy, FieldCut, FieldDelete, FieldEnd, FieldHome, FieldLeft, FieldPaste,
    FieldRight, FieldSelectAll, FieldSelectLeft, FieldSelectRight,
};

actions!(fff_gpui, [ToggleWindow, OpenConfig]);

#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

// mimalloc options we tune at startup so freed pages return to the OS quickly
// instead of being retained in the process. Required because this is a long-
// lived daemon — without this, RSS climbs after grep and never recovers.
//
// Indices match `mi_option_t` in mimalloc.h.
#[repr(C)]
#[derive(Clone, Copy)]
enum MiOption {
    PurgeDecommits = 5,
    PurgeDelay = 15,
    PageFullRetain = 36,
}

unsafe extern "C" {
    fn mi_option_set(option: MiOption, value: std::os::raw::c_long);
}

fn configure_allocator_for_low_rss() {
    unsafe {
        mi_option_set(MiOption::PurgeDecommits, 1);
        mi_option_set(MiOption::PurgeDelay, 0);
        mi_option_set(MiOption::PageFullRetain, 0);
    }
}

#[derive(Debug, Clone)]
struct PickerRequest {
    path: PathBuf,
    path_explicit: bool,
    in_grep: bool,
    query: Option<String>,
    /// Print the picks instead of opening them in the editor.
    print: bool,
}

enum Launch {
    Daemon,
    Picker(PickerRequest),
}

type ResponderArc = Arc<Mutex<Option<service::ClientStream>>>;

#[derive(Clone)]
struct PickerSession {
    base_path: PathBuf,
    shared: PickerSharedState,
    enable_content_indexing: bool,
    start_in_grep: bool,
    print_stdout: bool,
    responder: Option<ResponderArc>,
}

struct RuntimeConfig {
    config: AppConfig,
    config_path: PathBuf,
    hotkey_manager: Option<GlobalHotKeyManager>,
    registered_hotkey: Option<global_hotkey::hotkey::HotKey>,
}

impl PickerSession {
    fn new(
        base_path: PathBuf,
        enable_content_indexing: bool,
        start_in_grep: bool,
        print_stdout: bool,
    ) -> Self {
        Self {
            base_path,
            shared: PickerSharedState::default(),
            enable_content_indexing,
            start_in_grep,
            print_stdout,
            responder: None,
        }
    }
}

// Resolve the user's home directory on Unix.
#[cfg(unix)]
fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

fn normalize_dir(path: PathBuf) -> Option<PathBuf> {
    path.is_dir()
        .then(|| std::fs::canonicalize(&path).unwrap_or(path))
}

// Resolve the base path based on config
fn resolve_base_path(base_path: &Option<String>) -> PathBuf {
    base_path
        .as_deref()
        .and_then(|p| normalize_dir(expand_tilde(p)))
        .unwrap_or_else(home_dir)
}

fn resolve_excluded_dirs(base_path: &Path, exclude_dirs: &[PathBuf]) -> Vec<PathBuf> {
    let home = home_dir();
    exclude_dirs
        .iter()
        .filter_map(|dir| {
            let raw = dir.to_string_lossy();
            if raw.is_empty() {
                return None;
            }

            let resolved = if raw == "~" {
                home.clone()
            } else if let Some(rest) = raw.strip_prefix("~/") {
                home.join(rest)
            } else if dir.is_absolute() {
                dir.clone()
            } else {
                base_path.join(dir)
            };

            Some(resolved)
        })
        .collect()
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
struct Cli {
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
// (skhd, Shortcuts) points stdout at /dev/null and should still get the editor.
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

fn debug_requested(cli: &Cli) -> bool {
    cli.legacy_debug
        || std::env::var_os("FFF_GPUI_DEBUG").is_some_and(|value| !value.is_empty() && value != "0")
}

// Homebrew's `brew services` job runs a bare `fff-gpui`, which used to start the daemon.
fn started_as_brew_service() -> bool {
    std::env::args_os().len() == 1
        && std::env::var("XPC_SERVICE_NAME").is_ok_and(|name| name.contains("fff-gpui"))
}

fn picker(path: Option<PathBuf>, in_grep: bool, query: Option<String>, print: bool) -> Launch {
    Launch::Picker(PickerRequest {
        path_explicit: path.is_some(),
        path: path.unwrap_or_else(home_dir),
        in_grep,
        query,
        print: print || stdout_is_captured(),
    })
}

/// Runs commands that need no picker or daemon and returns `None` for them.
fn launch_from_cli(cli: Cli) -> anyhow::Result<Option<Launch>> {
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
            if started_as_brew_service() {
                Launch::Daemon
            } else {
                picker(
                    cli.legacy_open.or(cli.files.path),
                    cli.legacy_grep,
                    None,
                    cli.legacy_stdout,
                )
            }
        }
        Some(Command::Picker(PickerCommand::Files(args))) => picker(args.path, false, None, false),
        Some(Command::Picker(PickerCommand::Grep(args))) => {
            picker(args.path, true, args.query, false)
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
                    ForwardOutcome::Picked(_)
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

fn deliver_picks(entries: Vec<service::PickEntry>, print: bool, config_editor: &str) {
    for entry in entries {
        if print {
            println!("{}", entry.path.display());
            continue;
        }
        let goto = entry.line.zip(entry.column);
        match editor::open_in_editor(
            &entry.path,
            goto,
            config_editor,
            editor::EditorLaunchMode::Foreground,
        ) {
            Ok(mut child) => {
                if let Err(err) = child.wait() {
                    eprintln!("fff-gpui: failed to wait for the editor: {err}");
                }
            }
            Err(err) => eprintln!("fff-gpui: failed to open {}: {err}", entry.path.display()),
        }
    }
}

#[cfg(target_os = "macos")]
fn make_dockless() {
    use cocoa::appkit::{NSApp, NSApplication, NSApplicationActivationPolicyAccessory};

    unsafe {
        let app = NSApp();
        app.setActivationPolicy_(NSApplicationActivationPolicyAccessory);
    }
}

#[cfg(not(target_os = "macos"))]
fn make_dockless() {}

// Register the built-in key bindings for the picker and text field actions.
fn bind_base_keys(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("escape", Quit, None),
        KeyBinding::new("enter", OpenSelected, None),
        KeyBinding::new("return", OpenSelected, None),
        KeyBinding::new("up", SelectPrev, None),
        KeyBinding::new("down", SelectNext, None),
        KeyBinding::new("tab", ToggleSelected, None),
        KeyBinding::new("ctrl-a", ToggleSelectAll, None),
        KeyBinding::new("shift-tab", ShiftTab, None),
        KeyBinding::new("ctrl-up", CyclePreviousQuery, None),
        KeyBinding::new("ctrl-u", PreviewScrollUp, None),
        KeyBinding::new("ctrl-d", PreviewScrollDown, None),
        KeyBinding::new("cmd-f", SwitchFiles, None),
        KeyBinding::new("cmd-g", SwitchGrep, None),
        KeyBinding::new("backspace", FieldBackspace, None),
        KeyBinding::new("delete", FieldDelete, None),
        KeyBinding::new("left", FieldLeft, None),
        KeyBinding::new("right", FieldRight, None),
        KeyBinding::new("shift-left", FieldSelectLeft, None),
        KeyBinding::new("shift-right", FieldSelectRight, None),
        KeyBinding::new("cmd-a", FieldSelectAll, None),
        KeyBinding::new("cmd-v", FieldPaste, None),
        KeyBinding::new("cmd-c", FieldCopy, None),
        KeyBinding::new("cmd-x", FieldCut, None),
        KeyBinding::new("home", FieldHome, None),
        KeyBinding::new("end", FieldEnd, None),
    ]);
}

fn bind_runtime_keys(cx: &mut App, config: &AppConfig) {
    if let Some(binding) = config.global_keybind.as_deref()
        && let Ok(Some(_)) = hotkey::parse_hotkey(Some(binding))
    {
        cx.bind_keys([KeyBinding::new(binding, ToggleWindow, None)]);
    }
}

fn apply_key_bindings(cx: &mut App, config: &AppConfig, replace: bool) -> bool {
    match hotkey::parse_hotkey(config.global_keybind.as_deref()) {
        Ok(_) => {
            if replace {
                cx.clear_key_bindings();
            }
            bind_base_keys(cx);
            bind_runtime_keys(cx, config);
            true
        }
        Err(err) => {
            if !replace {
                cx.clear_key_bindings();
                bind_base_keys(cx);
            }
            info!(error = %err, "skipping global hotkey binding");
            false
        }
    }
}

fn register_global_hotkey(state: &mut RuntimeConfig, config: &AppConfig) {
    let Ok(Some(hotkey)) = hotkey::parse_hotkey(config.global_keybind.as_deref()) else {
        if let Some(manager) = &state.hotkey_manager
            && let Some(existing) = state.registered_hotkey.take()
        {
            let _ = manager.unregister(existing);
        }
        return;
    };

    if state.hotkey_manager.is_none() {
        state.hotkey_manager = GlobalHotKeyManager::new().ok();
    }

    let Some(manager) = &state.hotkey_manager else {
        info!("global hotkey manager unavailable");
        return;
    };

    if state.registered_hotkey == Some(hotkey) {
        return;
    }

    let previous = state.registered_hotkey.take();
    if let Some(existing) = previous {
        let _ = manager.unregister(existing);
    }

    match manager.register(hotkey) {
        Ok(_) => {
            state.registered_hotkey = Some(hotkey);
            info!(hotkey = %hotkey, "registered global toggle hotkey");
        }
        Err(err) => {
            if let Some(existing) = previous {
                let _ = manager.register(existing);
                state.registered_hotkey = Some(existing);
            }
            info!(error = %err, hotkey = %hotkey, "failed to register global hotkey");
        }
    }
}

fn refresh_runtime_config(runtime_config: &Arc<Mutex<RuntimeConfig>>) -> (AppConfig, bool) {
    let (config_path, current_config) = match runtime_config.lock() {
        Ok(state) => (state.config_path.clone(), state.config.clone()),
        Err(_) => return (AppConfig::default(), false),
    };

    let Ok(loaded) = config::load_config_from(&config_path) else {
        return (current_config, false);
    };

    if loaded.config != current_config {
        info!(
            path = %config_path.display(),
            "config changed on disk; refreshing runtime config"
        );
        if let Ok(mut state) = runtime_config.lock() {
            state.config = loaded.config.clone();
        }
        (loaded.config, true)
    } else {
        (current_config, false)
    }
}

fn open_config_file(runtime: &Arc<Mutex<RuntimeConfig>>) {
    let (path, config) = match runtime.lock() {
        Ok(state) => (state.config_path.clone(), state.config.clone()),
        Err(_) => (config::config_path(), AppConfig::default()),
    };

    if let Err(err) = config::ensure_config_file(&path) {
        info!(error = %err, path = %path.display(), "failed to ensure config file");
        return;
    }

    match editor::open_in_editor(
        &path,
        None,
        &config.editor,
        editor::EditorLaunchMode::Detached,
    ) {
        Ok(child) => {
            info!(pid = child.id(), path = %path.display(), "opened config file");
        }
        Err(err) => {
            info!(error = %err, path = %path.display(), "failed to open config file");
        }
    }
}

#[cfg(target_os = "macos")]
fn active_display_bounds() -> Option<Bounds<Pixels>> {
    use cocoa::appkit::NSScreen;
    use cocoa::base::{id, nil};
    use cocoa::foundation::{NSPoint, NSRect};
    use objc::{class, msg_send, sel, sel_impl};

    unsafe {
        let mouse: NSPoint = msg_send![class!(NSEvent), mouseLocation];
        let screens: id = NSScreen::screens(nil);
        let count: usize = msg_send![screens, count];
        if count == 0 {
            return None;
        }
        let primary: id = msg_send![screens, objectAtIndex: 0usize];
        let primary_h: f64 = NSScreen::frame(primary).size.height;
        for i in 0..count {
            let screen: id = msg_send![screens, objectAtIndex: i];
            let frame: NSRect = NSScreen::frame(screen);
            if mouse.x >= frame.origin.x
                && mouse.x < frame.origin.x + frame.size.width
                && mouse.y >= frame.origin.y
                && mouse.y < frame.origin.y + frame.size.height
            {
                let gpui_y = primary_h - frame.origin.y - frame.size.height;
                return Some(Bounds {
                    origin: point(px(frame.origin.x as f32), px(gpui_y as f32)),
                    size: size(px(frame.size.width as f32), px(frame.size.height as f32)),
                });
            }
        }
        None
    }
}

#[cfg(not(target_os = "macos"))]
fn active_display_bounds() -> Option<Bounds<Pixels>> {
    None
}

// Open the main picker window centered on the active display.
fn open_window(
    session: PickerSession,
    initial_query: Option<String>,
    runtime_config: &Arc<Mutex<RuntimeConfig>>,
    cx: &mut App,
) {
    let (config, changed) = refresh_runtime_config(runtime_config);
    if changed {
        let _ = apply_key_bindings(cx, &config, true);
    }
    theme::sync_from_config(&config, cx.window_appearance(), cx);

    let base_path = session.base_path;
    let shared = session.shared;
    let enable_content_indexing = session.enable_content_indexing;
    let start_in_grep = session.start_in_grep;
    let excluded_dirs = resolve_excluded_dirs(&base_path, &config.exclude_dirs);
    let print_stdout = session.print_stdout;
    let responder = session.responder;
    let editor = config.editor.clone();
    let window_width = config.window_width;
    let window_height = config.window_height;
    let bounds = active_display_bounds()
        .or_else(|| cx.primary_display().map(|d| d.bounds()))
        .map(|db| {
            let x = db.origin.x + (db.size.width - px(window_width)) / 2.0;
            let y = db.origin.y + (db.size.height - px(window_height)) / 3.0;
            Bounds {
                origin: point(x, y),
                size: size(px(window_width), px(window_height)),
            }
        })
        .unwrap_or(Bounds {
            origin: point(px(400.0), px(200.0)),
            size: size(px(window_width), px(window_height)),
        });

    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            titlebar: None,
            is_resizable: false,
            kind: WindowKind::PopUp,
            ..WindowOptions::default()
        },
        |window, cx| {
            let view = cx.new(|cx| {
                FffPicker::new(
                    base_path.clone(),
                    shared.clone(),
                    enable_content_indexing,
                    start_in_grep,
                    initial_query.clone().unwrap_or_default(),
                    excluded_dirs.clone(),
                    print_stdout,
                    editor.clone(),
                    responder.clone(),
                    cx,
                )
            });
            view.update(cx, |picker, cx| {
                picker.install_focus_lost_dismiss(window, cx);
            });
            let focus = view.read(cx).text_field_focus_handle(cx);
            window.focus(&focus);
            view
        },
    )
    .expect("failed to open fff window");
}

fn close_all_windows(cx: &mut App) {
    let windows = cx.windows();
    for window in windows {
        let _ = window.update(cx, |_, window, _| {
            window.remove_window();
        });
    }
}

fn toggle_picker(
    session: &Arc<Mutex<PickerSession>>,
    runtime_config: &Arc<Mutex<RuntimeConfig>>,
    cx: &mut App,
) {
    if cx.windows().is_empty() {
        let (config, changed) = refresh_runtime_config(runtime_config);
        if changed {
            let _ = apply_key_bindings(cx, &config, true);
            if let Ok(mut state) = runtime_config.lock() {
                register_global_hotkey(&mut state, &config);
            }
        }
        let mut session = snapshot_session(session);
        session.base_path = resolve_base_path(&config.base_path);
        session.start_in_grep = false;
        session.responder = None;
        info!(base_path = %session.base_path.display(), "opening picker window");
        open_window(session, None, runtime_config, cx);
    } else {
        info!("closing picker window(s)");
        close_all_windows(cx);
    }
}

fn snapshot_session(session: &Arc<Mutex<PickerSession>>) -> PickerSession {
    session
        .lock()
        .map(|session| session.clone())
        .unwrap_or_else(|_| PickerSession::new(home_dir(), false, false, false))
}

fn one_shot_session(
    path: PathBuf,
    sessions: &Arc<Mutex<HashMap<PathBuf, PickerSession>>>,
    start_in_grep: bool,
    print_stdout: bool,
    responder: Option<ResponderArc>,
) -> PickerSession {
    let fallback_path = path.clone();
    let mut session = sessions
        .lock()
        .map(|mut sessions| {
            sessions
                .entry(path.clone())
                .or_insert_with(|| PickerSession::new(path, true, start_in_grep, print_stdout))
                .clone()
        })
        .unwrap_or_else(|_| PickerSession::new(fallback_path, true, start_in_grep, print_stdout));
    session.start_in_grep = start_in_grep;
    session.print_stdout = print_stdout;
    session.responder = responder;
    session
}

fn wrap_responder(stream: Option<service::ClientStream>) -> Option<ResponderArc> {
    stream.map(|s| Arc::new(Mutex::new(Some(s))))
}

fn handle_service_command(
    command: ServiceCommand,
    stream: Option<service::ClientStream>,
    session: &Arc<Mutex<PickerSession>>,
    one_shot_sessions: &Arc<Mutex<HashMap<PathBuf, PickerSession>>>,
    runtime_config: &Arc<Mutex<RuntimeConfig>>,
    cx: &mut App,
) {
    let responder = wrap_responder(stream);
    match command {
        ServiceCommand::ToggleWindow => {
            debug!("received toggle-window service command");
            toggle_picker(session, runtime_config, cx)
        }
        ServiceCommand::OpenPath {
            path,
            in_grep,
            query,
        } => {
            debug!(path = %path.display(), in_grep, "received open-path service command");
            let (should_replace, session_snapshot) = match session.lock() {
                Ok(mut current) => {
                    let changed = current.base_path != path;
                    if changed {
                        *current =
                            PickerSession::new(path.clone(), true, in_grep, current.print_stdout);
                    } else {
                        current.start_in_grep = in_grep;
                    }
                    current.responder = responder.clone();
                    (changed, current.clone())
                }
                Err(_) => {
                    let mut s = PickerSession::new(path.clone(), true, in_grep, false);
                    s.responder = responder.clone();
                    (true, s)
                }
            };

            if cx.windows().is_empty() {
                open_window(session_snapshot, query, runtime_config, cx);
            } else if should_replace || query.is_some() {
                close_all_windows(cx);
                open_window(session_snapshot, query, runtime_config, cx);
            }
        }
        ServiceCommand::OpenOneShot {
            path,
            in_grep,
            query,
        } => {
            debug!(path = %path.display(), in_grep, "received one-shot open service command");
            let print_stdout = session
                .lock()
                .map(|session| session.print_stdout)
                .unwrap_or(false);
            open_window(
                one_shot_session(path, one_shot_sessions, in_grep, print_stdout, responder),
                query,
                runtime_config,
                cx,
            );
        }
        ServiceCommand::ReloadConfig => {
            let (config, changed) = refresh_runtime_config(runtime_config);
            if changed {
                apply_key_bindings(cx, &config, true);
                if let Ok(mut state) = runtime_config.lock() {
                    register_global_hotkey(&mut state, &config);
                }
                theme::sync_from_config(&config, cx.window_appearance(), cx);
            }
            info!(changed, "reloaded config on request");
        }
        ServiceCommand::OpenConfig => {
            debug!("received open-config service command");
            open_config_file(runtime_config);
        }
        ServiceCommand::Quit => {
            debug!("received quit service command");
            // A Homebrew service would restart the daemon right away; the LaunchAgent only
            // restarts it on the next request, so it needs no stop.
            if let Err(err) = service::stop_brew_service() {
                info!(error = %err, "failed to stop the Homebrew service");
            }
            cx.quit();
        }
    }
}

async fn drive_service_commands(
    rx: async_channel::Receiver<CommandEnvelope>,
    session: Arc<Mutex<PickerSession>>,
    one_shot_sessions: Arc<Mutex<HashMap<PathBuf, PickerSession>>>,
    runtime_config: Arc<Mutex<RuntimeConfig>>,
    last_activity: Rc<Cell<Instant>>,
    app: AsyncApp,
) {
    while let Ok((command, stream)) = rx.recv().await {
        last_activity.set(Instant::now());
        let _ = app.update(|app| {
            handle_service_command(
                command,
                stream,
                &session,
                &one_shot_sessions,
                &runtime_config,
                app,
            )
        });
    }
}

// Answer the listener's idle checks. The listener stops accepting before the reply that lets
// the daemon exit, so no request is accepted and then dropped by a quitting process.
async fn answer_idle_checks(
    checks: async_channel::Receiver<service::IdleCheck>,
    pending_commands: async_channel::Receiver<CommandEnvelope>,
    last_activity: Rc<Cell<Instant>>,
    timeout: Duration,
    cx: &mut AsyncApp,
) {
    while let Ok(reply) = checks.recv().await {
        let decision = cx.update(|app| {
            let idle_for = last_activity.get().elapsed();
            if !app.windows().is_empty() || !pending_commands.is_empty() {
                Some(timeout)
            } else if idle_for < timeout {
                Some(timeout - idle_for)
            } else {
                None
            }
        });
        let exit = matches!(decision, Ok(None));
        if reply.send(decision.unwrap_or(Some(timeout))).await.is_err() {
            continue;
        }
        if exit {
            info!(idle_secs = timeout.as_secs(), "exiting after idle timeout");
            if let Err(err) = cx.update(|app| app.quit()) {
                info!(error = %err, "failed to quit after idle timeout");
            }
            return;
        }
    }
}

// Launch the resident GPUI service.
fn main() {
    let cli = Cli::parse_from(expand_trailing_help(std::env::args_os().collect()));
    log::init_tracing(debug_requested(&cli));
    configure_allocator_for_low_rss();

    let request = match launch_from_cli(cli) {
        Ok(Some(Launch::Picker(request))) => Some(request),
        Ok(Some(Launch::Daemon)) => None,
        Ok(None) => return,
        Err(err) => {
            eprintln!("fff-gpui: {err:#}");
            std::process::exit(1);
        }
    };
    let daemon = request.is_none();
    let base_path = request
        .as_ref()
        .map_or_else(home_dir, |request| request.path.clone());
    let picker_session = Arc::new(Mutex::new(match &request {
        Some(request) => PickerSession::new(
            request.path.clone(),
            request.path_explicit,
            request.in_grep,
            request.print,
        ),
        None => PickerSession::new(home_dir(), false, false, false),
    }));
    let one_shot_sessions = Arc::new(Mutex::new(HashMap::new()));
    let loaded_config = match config::load_active_config() {
        Ok(loaded) => loaded,
        Err(err) => {
            info!(error = %err, "failed to load config; falling back to defaults");
            config::LoadedConfig {
                path: config::config_path(),
                config: AppConfig::default(),
                warnings: Vec::new(),
            }
        }
    };
    let runtime_config = Arc::new(Mutex::new(RuntimeConfig {
        config: loaded_config.config.clone(),
        config_path: loaded_config.path.clone(),
        hotkey_manager: None,
        registered_hotkey: None,
    }));
    let shell = std::env::var("SHELL").ok();
    let home = std::env::var("HOME").ok();
    let editor = std::env::var("EDITOR").ok();
    let visual = std::env::var("VISUAL").ok();
    let path_env = std::env::var("PATH").ok();
    let cwd = std::env::current_dir().ok();
    let editor_resolution = editor::resolve_editor(&loaded_config.config.editor);

    info!(
        base_path = %base_path.display(),
        config_path = %loaded_config.path.display(),
        global_keybind = ?loaded_config.config.global_keybind,
        "starting fff-gpui"
    );
    match editor_resolution {
        Some((source, ref value)) => {
            info!(editor_source = ?source, editor = %value, "resolved editor");
        }
        None => {
            info!(editor_source = "unset", editor = "", "resolved editor");
        }
    }
    debug!(
        shell = ?shell,
        home = ?home,
        editor = ?editor,
        visual = ?visual,
        path_env = ?path_env,
        cwd = ?cwd,
        "startup environment"
    );

    let mut launchd_listener = None;
    if let Some(request) = &request {
        // Prefer a running daemon: it has the directory indexed already.
        let command = ServiceCommand::OpenOneShot {
            path: request.path.clone(),
            in_grep: request.in_grep,
            query: request.query.clone(),
        };
        match forward_to_running_instance(&command)
            .expect("failed to forward launch request to existing service")
        {
            ForwardOutcome::NoDaemon => info!("no daemon running; showing the picker here"),
            ForwardOutcome::Picked(entries) => {
                info!(count = entries.len(), "received pick response from daemon");
                deliver_picks(entries, request.print, &loaded_config.config.editor);
                return;
            }
        }
    } else {
        launchd_listener =
            service::launchd_listener().expect("failed to adopt the launchd service socket");
        if launchd_listener.is_some() {
            info!("adopted service socket from launchd; this process is the daemon");
        } else if service::daemon_answers() {
            eprintln!("fff-gpui: the daemon is already running");
            std::process::exit(1);
        }
    }

    let (service_tx, service_rx) = async_channel::unbounded::<CommandEnvelope>();
    let idle_timeout = loaded_config.config.idle_exit_secs.map(Duration::from_secs);
    let (idle_check_tx, idle_check_rx) = async_channel::unbounded::<service::IdleCheck>();
    if daemon {
        let idle_exit = idle_timeout.map(|timeout| service::IdleExit {
            timeout,
            checks: idle_check_tx,
        });
        start_listener(service_tx.clone(), launchd_listener, idle_exit)
            .expect("failed to start launch request listener");
        info!("resident service listener started");
    } else {
        info!("running one-shot picker");
    }

    let app = Application::new().with_assets(Assets);

    app.run(move |cx: &mut App| {
        FontAssets::load_fonts(cx).expect("failed to load bundled fonts");
        make_dockless();
        if daemon {
            hotkey::install_event_handler(service_tx.clone());
        }
        if let Ok(state) = runtime_config.lock() {
            theme::sync_from_config(&state.config, cx.window_appearance(), cx);
            let _ = apply_key_bindings(cx, &state.config, false);
        }
        cx.on_action({
            let picker_session = picker_session.clone();
            let runtime_config = runtime_config.clone();
            move |_: &ToggleWindow, cx| toggle_picker(&picker_session, &runtime_config, cx)
        });
        {
            let runtime_config = runtime_config.clone();
            cx.on_action(move |_: &OpenConfig, _cx| {
                info!("config action invoked");
                open_config_file(&runtime_config);
            });
        }
        if daemon {
            cx.spawn({
                let runtime_config = runtime_config.clone();
                async move |cx: &mut AsyncApp| {
                    let _ = cx.update(|_app| {
                        if let Ok(mut state) = runtime_config.lock() {
                            let config = state.config.clone();
                            register_global_hotkey(&mut state, &config);
                        }
                    });
                }
            })
            .detach();
        }
        if let Some(request) = request.clone() {
            open_window(
                one_shot_session(
                    request.path,
                    &one_shot_sessions,
                    request.in_grep,
                    request.print,
                    None,
                ),
                request.query,
                &runtime_config,
                cx,
            );
            cx.on_window_closed(|cx| cx.quit()).detach();
        } else {
            info!("launching without initial picker window");
        }
        if daemon {
            menubar::install(service_tx.clone());

            let last_activity = Rc::new(Cell::new(Instant::now()));
            cx.on_window_closed({
                let last_activity = last_activity.clone();
                move |_| last_activity.set(Instant::now())
            })
            .detach();
            if let Some(timeout) = idle_timeout {
                cx.spawn({
                    let pending_commands = service_rx.clone();
                    let last_activity = last_activity.clone();
                    async move |cx: &mut AsyncApp| {
                        answer_idle_checks(
                            idle_check_rx,
                            pending_commands,
                            last_activity,
                            timeout,
                            cx,
                        )
                        .await
                    }
                })
                .detach();
            }

            cx.spawn(move |cx: &mut AsyncApp| {
                drive_service_commands(
                    service_rx,
                    picker_session.clone(),
                    one_shot_sessions.clone(),
                    runtime_config.clone(),
                    last_activity.clone(),
                    cx.clone(),
                )
            })
            .detach();
        }
    });
}
