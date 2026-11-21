use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

pub const DEFAULT_WINDOW_WIDTH: f32 = 960.0;
pub const DEFAULT_WINDOW_HEIGHT: f32 = 520.0;
pub const DEFAULT_PICKER_PANE_WIDTH: f32 = 430.0;
// launchd delays relaunching a job that ran for less than its 10 s default ThrottleInterval.
const MIN_IDLE_EXIT_SECS: u64 = 10;

fn default_window_width() -> f32 {
    DEFAULT_WINDOW_WIDTH
}

fn default_window_height() -> f32 {
    DEFAULT_WINDOW_HEIGHT
}

fn default_picker_pane_width() -> f32 {
    DEFAULT_PICKER_PANE_WIDTH
}

fn default_sync_zed_settings() -> bool {
    true
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FontConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_family: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ui_size: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub buffer_size: Option<f32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ThemeConfig {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub border: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub selected_row: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hover_row: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_primary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_secondary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_dim: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status_bar_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_highlight: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub match_highlight_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_bg: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor_selection: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_muted: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub icon_accent: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppConfig {
    #[serde(default)]
    pub editor: String,
    #[serde(default = "default_sync_zed_settings", alias = "sync-zed-settings")]
    pub sync_zed_settings: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub global_keybind: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_path: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub exclude_dirs: Vec<PathBuf>,
    #[serde(default = "default_window_width")]
    pub window_width: f32,
    #[serde(default = "default_window_height")]
    pub window_height: f32,
    #[serde(default = "default_picker_pane_width")]
    pub picker_pane_width: f32,
    #[serde(default)]
    pub font: FontConfig,
    #[serde(default)]
    pub theme: ThemeConfig,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idle_exit_secs: Option<u64>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            editor: String::new(),
            sync_zed_settings: true,
            global_keybind: None,
            base_path: None,
            exclude_dirs: Vec::new(),
            window_width: DEFAULT_WINDOW_WIDTH,
            window_height: DEFAULT_WINDOW_HEIGHT,
            picker_pane_width: DEFAULT_PICKER_PANE_WIDTH,
            font: FontConfig::default(),
            theme: ThemeConfig::default(),
            idle_exit_secs: None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub path: PathBuf,
    pub config: AppConfig,
}

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/"))
}

pub fn config_path() -> PathBuf {
    if let Some(xdg) = std::env::var_os("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("fff-gpui/config.toml")
    } else {
        home_dir().join(".config/fff-gpui/config.toml")
    }
}

fn config_parent(path: &Path) -> Option<PathBuf> {
    path.parent().map(PathBuf::from)
}

pub fn load_active_config() -> Result<LoadedConfig> {
    let path = config_path();

    if !path.exists() {
        ensure_config_file(&path)?;
    }

    load_config_from(&path)
        .with_context(|| format!("failed to load config from {}", path.display()))
}

pub fn load_config_from(path: &Path) -> Result<LoadedConfig> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file {}", path.display()))?;
    let mut config = toml::from_str::<AppConfig>(&contents)
        .with_context(|| format!("failed to parse config file {}", path.display()))?;
    if config
        .global_keybind
        .as_deref()
        .is_some_and(|binding| binding.trim().is_empty())
    {
        config.global_keybind = None;
    }
    config
        .exclude_dirs
        .retain(|path| !path.as_os_str().is_empty());
    if !config.window_width.is_finite() || config.window_width <= 0.0 {
        warn!(
            value = config.window_width,
            "invalid window_width in config; falling back to default"
        );
        config.window_width = DEFAULT_WINDOW_WIDTH;
    }
    if !config.window_height.is_finite() || config.window_height <= 0.0 {
        warn!(
            value = config.window_height,
            "invalid window_height in config; falling back to default"
        );
        config.window_height = DEFAULT_WINDOW_HEIGHT;
    }
    if !config.picker_pane_width.is_finite() || config.picker_pane_width <= 0.0 {
        warn!(
            value = config.picker_pane_width,
            "invalid picker_pane_width in config; falling back to default"
        );
        config.picker_pane_width = DEFAULT_PICKER_PANE_WIDTH;
    }
    match config.idle_exit_secs {
        Some(0) => config.idle_exit_secs = None,
        Some(secs) if secs < MIN_IDLE_EXIT_SECS => {
            warn!(
                value = secs,
                "idle_exit_secs below minimum; using {MIN_IDLE_EXIT_SECS}"
            );
            config.idle_exit_secs = Some(MIN_IDLE_EXIT_SECS);
        }
        _ => {}
    }
    Ok(LoadedConfig {
        path: path.to_path_buf(),
        config,
    })
}

const DEFAULT_CONFIG: &str = "\
sync_zed_settings = true
editor = \"\"
global_keybind = \"\"
window_width = 960.0
window_height = 520.0
picker_pane_width = 430.0
";

pub fn ensure_config_file(path: &Path) -> Result<()> {
    if let Some(parent) = config_parent(path) {
        fs::create_dir_all(&parent)
            .with_context(|| format!("failed to create config directory {}", parent.display()))?;
    }

    if path.exists() {
        return Ok(());
    }

    fs::write(path, DEFAULT_CONFIG)
        .with_context(|| format!("failed to write config file {}", path.display()))?;
    info!(path = %path.display(), "wrote default config");
    Ok(())
}
