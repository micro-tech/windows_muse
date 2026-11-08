//! Config and settings for the Muse Windows app.
//!
//! Two files live in the app's config directory (see [`app_config_dir`]):
//! - `config.toml` — hub credentials (URL, token, allowed senders) and the
//!   app's agent name (`agent_name`, default `"john"`) for the `to`
//!   addressing check. John fills this in on-machine; the repo ships only
//!   `config.example.toml` with a placeholder. Never commit a real token.
//! - `settings.json` — UI preferences (currently just the WebView zoom level).
//!   The app reads and writes this itself; no secrets in it.

use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;

/// Hub connection details, read from `config.toml`.
#[derive(Debug, Clone, Deserialize)]
pub struct HubConfig {
    /// Base URL of the chat hub, e.g. `https://chat.micro-tech.xyz`.
    pub hub_url: String,
    /// Bearer token for the hub (a hub token minted for this app).
    /// A placeholder or empty value means "not configured".
    pub hub_token: String,
    /// Senders whose `!` commands the watcher will run. Defaults to
    /// `cobble-jr` and `john` when the key is absent.
    #[serde(default = "default_allowed_senders")]
    pub allowed_senders: Vec<String>,
    /// This app's hub agent name, used for the `to` addressing check
    /// (chat-command spec §4 step 4): a structured `!` message addressed
    /// to a different agent is ignored entirely. Defaults to `john`
    /// when the key is absent (the app uses the john hub token).
    #[serde(default = "default_agent_name")]
    pub agent_name: String,
}

fn default_allowed_senders() -> Vec<String> {
    vec!["cobble-jr".to_string(), "john".to_string()]
}

fn default_agent_name() -> String {
    "john".to_string()
}

/// UI preferences, read from / written to `settings.json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    /// WebView zoom factor for the muse.ai view (1.0 = 100%).
    #[serde(default = "default_zoom")]
    pub zoom: f64,
}

fn default_zoom() -> f64 {
    1.0
}

impl Default for Settings {
    fn default() -> Self {
        Self { zoom: 1.0 }
    }
}

/// Where the app keeps `config.toml` and `settings.json`.
///
/// Windows: `%APPDATA%\muse-windows-app` (the user's own profile — not
/// world-readable by default). Other platforms: `~/.config/muse-windows-app`.
/// This is a small hand-rolled helper instead of the `dirs` crate, because
/// the app is Windows-targeted and the mapping above is all it needs.
pub fn app_config_dir() -> Option<PathBuf> {
    #[cfg(target_os = "windows")]
    {
        std::env::var_os("APPDATA").map(|appdata| PathBuf::from(appdata).join("muse-windows-app"))
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(".config").join("muse-windows-app"))
    }
}

/// Read the hub config. Returns `None` when the file is missing, unreadable,
/// or holds a placeholder/empty token — in all of those cases the hub watcher
/// stays disabled and the UI says so, rather than the app failing to start.
pub fn load_hub_config() -> Option<HubConfig> {
    let path = app_config_dir()?.join("config.toml");
    let text = fs::read_to_string(path).ok()?;
    let config: HubConfig = toml::from_str(&text).ok()?;
    let token = config.hub_token.trim();
    if token.is_empty() || token == "PASTE_YOUR_HUB_TOKEN_HERE" {
        return None;
    }
    if config.hub_url.trim().is_empty() {
        return None;
    }
    Some(config)
}

/// Load UI settings, defaulting to zoom 1.0 when the file is missing or bad.
pub fn load_settings() -> Settings {
    let Some(path) = app_config_dir().map(|d| d.join("settings.json")) else {
        return Settings::default();
    };
    let Ok(text) = fs::read_to_string(path) else {
        return Settings::default();
    };
    serde_json::from_str(&text).unwrap_or_default()
}

/// Persist UI settings. Failures are swallowed (a lost zoom preference is not
/// worth crashing the app over); the caller does not need to handle the error.
pub fn save_settings(settings: &Settings) {
    let Some(dir) = app_config_dir() else {
        return;
    };
    if fs::create_dir_all(&dir).is_err() {
        return;
    }
    if let Ok(text) = serde_json::to_string_pretty(settings) {
        let _ = fs::write(dir.join("settings.json"), text);
    }
}

/// Clamp a zoom factor to the range the UI offers (50%–300%).
pub fn clamp_zoom(zoom: f64) -> f64 {
    zoom.clamp(0.5, 3.0)
}
