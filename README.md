# fff-gpui

A fast, keyboard-driven file finder for macOS built on [GPUI](https://github.com/zed-industries/zed/tree/main/crates/gpui) — the same UI framework that powers [Zed](https://zed.dev). It runs as a system-wide overlay you can summon instantly with a keybind, and integrates seamlessly into Zed as a custom task.

Under the hood it uses [fff](https://crates.io/crates/fff-search) for fuzzy file search and grep, with frecency-based ranking so the files you actually use rise to the top.

<img width="1072" height="633" alt="Screenshot 2026-05-05 at 4 49 11 PM" src="https://github.com/user-attachments/assets/dec3811f-2de0-4bd3-b90b-812d9c36d124" />

## Features

- Fuzzy file search and grep across your project
- Frecency ranking — frequently and recently opened files are prioritised
- Syntax-highlighted file preview
- Global keybind support for system-wide access
- Deep Zed integration via custom tasks — works across all projects

<details>
<summary>
<h2>Installation</h2>
</summary>

### Homebrew (recommended)

Apple Silicon and Intel supported
```sh
brew tap th0jensen/fff-gpui
brew install fff-gpui
brew services start fff-gpui
```

### Build from source

**Requirements:**
- macOS (Apple Silicon and Intel)
- Latest stable Rust via [rustup](https://rustup.rs)
- Xcode Command Line Tools (`xcode-select --install`)
- CMake ([required by wasmtime](https://docs.rs/wasmtime-c-api-impl/latest/wasmtime_c_api/))
- Zig 0.16.0 ([required by zlob](https://crates.io/crates/zlob))

To compile without Zig, disable zlob in `Cargo.toml`. This will lead to slightly slower performance, but it's not required for the app to work.

```toml
+ fff-search = "0.6"
+ fff-query-parser = "0.6"
- fff-search = { version = "0.6", features = ["zlob"] }
- fff-query-parser = { version = "0.6", features = ["zlob"] }
```

```sh
git clone https://github.com/th0jensen/fff-gpui
cd fff-gpui
cargo build --release
```

The binary will be at `target/release/fff-gpui`. You can move it anywhere on your `$PATH` or reference it directly in your config.

Having trouble building? Check Zed's [macOS troubleshooting guide](https://zed.dev/docs/development/macos#troubleshooting) — the build requirements are the same.

### Test a pull request build

Pull requests run **macOS Test Build**, which tests and builds release binaries
for Apple Silicon (`arm64`) and Intel (`x86_64`). Open the workflow run from the
PR's checks and download the artifact for your Mac. Artifacts expire after seven
days and require a GitHub login to download. The workflow also supports manual
runs from the Actions tab once it is on the default branch.

Unzip the Actions download, then extract its `.tar.gz` archive. The archive
preserves executable permissions and includes `BUILD.txt` with the tested commit,
target, and workflow run. PR builds test the merge commit, not just the branch tip.

```sh
tar -xzf fff-gpui-macos-*.tar.gz
./fff-gpui --version
./fff-gpui --open /path/to/project
```

Stop any existing fff-gpui daemon before testing so requests do not go to the
installed version. For Homebrew, use `brew services stop fff-gpui`. To test the
[Unix socket integration](#unix-socket-integrations), run `./fff-gpui` without
`--open` to start the test daemon, then send the documented requests. Quit the
test daemon and restore your normal service when finished.

These builds are ad-hoc signed, but not notarized. If macOS blocks the binary,
review the PR's source before using **System Settings → Privacy & Security →
Open Anyway**. Test builds do not publish a release or replace your installed
binary.
</details>

<details>
<summary>
<h2>Configuration</h2>
</summary>

Set options in `~/.config/fff-gpui/config.toml`:

```toml
editor = "/usr/local/bin/zed" # run `which $EDITOR` in your shell to find this path
sync_zed_settings = true
global_keybind = "hyper+f"
base_dir = "~/Developer" # sets the default base_dir when launching from global keybind
exclude_dirs = [
    "Library",
    "node_modules"
]
window_width = 960.0
window_height = 520.0
picker_pane_width = 430.0

[font]
ui_family = ".SystemUIFont"
buffer_family = "UbuntuMono Nerd Font"
ui_size = 16.0
buffer_size = 15.0

[theme]
name = "One Dark"
```

`editor` is a fallback for the resident Homebrew service, which does not inherit your shell environment. If `EDITOR` or `VISUAL` is present in the current process, those still win, so custom tasks and other integrations can keep overriding it naturally.

When `sync_zed_settings` is enabled, fff-gpui reads Zed's `settings.json` and mirrors the UI font, buffer font, font sizes, light/dark theme selection, and theme colors — from the bundled Zed themes plus any installed or local Zed theme.

Explicit config values still win, so you can keep Zed sync enabled and override just the theme, fonts, sizes, or specific colors when needed. In practice, `[theme].name` overrides Zed's chosen theme, and `[font]` overrides the synced font families and sizes.

For `global_keybind`, `hyper` is accepted as a shorthand for `shift+control+alt+super`.

`exclude_dirs` takes an array of directory paths. Relative entries are resolved from the current base path, so `["Library"]` excludes that directory anywhere under the opened scope. We do not currently support wildcard globbing here. In practice, plain directory names are enough for the system-wide picker use case this option targets, and they keep the config predictable.

Zed themes are discovered from the bundled theme set, your local Zed installation, and extension themes under `~/Library/Application Support/Zed/extensions/installed/`.

### Themes

fff-gpui comes bundled with the same themes as Zed. These are the valid options:

|       | Ayu        | Gruvbox            | One      |
|-------|------------|--------------------|----------|
| Dark  | Ayu Dark, Ayu Mirage | Gruvbox Dark, Gruvbox Dark Hard, Gruvbox Dark Soft | One Dark |
| Light | Ayu Light  | Gruvbox Light, Gruvbox Light Hard, Gruvbox Light Soft | One Light |

</details>

<details>
<summary>
<h2>Running</h2>
</summary>

Launch fff-gpui once to start it as a background service with your global keybind:

```sh
fff-gpui
```

If installed via Homebrew, `brew services start fff-gpui` handles this and re-launches it at login automatically.


Launch fff-gpui with the `--open <path>` flag if you don't want the daemon running:

```sh
fff-gpui --open <path>
```

The daemon has a memory footprint of ~150mb idle and ~400mb when actively searching. Most of the reported usage in tools like btop is macOS page reservation and will be reclaimed under memory pressure. Report issues if you experience sustained growth beyond these baselines.

### Unix socket integrations

The daemon accepts newline-delimited JSON at
`~/.local/state/fff-gpui/fff-gpui.sock`. Use `open_path` to open a directory.
The optional `query` string pre-fills the search bar and starts the search when
the file index is ready:

```json
{"cmd":"open_path","path":"/my/project","in_grep":false,"query":"**/*.{ts,tsx} git:modified"}
```

```json
{"cmd":"open_path","path":"/my/project","in_grep":true,"query":"TODO"}
```

`in_grep` defaults to `false`. Grep opens in plain-text mode; `query` does not
change the grep matching mode. If `query` is absent, the existing behavior is
unchanged: a new window starts with an empty search bar. An explicit empty
string clears the initial query. A request with `query` replaces an existing
picker window even when it targets the same directory. The query applies only
to that request, not to later launches from the global keybind.

Keep the connection open to receive the existing `PickResponse` JSON line when
the user selects a result or dismisses the picker.
</details>

<details>
<summary>
<h2>Zed integration</h2>
</summary>

This is the recommended way to use fff-gpui within a project. Add the following to your Zed config files and replace `/path/to/fff-gpui` with the actual path to your binary.

**`~/.config/zed/tasks.json`**
```json
[
  {
    "label": "fff-gpui: Files",
    "command": "/path/to/fff-gpui --open .",
    "env": { "EDITOR": "zed" },
    "use_new_terminal": false,
    "allow_concurrent_runs": false,
    "reveal": "never",
    "reveal_target": "dock",
    "hide": "always",
    "shell": "system",
    "show_summary": false,
    "show_command": false,
    "save": "none"
  },
  {
    "label": "fff-gpui: Grep",
    "command": "/path/to/fff-gpui --open . --grep",
    "env": { "EDITOR": "zed" },
    "use_new_terminal": false,
    "allow_concurrent_runs": false,
    "reveal": "never",
    "reveal_target": "dock",
    "hide": "always",
    "shell": "system",
    "show_summary": false,
    "show_command": false,
    "save": "none"
  }
]
```

**`~/.config/zed/keymap.json`**
```json
{
  "context": "Workspace",
  "bindings": {
    "cmd-k cmd-p": ["task::Spawn", { "task_name": "fff-gpui: Files" }],
    "cmd-k cmd-f": ["task::Spawn", { "task_name": "fff-gpui: Grep" }]
  }
}
```


This opens fff-gpui scoped to the current project root. `cmd-k cmd-p` launches in file-search mode, `cmd-k cmd-f` launches directly in grep mode. Selected files open in Zed; with grep, the editor jumps to the matched line.
</details>

## License

MIT
