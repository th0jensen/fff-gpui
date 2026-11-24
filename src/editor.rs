#[cfg(unix)]
use std::os::unix::process::CommandExt as _;
use std::path::Path;
use std::process::{Child, Command};

use anyhow::{Context as _, anyhow};
use tracing::{debug, info, instrument};

fn env_editor(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .and_then(|value| (!value.trim().is_empty()).then_some(value))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorSource {
    EnvEditor,
    EnvVisual,
    Config,
}

// `Detached` puts the child in its own process group so a fire-and-forget
// daemon-side spawn survives the parent exiting. `Foreground` leaves the
// child in the parent's PG so terminal editors (hx, nvim, vim, …) can drive
// the controlling TTY — used by the terminal-launched client that calls
// `child.wait()`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorLaunchMode {
    Foreground,
    Detached,
}

pub fn resolve_editor(config_editor: &str) -> Option<(EditorSource, String)> {
    env_editor("EDITOR")
        .map(|editor| (EditorSource::EnvEditor, editor))
        .or_else(|| env_editor("VISUAL").map(|editor| (EditorSource::EnvVisual, editor)))
        .or_else(|| {
            let editor = config_editor.trim();
            (!editor.is_empty()).then(|| (EditorSource::Config, editor.to_owned()))
        })
}

// Spawn the user's $EDITOR or $VISUAL, falling back to config.editor when needed.
#[instrument(skip(path, config_editor), fields(path = %path.display(), goto = ?goto, mode = ?mode))]
pub fn open_in_editor(
    path: &Path,
    goto: Option<(usize, usize)>,
    config_editor: &str,
    mode: EditorLaunchMode,
) -> anyhow::Result<Child> {
    let editor = resolve_editor(config_editor).map(|(_, editor)| editor).ok_or_else(|| {
        anyhow!(
            "Neither $EDITOR nor $VISUAL nor config editor is set; refusing to guess an editor for {}",
            path.display()
        )
    })?;

    info!(editor = ?editor, "opening file with editor");

    let mut cmd = editor_command(&editor, path, goto);
    // Launchd sets this for the daemon; an editor that inherits it passes it to its terminals,
    // where fff-gpui would then mistake itself for a launchd job.
    cmd.env_remove("XPC_SERVICE_NAME");
    #[cfg(unix)]
    if matches!(mode, EditorLaunchMode::Detached) {
        cmd.process_group(0);
    }
    cmd.spawn()
        .with_context(|| format!("failed to spawn editor command {:?}", editor))
}

#[derive(Clone, Copy)]
enum LineFormat {
    // vim/nvim/emacs/nano: `editor +line path`
    Plus,
    // zed/subl/mate: `editor path:line:column` (positional)
    Colon,
    // code/cursor/codium: `editor -g path:line:column` (needs --goto flag)
    GotoFlag,
}

fn line_format(editor: &str) -> LineFormat {
    let bin = editor.split_whitespace().next().unwrap_or("");
    let basename = Path::new(bin)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(bin)
        .to_lowercase();
    match basename.as_str() {
        "zed" | "zed-preview" | "subl" | "sublime_text" | "mate" => LineFormat::Colon,
        "code" | "code-insiders" | "cursor" | "codium" | "vscodium" => LineFormat::GotoFlag,
        _ => LineFormat::Plus,
    }
}

// Build a shell command when the editor contains arguments, otherwise run it directly.
fn editor_command(editor: &str, path: &Path, goto: Option<(usize, usize)>) -> Command {
    let format = line_format(editor);
    let has_args = editor.split_whitespace().nth(1).is_some();

    if has_args {
        debug!(editor = ?editor, "editor command uses shell wrapper");
        let mut command = Command::new("sh");
        match (format, goto) {
            (LineFormat::Plus, Some((line, _))) => {
                command
                    .arg("-c")
                    .arg(format!("exec {editor} +{line} \"$1\""))
                    .arg("fff-editor")
                    .arg(path);
            }
            (LineFormat::Colon, Some((line, column))) => {
                command
                    .arg("-c")
                    .arg(format!("exec {editor} \"$1\""))
                    .arg("fff-editor")
                    .arg(format!("{}:{line}:{column}", path.display()));
            }
            (LineFormat::GotoFlag, Some((line, column))) => {
                command
                    .arg("-c")
                    .arg(format!("exec {editor} -g \"$1\""))
                    .arg("fff-editor")
                    .arg(format!("{}:{line}:{column}", path.display()));
            }
            (_, None) => {
                command
                    .arg("-c")
                    .arg(format!("exec {editor} \"$1\""))
                    .arg("fff-editor")
                    .arg(path);
            }
        }
        command
    } else {
        let mut command = Command::new(editor);
        match (format, goto) {
            (LineFormat::Plus, Some((line, _))) => {
                command.arg(format!("+{line}")).arg(path);
            }
            (LineFormat::Colon, Some((line, column))) => {
                command.arg(format!("{}:{line}:{column}", path.display()));
            }
            (LineFormat::GotoFlag, Some((line, column))) => {
                command
                    .arg("-g")
                    .arg(format!("{}:{line}:{column}", path.display()));
            }
            (_, None) => {
                command.arg(path);
            }
        }
        command
    }
}
