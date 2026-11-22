use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_channel::Sender;
use indoc::formatdoc;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

#[cfg(unix)]
pub type ClientStream = std::os::unix::net::UnixStream;
#[cfg(not(unix))]
pub type ClientStream = std::convert::Infallible;

#[cfg(unix)]
pub type ServiceListener = std::os::unix::net::UnixListener;
#[cfg(not(unix))]
pub type ServiceListener = std::convert::Infallible;

pub type CommandEnvelope = (ServiceCommand, Option<ClientStream>);

/// Reply channel for an idle check: `None` lets the daemon exit now, `Some` asks the listener
/// to check again after that long.
pub type IdleCheck = Sender<Option<Duration>>;

pub struct IdleExit {
    pub timeout: Duration,
    pub checks: Sender<IdleCheck>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum ServiceCommand {
    OpenPath {
        path: PathBuf,
        #[serde(default)]
        in_grep: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    OpenOneShot {
        path: PathBuf,
        #[serde(default)]
        in_grep: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        query: Option<String>,
    },
    OpenConfig,
    ReloadConfig,
    ToggleWindow,
    Quit,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PickEntry {
    pub path: PathBuf,
    #[serde(default)]
    pub line: Option<usize>,
    #[serde(default)]
    pub column: Option<usize>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PickResponse {
    pub paths: Vec<PickEntry>,
}

#[derive(Debug)]
pub enum ForwardOutcome {
    NoDaemon,
    Picked(Vec<PickEntry>),
}

fn socket_path() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"))
        .join(".local/state/fff-gpui/fff-gpui.sock")
}

#[cfg(unix)]
fn read_request(line: &str) -> Option<ServiceCommand> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }
    match serde_json::from_str::<ServiceCommand>(trimmed) {
        Ok(command) => Some(command),
        Err(err) => {
            warn!(error = %err, line = %trimmed, "failed to parse service request");
            None
        }
    }
}

fn write_request(stream: &mut impl Write, command: &ServiceCommand) -> Result<()> {
    let payload = serde_json::to_string(command).context("failed to serialize launch request")?;
    writeln!(stream, "{payload}").context("failed to send launch request")
}

/// Forward this launch to a running service instance and wait for the picker's response.
///
/// On unix: writes the request to the daemon's socket, then reads back a single JSON
/// `PickResponse` line. The client uses the response to spawn editors itself, which is what
/// makes terminal editors (nvim, helix, etc.) work — the daemon has no TTY but the client does.
#[instrument(skip(command), fields(command = ?command))]
pub fn forward_to_running_instance(command: &ServiceCommand) -> Result<ForwardOutcome> {
    #[cfg(unix)]
    {
        use std::os::unix::net::UnixStream;

        let path = socket_path();
        debug!(socket = %path.display(), "trying to connect to resident service");
        match UnixStream::connect(&path) {
            Ok(mut stream) => {
                write_request(&mut stream, command)?;
                let _ = stream.flush();
                info!("forwarded launch request to resident service");

                let reader = BufReader::new(&stream);
                let entries = read_pick_response(reader);
                Ok(ForwardOutcome::Picked(entries))
            }
            Err(err) => {
                debug!(error = %err, "no resident service available");
                Ok(ForwardOutcome::NoDaemon)
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = command;
        Ok(ForwardOutcome::NoDaemon)
    }
}

#[cfg(unix)]
fn read_pick_response<R: BufRead>(mut reader: R) -> Vec<PickEntry> {
    let mut line = String::new();
    match reader.read_line(&mut line) {
        Ok(0) => {
            debug!("daemon closed without response");
            Vec::new()
        }
        Ok(_) => match serde_json::from_str::<PickResponse>(line.trim()) {
            Ok(resp) => resp.paths,
            Err(err) => {
                warn!(error = %err, line = %line.trim(), "failed to parse pick response");
                Vec::new()
            }
        },
        Err(err) => {
            warn!(error = %err, "failed to read pick response from daemon");
            Vec::new()
        }
    }
}

/// Take over the socket launchd created for a LaunchAgent that declares a `Listeners` socket.
///
/// Returns `None` when this process was not started by such a job, so the caller binds the
/// socket itself.
#[cfg(target_os = "macos")]
pub fn launchd_listener() -> Result<Option<ServiceListener>> {
    use std::ffi::{c_char, c_int, c_void};
    use std::os::fd::{FromRawFd, OwnedFd};
    use std::os::unix::net::UnixListener;

    unsafe extern "C" {
        fn launch_activate_socket(
            name: *const c_char,
            fds: *mut *mut c_int,
            count: *mut usize,
        ) -> c_int;
        fn free(pointer: *mut c_void);
    }

    let mut fds: *mut c_int = std::ptr::null_mut();
    let mut count = 0usize;
    let status = unsafe { launch_activate_socket(c"Listeners".as_ptr(), &mut fds, &mut count) };
    if status != 0 {
        debug!(status, "no launchd socket to adopt");
        return Ok(None);
    }
    if fds.is_null() {
        anyhow::bail!("launchd returned no file descriptors for the Listeners socket");
    }
    let owned: Vec<OwnedFd> = unsafe { std::slice::from_raw_parts(fds, count) }
        .iter()
        .map(|&fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect();
    unsafe { free(fds.cast()) };

    let fd = owned
        .into_iter()
        .next()
        .context("launchd returned no file descriptors for the Listeners socket")?;
    let listener = UnixListener::from(fd);
    // The accept loop needs blocking accepts; do not depend on the mode launchd chose.
    listener
        .set_nonblocking(false)
        .context("failed to make the launchd socket blocking")?;
    Ok(Some(listener))
}

#[cfg(not(target_os = "macos"))]
pub fn launchd_listener() -> Result<Option<ServiceListener>> {
    Ok(None)
}

/// Wait until a client connects or `timeout` passes; returns false on timeout.
#[cfg(unix)]
fn wait_for_connection(listener: &ServiceListener, timeout: Duration) -> bool {
    use std::ffi::{c_int, c_short};
    use std::os::fd::AsRawFd;

    #[repr(C)]
    struct PollFd {
        fd: c_int,
        events: c_short,
        revents: c_short,
    }
    #[cfg(target_os = "linux")]
    type Nfds = std::ffi::c_ulong;
    #[cfg(not(target_os = "linux"))]
    type Nfds = std::ffi::c_uint;
    unsafe extern "C" {
        fn poll(fds: *mut PollFd, nfds: Nfds, timeout: c_int) -> c_int;
    }
    const POLLIN: c_short = 0x1;

    let mut poll_fd = PollFd {
        fd: listener.as_raw_fd(),
        events: POLLIN,
        revents: 0,
    };
    let millis = c_int::try_from(timeout.as_nanos().div_ceil(1_000_000)).unwrap_or(c_int::MAX);
    loop {
        match unsafe { poll(&mut poll_fd, 1, millis) } {
            0 => return false,
            -1 => {
                let err = std::io::Error::last_os_error();
                if err.kind() == std::io::ErrorKind::Interrupted {
                    continue;
                }
                // Fall back to a blocking accept rather than spinning on a broken poll.
                warn!(error = %err, "poll on service socket failed");
                return true;
            }
            _ => return true,
        }
    }
}

/// Ask the app whether the daemon may exit; see `IdleCheck`.
fn check_idle(idle: &IdleExit) -> Option<Duration> {
    let (reply_tx, reply_rx) = async_channel::bounded(1);
    if idle.checks.send_blocking(reply_tx).is_err() {
        return Some(idle.timeout);
    }
    reply_rx.recv_blocking().unwrap_or(Some(idle.timeout))
}

/// Start the background listener that receives launch requests from subsequent invocations.
///
/// The listener keeps each accepted stream alive after parsing the request: the stream is
/// passed alongside the command so the picker can later write back a `PickResponse` on the
/// same connection. For non-Open commands (toggle/show/quit/config) the consumer simply
/// drops the stream, which closes the connection.
///
/// `adopted` is a listener from `launchd_listener`; without one, the socket is bound here.
///
/// With `idle_exit`, the listener asks the app whether to exit after `timeout` without a
/// connection, and stops accepting before the app quits. A request that arrives afterwards
/// stays in the socket backlog, where launchd sees it and starts a new daemon, instead of being
/// accepted by a process that is about to exit.
#[instrument(skip(commands, adopted, idle_exit))]
pub fn start_listener(
    commands: Sender<CommandEnvelope>,
    adopted: Option<ServiceListener>,
    idle_exit: Option<IdleExit>,
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::fs;
        use std::os::unix::net::UnixListener;

        let (listener, bound_path) = match adopted {
            Some(listener) => (listener, None),
            None => {
                let path = socket_path();
                if let Some(parent) = path.parent() {
                    fs::create_dir_all(parent).context("failed to create service directory")?;
                }
                let _ = fs::remove_file(&path);
                let listener = UnixListener::bind(&path).with_context(|| {
                    format!("failed to bind service socket at {}", path.display())
                })?;
                info!(socket = %path.display(), "service listener bound");
                (listener, Some(path))
            }
        };

        thread::Builder::new()
            .name("fff-gpui-ipc".to_string())
            .spawn(move || {
                let mut wait = idle_exit.as_ref().map(|idle| idle.timeout);
                loop {
                    if let (Some(idle), Some(duration)) = (&idle_exit, wait) {
                        if !wait_for_connection(&listener, duration) {
                            wait = check_idle(idle);
                            if wait.is_none() {
                                info!("stopped accepting service requests after idle timeout");
                                // Without launchd nobody answers this path anymore; remove it so
                                // clients see a missing socket instead of a refused connection.
                                if let Some(path) = &bound_path
                                    && let Err(err) = fs::remove_file(path)
                                {
                                    warn!(error = %err, "failed to remove service socket");
                                }
                                return;
                            }
                            continue;
                        }
                        wait = Some(idle.timeout);
                    }
                    let Ok((stream, _)) = listener.accept() else {
                        warn!("incoming service request stream failed");
                        continue;
                    };
                    let mut reader = BufReader::new(stream);
                    let mut line = String::new();
                    if reader.read_line(&mut line).is_ok()
                        && let Some(request) = read_request(&line)
                    {
                        debug!(?request, "received service request");
                        let stream = reader.into_inner();
                        let _ = commands.send_blocking((request, Some(stream)));
                    }
                }
            })
            .context("failed to spawn launch request listener")?;
    }

    #[cfg(not(unix))]
    {
        let _ = (commands, adopted, idle_exit);
    }

    Ok(())
}

#[cfg(target_os = "macos")]
const LAUNCH_AGENT_LABEL: &str = "com.github.th0jensen.fff-gpui";

#[cfg(target_os = "macos")]
fn launch_agents_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
        .join("Library/LaunchAgents")
}

#[cfg(target_os = "macos")]
fn launch_agent_path() -> PathBuf {
    launch_agents_dir().join(format!("{LAUNCH_AGENT_LABEL}.plist"))
}

#[cfg(target_os = "macos")]
fn launchctl(args: &[&str]) -> Result<std::process::Output> {
    std::process::Command::new("/bin/launchctl")
        .args(args)
        .output()
        .context("failed to run launchctl")
}

#[cfg(target_os = "macos")]
fn launchctl_checked(args: &[&str]) -> Result<()> {
    let output = launchctl(args)?;
    if !output.status.success() {
        anyhow::bail!(
            "launchctl {} failed: {}",
            args.first().copied().unwrap_or_default(),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn job_target() -> String {
    format!("{}/{LAUNCH_AGENT_LABEL}", gui_domain())
}

#[cfg(target_os = "macos")]
enum JobState {
    NotLoaded,
    Waiting,
    Running(u32),
}

#[cfg(target_os = "macos")]
fn job_state() -> Result<JobState> {
    let output = launchctl(&["print", &job_target()])?;
    if !output.status.success() {
        return Ok(JobState::NotLoaded);
    }
    // Only the job's own top-level `pid` line, not those of nested entries.
    let pid = String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.strip_prefix("\tpid = ")?.trim().parse().ok());
    Ok(pid.map_or(JobState::Waiting, JobState::Running))
}

#[cfg(target_os = "macos")]
fn bootstrap_launch_agent() -> Result<()> {
    launchctl_checked(&[
        "bootstrap",
        &gui_domain(),
        &launch_agent_path().to_string_lossy(),
    ])
}

/// Whether something answers on the service socket. Under a loaded LaunchAgent this also makes
/// launchd start the daemon, so check `launchd_job_waiting` first when that is unwanted.
pub fn daemon_answers() -> bool {
    #[cfg(unix)]
    {
        std::os::unix::net::UnixStream::connect(socket_path()).is_ok()
    }
    #[cfg(not(unix))]
    {
        false
    }
}

/// Whether the LaunchAgent is loaded but its daemon is not running yet.
#[cfg(target_os = "macos")]
pub fn launchd_job_waiting() -> Result<bool> {
    Ok(matches!(job_state()?, JobState::Waiting))
}

#[cfg(not(target_os = "macos"))]
pub fn launchd_job_waiting() -> Result<bool> {
    Ok(false)
}

/// Start the daemon: through launchd when the LaunchAgent is installed, otherwise as a
/// detached `fff-gpui server run`.
#[cfg(target_os = "macos")]
pub fn start_service() -> Result<()> {
    use std::os::unix::process::CommandExt;
    use std::process::Stdio;

    if launch_agent_path().exists() {
        if let JobState::NotLoaded = job_state()? {
            bootstrap_launch_agent()?;
        }
        return launchctl_checked(&["kickstart", &job_target()]);
    }
    if daemon_answers() {
        return Ok(());
    }
    let exe = std::env::current_exe().context("failed to locate the fff-gpui binary")?;
    std::process::Command::new(exe)
        .args(["server", "run"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        .spawn()
        .context("failed to start the daemon")?;
    for _ in 0..100 {
        if daemon_answers() {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(100));
    }
    anyhow::bail!("the daemon did not start; see `fff-gpui server logs`")
}

/// Ask the running daemon to quit and wait for it; returns false when nothing was running.
#[cfg(target_os = "macos")]
pub fn stop_service() -> Result<bool> {
    let pid = match job_state()? {
        // Connecting to launchd's socket now would start the daemon just to stop it.
        JobState::Waiting => return Ok(false),
        JobState::Running(pid) => Some(pid),
        JobState::NotLoaded => None,
    };
    if let ForwardOutcome::NoDaemon = forward_to_running_instance(&ServiceCommand::Quit)? {
        return Ok(false);
    }
    for _ in 0..50 {
        let stopped = match pid {
            Some(pid) => !matches!(job_state()?, JobState::Running(current) if current == pid),
            None => !daemon_answers(),
        };
        if stopped {
            break;
        }
        thread::sleep(Duration::from_millis(100));
    }
    Ok(true)
}

#[cfg(target_os = "macos")]
pub fn restart_service() -> Result<()> {
    if !launch_agent_path().exists() {
        stop_service()?;
        return start_service();
    }
    match job_state()? {
        // RunAtLoad starts the daemon as part of loading.
        JobState::NotLoaded => bootstrap_launch_agent(),
        JobState::Waiting | JobState::Running(_) => {
            launchctl_checked(&["kickstart", "-k", &job_target()])
        }
    }
}

#[cfg(target_os = "macos")]
pub fn service_status() -> Result<String> {
    let state = job_state()?;
    let service = match (launch_agent_path().exists(), &state) {
        (false, _) => "no LaunchAgent (`fff-gpui server install` starts it at login)",
        (true, JobState::NotLoaded) => "installed, not loaded (run `fff-gpui server start`)",
        (true, _) => "installed",
    };
    let daemon = match state {
        JobState::Running(pid) => format!("running (pid {pid})"),
        JobState::Waiting => "not running; starts on the next request".to_string(),
        JobState::NotLoaded if daemon_answers() => "running outside launchd".to_string(),
        JobState::NotLoaded => "not running".to_string(),
    };
    Ok(format!(
        "service: {service}\ndaemon:  {daemon}\nsocket:  {}\nlog:     {}\n",
        socket_path().display(),
        crate::log::log_path().display()
    ))
}

#[cfg(target_os = "macos")]
fn gui_domain() -> String {
    unsafe extern "C" {
        fn getuid() -> u32;
    }
    format!("gui/{}", unsafe { getuid() })
}

#[cfg(target_os = "macos")]
fn service_program() -> Result<PathBuf> {
    let exe = std::env::current_exe()
        .and_then(std::fs::canonicalize)
        .context("failed to locate the fff-gpui binary")?;
    // Homebrew's Cellar path changes on every upgrade; its opt link does not.
    if let Some((prefix, _)) = exe.to_str().and_then(|path| path.split_once("/Cellar/")) {
        let opt = PathBuf::from(prefix).join("opt/fff-gpui/bin/fff-gpui");
        if opt.exists() {
            return Ok(opt);
        }
    }
    Ok(exe)
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

fn launch_agent_plist(label: &str, program: &str, socket: &str) -> String {
    formatdoc!(
        r#"
        <?xml version="1.0" encoding="UTF-8"?>
        <!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
        <plist version="1.0">
        <dict>
          <key>Label</key>
          <string>{label}</string>
          <key>ProgramArguments</key>
          <array>
            <string>{program}</string>
            <string>server</string>
            <string>run</string>
          </array>
          <key>RunAtLoad</key>
          <true/>
          <key>Sockets</key>
          <dict>
            <key>Listeners</key>
            <dict>
              <key>SockPathName</key>
              <string>{socket}</string>
              <key>SockPathMode</key>
              <integer>384</integer>
            </dict>
          </dict>
        </dict>
        </plist>
        "#,
        label = xml_escape(label),
        program = xml_escape(program),
        socket = xml_escape(socket),
    )
}

/// Install and load a LaunchAgent that owns the service socket, so launchd starts the daemon
/// on the first request. Replaces an earlier install and the Homebrew service, and stops a
/// daemon started by hand.
#[cfg(target_os = "macos")]
pub fn install_launch_agent() -> Result<PathBuf> {
    use std::fs;

    // The Homebrew service starts the daemon at login and keeps it alive, which would hold the
    // socket and defeat on-demand start.
    if stop_brew_service()? {
        info!("stopped the Homebrew service");
    }

    let program = service_program()?;
    let socket = socket_path();
    let plist_path = launch_agent_path();

    // Not loaded yet is fine; a loaded job must go so launchd re-reads the plist.
    launchctl(&["bootout", &job_target()])?;
    if let ForwardOutcome::Picked(_) = forward_to_running_instance(&ServiceCommand::Quit)? {
        info!("stopped the running daemon");
    }

    if let Some(parent) = socket.parent() {
        fs::create_dir_all(parent).context("failed to create service directory")?;
    }
    match fs::remove_file(&socket) {
        Ok(()) => {}
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => return Err(err).context("failed to remove the old service socket"),
    }
    fs::create_dir_all(launch_agents_dir()).context("failed to create ~/Library/LaunchAgents")?;
    fs::write(
        &plist_path,
        launch_agent_plist(
            LAUNCH_AGENT_LABEL,
            &program.to_string_lossy(),
            &socket.to_string_lossy(),
        ),
    )
    .with_context(|| format!("failed to write {}", plist_path.display()))?;

    bootstrap_launch_agent()?;
    Ok(plist_path)
}

#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
const BREW: &str = "/opt/homebrew/bin/brew";
#[cfg(all(target_os = "macos", not(target_arch = "aarch64")))]
const BREW: &str = "/usr/local/bin/brew";

/// Stop and unregister the `brew services` job if it is installed; returns whether it was.
#[cfg(target_os = "macos")]
pub fn stop_brew_service() -> Result<bool> {
    if !launch_agents_dir()
        .join("homebrew.mxcl.fff-gpui.plist")
        .exists()
    {
        return Ok(false);
    }
    let output = std::process::Command::new(BREW)
        .args(["services", "stop", "fff-gpui"])
        .output()
        .with_context(|| format!("failed to run {BREW} services stop fff-gpui"))?;
    if !output.status.success() {
        anyhow::bail!(
            "brew services stop fff-gpui failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(true)
}

#[cfg(not(target_os = "macos"))]
pub fn stop_brew_service() -> Result<bool> {
    Ok(false)
}

#[cfg(target_os = "macos")]
pub fn uninstall_launch_agent() -> Result<()> {
    use std::fs;

    let plist_path = launch_agent_path();
    let output = launchctl(&["bootout", &job_target()])?;
    if !output.status.success() && !plist_path.exists() {
        anyhow::bail!("the fff-gpui LaunchAgent is not installed");
    }
    for path in [plist_path, socket_path()] {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => {
                return Err(err).with_context(|| format!("failed to remove {}", path.display()));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_open_one_shot() {
        let cmd = ServiceCommand::OpenOneShot {
            path: PathBuf::from("/tmp/foo"),
            in_grep: true,
            query: Some("TODO".to_string()),
        };
        let s = serde_json::to_string(&cmd).unwrap();
        let back: ServiceCommand = serde_json::from_str(&s).unwrap();
        match back {
            ServiceCommand::OpenOneShot {
                path,
                in_grep,
                query,
            } => {
                assert_eq!(path, PathBuf::from("/tmp/foo"));
                assert!(in_grep);
                assert_eq!(query.as_deref(), Some("TODO"));
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[test]
    fn round_trip_open_path_minimal() {
        let cmd = ServiceCommand::OpenPath {
            path: PathBuf::from("/home/u/proj"),
            in_grep: false,
            query: None,
        };
        let s = serde_json::to_string(&cmd).unwrap();
        assert!(!s.contains("query"));
        let back: ServiceCommand = serde_json::from_str(&s).unwrap();
        assert!(matches!(
            back,
            ServiceCommand::OpenPath { in_grep: false, .. }
        ));
    }

    #[test]
    fn round_trip_unit_variants() {
        for cmd in [
            ServiceCommand::OpenConfig,
            ServiceCommand::ReloadConfig,
            ServiceCommand::ToggleWindow,
            ServiceCommand::Quit,
        ] {
            let s = serde_json::to_string(&cmd).unwrap();
            let back: ServiceCommand = serde_json::from_str(&s).unwrap();
            assert_eq!(std::mem::discriminant(&cmd), std::mem::discriminant(&back));
        }
    }

    #[test]
    fn parse_accepts_missing_in_grep() {
        let s = r#"{"cmd":"open_path","path":"/x"}"#;
        let cmd: ServiceCommand = serde_json::from_str(s).unwrap();
        match cmd {
            ServiceCommand::OpenPath {
                path,
                in_grep,
                query,
            } => {
                assert_eq!(path, PathBuf::from("/x"));
                assert!(!in_grep);
                assert!(query.is_none());
            }
            other => panic!("unexpected variant: {other:?}"),
        }
    }

    #[cfg(unix)]
    #[test]
    fn open_path_preserves_query() {
        for (in_grep, query) in [
            (false, "**/*.{ts,tsx} git:modified"),
            (true, "TODO|FIXME"),
            (false, ""),
            (true, " café \"quoted\" \\path\nnext "),
        ] {
            let request = serde_json::json!({
                "cmd": "open_path",
                "path": "/project",
                "in_grep": in_grep,
                "query": query,
            });
            let command = read_request(&format!("{request}\n")).unwrap();
            let mut wire = Vec::new();
            write_request(&mut wire, &command).unwrap();
            let wire = String::from_utf8(wire).unwrap();
            assert_eq!(wire.lines().count(), 1);
            let actual: serde_json::Value = serde_json::from_str(&wire).unwrap();
            assert_eq!(actual, request);
        }
    }

    #[test]
    fn pick_response_round_trip() {
        let resp = PickResponse {
            paths: vec![
                PickEntry {
                    path: PathBuf::from("/a/b.rs"),
                    line: Some(12),
                    column: Some(4),
                },
                PickEntry {
                    path: PathBuf::from("/a/c.rs"),
                    line: None,
                    column: None,
                },
            ],
        };
        let s = serde_json::to_string(&resp).unwrap();
        let back: PickResponse = serde_json::from_str(&s).unwrap();
        assert_eq!(back.paths.len(), 2);
        assert_eq!(back.paths[0].path, PathBuf::from("/a/b.rs"));
        assert_eq!(back.paths[0].line, Some(12));
        assert_eq!(back.paths[0].column, Some(4));
        assert!(back.paths[1].line.is_none());
    }

    #[test]
    fn launch_agent_plist_escapes_paths() {
        let plist = launch_agent_plist("label", "/Apps/A&B <x>/fff-gpui", "/sock");
        assert!(plist.contains("<string>/Apps/A&amp;B &lt;x&gt;/fff-gpui</string>"));
        assert!(!plist.contains("A&B"));
    }

    #[test]
    fn pick_response_empty_round_trip() {
        let resp = PickResponse::default();
        let s = serde_json::to_string(&resp).unwrap();
        let back: PickResponse = serde_json::from_str(&s).unwrap();
        assert!(back.paths.is_empty());
    }
}
