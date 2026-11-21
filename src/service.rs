use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context as _, Result};
use async_channel::Sender;
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
    },
    OpenConfig,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_open_one_shot() {
        let cmd = ServiceCommand::OpenOneShot {
            path: PathBuf::from("/tmp/foo"),
            in_grep: true,
        };
        let s = serde_json::to_string(&cmd).unwrap();
        let back: ServiceCommand = serde_json::from_str(&s).unwrap();
        match back {
            ServiceCommand::OpenOneShot { path, in_grep } => {
                assert_eq!(path, PathBuf::from("/tmp/foo"));
                assert!(in_grep);
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
    fn pick_response_empty_round_trip() {
        let resp = PickResponse::default();
        let s = serde_json::to_string(&resp).unwrap();
        let back: PickResponse = serde_json::from_str(&s).unwrap();
        assert!(back.paths.is_empty());
    }
}
