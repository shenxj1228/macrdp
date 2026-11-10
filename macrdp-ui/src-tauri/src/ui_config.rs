//! Independent UI configuration with persistent storage at ~/.macrdp/config-ui.toml

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// UI-specific configuration. All fields have sensible defaults so
/// `UiConfig::default()` is always usable.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    // ── Server ──────────────────────────────────────────────
    pub port: u16,
    pub frame_rate: u32,
    pub bitrate_mbps: u32,
    pub encoder: String,
    pub chroma_mode: String,
    pub bind_address: String,
    pub max_connections: u32,
    pub idle_timeout_secs: u64,

    // ── Auth ────────────────────────────────────────────────
    pub username: String,
    pub password: String,

    // ── Display ─────────────────────────────────────────────
    #[serde(alias = "hidpi_scale")]
    pub resolution: String,
    pub show_cursor: bool,
    /// Keep the display awake while the server runs (avoids the lock screen blocking input).
    pub keep_display_awake: bool,

    // ── Input ───────────────────────────────────────────────
    /// How Windows modifier keys map to macOS: "shortcut" (Ctrl→⌘, Windows habits) or
    /// "position" (Ctrl→Control, Alt→⌘).
    pub keyboard_modifier_mapping: String,

    // ── Application ─────────────────────────────────────────
    pub log_level: String,
    pub theme: String,
    pub autostart: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            port: 3389,
            frame_rate: 60,
            bitrate_mbps: 50,
            encoder: "auto".to_string(),
            chroma_mode: "avc420".to_string(),
            bind_address: "0.0.0.0".to_string(),
            max_connections: 3,
            idle_timeout_secs: 1800,
            username: "Admin".to_string(),
            password: String::new(),
            resolution: "auto".to_string(),
            show_cursor: false,
            keep_display_awake: false,
            keyboard_modifier_mapping: "shortcut".to_string(),
            log_level: "warn".to_string(),
            theme: "system".to_string(),
            autostart: false,
        }
    }
}

/// Returns the config file path: `~/.macrdp/config-ui.toml`
pub fn config_path() -> PathBuf {
    dirs::home_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join(".macrdp")
        .join("config-ui.toml")
}

impl UiConfig {
    /// Load from `~/.macrdp/config-ui.toml`, creating defaults if the file
    /// does not exist or is unparseable.
    pub fn load() -> Result<Self, String> {
        let path = config_path();
        if !path.exists() {
            let mut cfg = Self::default();
            cfg.ensure_credentials();
            // Try to persist the defaults so the user has a file to edit.
            let _ = cfg.save();
            return Ok(cfg);
        }
        let content =
            std::fs::read_to_string(&path).map_err(|e| format!("read config: {e}"))?;
        let mut config: Self = toml::from_str(&content).map_err(|e| format!("parse config: {e}"))?;
        if config.ensure_credentials() {
            let _ = config.save();
        }
        Ok(config)
    }

    /// Fill in default credentials when they are left empty: the RDP service must never run
    /// without authentication. Returns whether anything changed (so the caller can persist it).
    pub fn ensure_credentials(&mut self) -> bool {
        let mut changed = false;
        if self.username.trim().is_empty() {
            self.username = "Admin".to_string();
            changed = true;
        }
        if self.password.is_empty() {
            self.password = macrdp_core::generate_password(16);
            changed = true;
            tracing::info!(username = %self.username, "No password configured — generated one");
        }
        changed
    }

    /// Serialize to TOML and write to `~/.macrdp/config-ui.toml`.
    pub fn save(&self) -> Result<(), String> {
        let path = config_path();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("create config dir: {e}"))?;
        }
        let content =
            toml::to_string_pretty(self).map_err(|e| format!("serialize config: {e}"))?;
        std::fs::write(&path, content).map_err(|e| format!("write config: {e}"))
    }

    /// Update a single field by key name from a JSON value.
    /// Returns `restart_required`: `false` for hot-updatable fields
    /// (frame_rate, bitrate_mbps, log_level, theme, autostart), `true` for
    /// everything else.
    pub fn set_field(
        &mut self,
        key: &str,
        value: &serde_json::Value,
    ) -> Result<bool, String> {
        match key {
            "port" => {
                self.port = value
                    .as_u64()
                    .ok_or("port must be a number")?
                    as u16;
            }
            "frame_rate" => {
                self.frame_rate = value
                    .as_u64()
                    .ok_or("frame_rate must be a number")?
                    as u32;
            }
            "bitrate_mbps" => {
                self.bitrate_mbps = value
                    .as_u64()
                    .ok_or("bitrate_mbps must be a number")?
                    as u32;
            }
            "encoder" => {
                self.encoder = value
                    .as_str()
                    .ok_or("encoder must be a string")?
                    .to_string();
            }
            "chroma_mode" => {
                self.chroma_mode = value
                    .as_str()
                    .ok_or("chroma_mode must be a string")?
                    .to_string();
            }
            "bind_address" => {
                self.bind_address = value
                    .as_str()
                    .ok_or("bind_address must be a string")?
                    .to_string();
            }
            "max_connections" => {
                self.max_connections = value
                    .as_u64()
                    .ok_or("max_connections must be a number")?
                    as u32;
            }
            "idle_timeout_secs" => {
                self.idle_timeout_secs = value
                    .as_u64()
                    .ok_or("idle_timeout_secs must be a number")?;
            }
            "username" => {
                self.username = value
                    .as_str()
                    .ok_or("username must be a string")?
                    .to_string();
            }
            "password" => {
                self.password = value
                    .as_str()
                    .ok_or("password must be a string")?
                    .to_string();
            }
            "resolution" | "hidpi_scale" => {
                self.resolution = value
                    .as_str()
                    .or_else(|| value.as_u64().map(|_| ""))
                    .ok_or("resolution must be a string")?
                    .to_string();
                if self.resolution.is_empty() {
                    self.resolution = value.as_u64().unwrap().to_string();
                }
            }
            "show_cursor" => {
                self.show_cursor = value
                    .as_bool()
                    .ok_or("show_cursor must be a boolean")?;
            }
            "keep_display_awake" => {
                self.keep_display_awake = value
                    .as_bool()
                    .ok_or("keep_display_awake must be a boolean")?;
            }
            "keyboard_modifier_mapping" => {
                let mapping = value
                    .as_str()
                    .ok_or("keyboard_modifier_mapping must be a string")?;
                if mapping != "shortcut" && mapping != "position" {
                    return Err(
                        "keyboard_modifier_mapping must be \"shortcut\" or \"position\""
                            .to_string(),
                    );
                }
                self.keyboard_modifier_mapping = mapping.to_string();
            }
            "log_level" => {
                self.log_level = value
                    .as_str()
                    .ok_or("log_level must be a string")?
                    .to_string();
            }
            "theme" => {
                self.theme = value
                    .as_str()
                    .ok_or("theme must be a string")?
                    .to_string();
            }
            "autostart" => {
                self.autostart = value
                    .as_bool()
                    .ok_or("autostart must be a boolean")?;
            }
            _ => return Err(format!("unknown config key: {key}")),
        }

        // Hot-updatable fields do NOT require a restart.
        // Only "theme" and "autostart" are pure UI settings.
        // All other server settings are hot-updatable or auto-restarted, except the
        // keyboard mapping: the input handler reads it only when the server starts.
        let restart_required = key == "keyboard_modifier_mapping";
        Ok(restart_required)
    }

    /// Convert to the core library's `ServerConfig` used to start the server.
    pub fn to_server_config(&self) -> macrdp_core::ServerConfig {
        macrdp_core::ServerConfig {
            port: self.port,
            frame_rate: self.frame_rate,
            width: 0,  // auto-detect
            height: 0, // auto-detect
            username: Some(self.username.clone()),
            password: if self.password.is_empty() {
                None
            } else {
                Some(self.password.clone())
            },
            cert_path: None,
            key_path: None,
            idle_timeout_secs: self.idle_timeout_secs,
            log_level: Some(self.log_level.clone()),
            quality: None,
            encoder: Some(self.encoder.clone()),
            chroma_mode: Some(self.chroma_mode.clone()),
            resolution: Some(self.resolution.clone()),
            show_cursor: Some(self.show_cursor),
            keep_display_awake: self.keep_display_awake,
            cursor_mode: None,
            bitrate_mbps: Some(self.bitrate_mbps),
            max_qp: None,
            audio: macrdp_core::AudioConfig::default(),
            clipboard: macrdp_core::ClipboardConfig::default(),
            keyboard: macrdp_core::KeyboardConfig {
                modifier_mapping: match self.keyboard_modifier_mapping.as_str() {
                    "position" => macrdp_core::ModifierMapping::Position,
                    _ => macrdp_core::ModifierMapping::Shortcut,
                },
            },
        }
    }
}
