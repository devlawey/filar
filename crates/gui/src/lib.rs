//! Minimal GUI launcher for Warp.
//!
//! Shows a simple window where the user can:
//! - Pick a previous session (up to 10) to restore.
//! - Select a target: `Local` or `SSH1`–`SSH5` (up to 5 saved SSH profiles).
//! - Enter model, API URL, and API key.
//!
//! **Security:** Sensitive data (API keys, SSH passwords) is stored in the
//! OS credential manager (Windows Credential Manager, macOS Keychain, Linux
//! Secret Service) — NEVER in plain-text files. Non-sensitive data (host,
//! port, user, model, API URL) is saved in `settings.json`.
//!
//! The API key is always saved to the credential store after the first launch.
//! SSH passwords are saved only when the user checks "Save password" for that
//! SSH slot.
//!
//! On "Launch", returns a [`LaunchConfig`] that `main.rs` uses to start the TUI.

use eframe::egui;
use serde::{Deserialize, Serialize};

use filar_core::{Config, SessionMeta, SessionStore};

/// Number of SSH profile slots.
const SSH_SLOTS: usize = 5;

/// Service name used for the OS credential store.
const CRED_SERVICE: &str = "filar";

// ---------------------------------------------------------------------------
// Credential store helpers (OS keyring / Credential Manager)
// ---------------------------------------------------------------------------

/// Save a secret to the OS credential store.
fn save_secret(username: &str, secret: &str) {
    if secret.is_empty() {
        delete_secret(username);
        return;
    }
    match keyring::Entry::new(CRED_SERVICE, username) {
        Ok(entry) => {
            if let Err(e) = entry.set_password(secret) {
                tracing::warn!(error = %e, "failed to save secret to credential store");
            }
        }
        Err(e) => tracing::warn!(error = %e, "failed to create credential entry"),
    }
}

/// Load a secret from the OS credential store. Returns empty string if not found.
fn load_secret(username: &str) -> String {
    match keyring::Entry::new(CRED_SERVICE, username) {
        Ok(entry) => entry.get_password().unwrap_or_default(),
        Err(_) => String::new(),
    }
}

/// Delete a secret from the OS credential store.
fn delete_secret(username: &str) {
    if let Ok(entry) = keyring::Entry::new(CRED_SERVICE, username) {
        let _ = entry.delete_credential();
    }
}

/// Credential key for the API key.
fn api_key_cred_name() -> &'static str {
    "api_key"
}

/// Credential key for SSH slot N (0-based).
fn ssh_cred_name(slot: usize) -> String {
    format!("ssh{slot}")
}

// ---------------------------------------------------------------------------
// LaunchConfig — returned to main.rs
// ---------------------------------------------------------------------------

/// The user's choices from the launcher GUI.
///
/// Secrets (API key, SSH passwords) are NOT serialized — they go through
/// the OS credential store. See `filar_core::secrets::KeyringSecretProvider`.
#[derive(Serialize, Deserialize)]
pub struct LaunchConfig {
    /// `"local"` or `"ssh"`.
    pub target: String,
    /// SSH connection details (when target is "ssh").
    pub ssh: Option<SshConnection>,
    /// Model name (e.g. `"glm-5.1"`).
    pub model: String,
    /// API base URL.
    pub api_base_url: String,
    /// API key entered by the user — NEVER written to disk.
    #[serde(skip, default)]
    pub api_key: String,
    /// Session ID to restore, if the user picked a previous session.
    pub session_id: Option<String>,
    /// Temperature as text (empty = provider default).
    #[serde(default)]
    pub temperature: String,
    /// Extra body JSON as text (empty = none).
    #[serde(default)]
    pub extra_body: String,
}

/// SSH connection details from the GUI.
///
/// The password is NEVER serialized — it goes through the OS credential store.
#[derive(Clone, Serialize, Deserialize)]
pub struct SshConnection {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// Password — NEVER written to disk.
    #[serde(skip, default)]
    pub password: String,
    /// Slot index for saving/loading the password from the OS credential store.
    /// Not a secret — must be persisted so resume picks the correct keyring entry.
    #[serde(default)]
    pub slot: usize,
}

// ---------------------------------------------------------------------------
// Pending launch — used when GUI runs as a subprocess
// ---------------------------------------------------------------------------

fn pending_launch_path() -> Option<std::path::PathBuf> {
    let base = filar_core::session::default_base_dir().ok()?;
    Some(base.join("filar").join("pending_launch.json"))
}

pub fn save_pending_launch(cfg: &LaunchConfig) {
    if let Some(p) = pending_launch_path() {
        if let Some(parent) = p.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(data) = serde_json::to_string(cfg) {
            let _ = std::fs::write(p, data);
        }
    }
}

pub fn load_pending_launch() -> Option<LaunchConfig> {
    let p = pending_launch_path()?;
    if !p.exists() {
        return None;
    }
    let data = std::fs::read_to_string(&p).ok()?;

    // Check for old-format files that still contain plaintext secrets.
    // Users upgrading from 0.6.x may have these on disk; delete them
    // and return None so the GUI re-launches with the new clean format.
    if data.contains("\"api_key\":") || data.contains("\"password\":") {
        let _ = std::fs::remove_file(&p);
        tracing::info!("deleted old-format pending_launch.json with plaintext secrets");
        return None;
    }

    let _ = std::fs::remove_file(&p);
    serde_json::from_str(&data).ok()
}

// ---------------------------------------------------------------------------
// Settings — saved between launches (NO secrets, only non-sensitive data)
// ---------------------------------------------------------------------------

/// A saved SSH profile (host/port/user only — NO password).
#[derive(Serialize, Deserialize, Default, Clone)]
struct SshProfile {
    host: String,
    port: String,
    user: String,
    /// Optional alias shown in the target selector instead of `SSHn`.
    #[serde(default)]
    alias: String,
    /// Whether the user checked "Save password" for this slot.
    #[serde(default)]
    save_password: bool,
}

/// Persistent settings saved between launches.
#[derive(Serialize, Deserialize, Default)]
struct Settings {
    model: String,
    api_base_url: String,
    #[serde(default)]
    ssh_profiles: Vec<SshProfile>,
    #[serde(default)]
    last_ssh: usize,
    #[serde(default)]
    temperature: String,
    #[serde(default)]
    extra_body: String,
}

impl Settings {
    fn path() -> Option<std::path::PathBuf> {
        let base = filar_core::session::default_base_dir().ok()?;
        Some(base.join("filar").join("settings.json"))
    }

    fn load() -> Self {
        let mut settings = match Self::path() {
            Some(p) if p.exists() => {
                let data = std::fs::read_to_string(&p).unwrap_or_default();
                serde_json::from_str(&data).unwrap_or_default()
            }
            _ => Self::default(),
        };
        while settings.ssh_profiles.len() < SSH_SLOTS {
            settings.ssh_profiles.push(SshProfile::default());
        }
        settings.ssh_profiles.truncate(SSH_SLOTS);
        if settings.last_ssh >= SSH_SLOTS {
            settings.last_ssh = 0;
        }
        settings
    }

    fn save(&self) {
        if let Some(p) = Self::path() {
            // Ensure parent directory exists before writing.
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(data) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(p, data);
            }
        }
    }
}

/// Write `[llm]` settings to `config.toml` in `%APPDATA%\filar\`
/// so `filar-tui` invoked without the GUI launcher picks them up.
///
/// If a config.toml already exists, only the `[llm]` section is updated;
/// other sections (SSH targets, profiles, etc.) are preserved.
fn save_config_toml(settings: &Settings) {
    let base = match filar_core::default_base_dir() {
        Ok(b) => b,
        Err(_) => return,
    };
    let app_dir = base.join("filar");
    let path = app_dir.join("config.toml");

    // Only save if the model field is non-empty.
    if settings.model.is_empty() {
        return;
    }

    // Load existing config to preserve non-LLM sections.
    let mut config: filar_core::Config = if path.exists() {
        filar_core::Config::load(&path).unwrap_or_default()
    } else {
        filar_core::Config::default()
    };

    // Update LLM section from GUI settings.
    config.llm.model = settings.model.clone();
    if !settings.api_base_url.is_empty() {
        config.llm.api_base_url = settings.api_base_url.clone();
    }
    if !settings.temperature.is_empty() {
        config.llm.temperature = settings.temperature.parse().ok();
    }
    if !settings.extra_body.is_empty() {
        config.llm.extra_body = serde_json::from_str(&settings.extra_body).ok();
    }

    let _ = std::fs::create_dir_all(&app_dir);
    match toml::to_string_pretty(&config) {
        Ok(data) => {
            if let Err(e) = std::fs::write(&path, &data) {
                tracing::warn!(path = %path.display(), error = %e, "failed to save config.toml");
            }
        }
        Err(e) => {
            tracing::warn!(error = %e, "failed to serialize config.toml");
        }
    }
}

// ---------------------------------------------------------------------------
// LauncherApp
// ---------------------------------------------------------------------------

struct SshSlot {
    host: String,
    port: String,
    user: String,
    alias: String,
    password: String,
    save_password: bool,
}

impl SshSlot {
    fn from_profile(p: &SshProfile, slot_idx: usize) -> Self {
        let password = if p.save_password {
            load_secret(&ssh_cred_name(slot_idx))
        } else {
            String::new()
        };
        Self {
            host: p.host.clone(),
            port: if p.port.is_empty() {
                "22".to_string()
            } else {
                p.port.clone()
            },
            user: p.user.clone(),
            alias: p.alias.clone(),
            password,
            save_password: p.save_password,
        }
    }

    fn to_profile(&self) -> SshProfile {
        SshProfile {
            host: self.host.clone(),
            port: self.port.clone(),
            user: self.user.clone(),
            alias: self.alias.trim().chars().take(32).collect(),
            save_password: self.save_password,
        }
    }
}

struct LauncherApp {
    sessions: Vec<SessionMeta>,
    selected_session: Option<usize>,
    /// 0 = local, 1..=5 = SSH1..SSH5
    target_mode: usize,
    model: String,
    api_base_url: String,
    api_key: String,
    ssh_slots: Vec<SshSlot>,
    temperature: String,
    extra_body: String,
    validation_error: String,
}

/// Apply the dark theme, matching the TUI accent palette:
/// muted dark background, one accent colour (used for buttons and highlights),
/// readable grey scale for secondary text.
fn configure_theme(ctx: &egui::Context) {
    let mut visuals = egui::Visuals::dark();
    // Accent — a teal/cyan that matches the TUI's mode colours.
    let accent = egui::Color32::from_rgb(0x3d, 0xb3, 0xb3);
    visuals.override_text_color = Some(egui::Color32::from_gray(0xe0));
    visuals.widgets.noninteractive.bg_fill = egui::Color32::from_gray(0x22);
    visuals.widgets.inactive.bg_fill = egui::Color32::from_gray(0x2a);
    visuals.widgets.hovered.bg_fill = egui::Color32::from_gray(0x38);
    visuals.widgets.active.bg_fill = accent;
    visuals.widgets.noninteractive.fg_stroke.color = egui::Color32::from_gray(0xc0);
    visuals.selection.bg_fill = accent.linear_multiply(0.3);
    ctx.set_visuals(visuals);
}

impl LauncherApp {
    fn render_session_list(&mut self, ui: &mut egui::Ui) {
        ui.label("Recent sessions:");
        if self.sessions.is_empty() {
            ui.label("  (no saved sessions yet)");
            return;
        }
        let new_selected = self.selected_session.is_none();
        if ui
            .selectable_label(new_selected, "  + Start new session")
            .clicked()
        {
            self.selected_session = None;
        }
        egui::ScrollArea::vertical()
            .max_height(100.0)
            .show(ui, |ui| {
                for (i, session) in self.sessions.iter().enumerate() {
                    let selected = self.selected_session == Some(i);
                    let text = format!(
                        "  {} | {} | {}",
                        session.timestamp, session.target, session.preview
                    );
                    if ui.selectable_label(selected, &text).clicked() {
                        self.selected_session = Some(i);
                    }
            }});
    }

    fn render_target_selector(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Target:");
            ui.radio_value(&mut self.target_mode, 0, "Local");
            for i in 1..=SSH_SLOTS {
                let alias = self.ssh_slots[i - 1].alias.trim();
                let label = if alias.is_empty() {
                    format!("SSH{i}")
                } else if alias.chars().count() > 32 {
                    format!("{}…", alias.chars().take(31).collect::<String>())
                } else {
                    alias.to_string()
                };
                ui.radio_value(&mut self.target_mode, i, label);
            }
        });
    }

    fn render_ssh_fields(&mut self, ui: &mut egui::Ui) {
        if self.target_mode == 0 {
            return;
        }
        let idx = self.target_mode - 1;
        let slot = &mut self.ssh_slots[idx];
        egui::Grid::new("ssh_grid")
            .num_columns(2)
            .spacing([10.0, 6.0])
            .show(ui, |ui| {
                ui.label("Host:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.host)
                        .hint_text("192.168.1.100"),
                );
                ui.end_row();
                ui.label("Port:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.port)
                        .hint_text("22"),
                );
                ui.end_row();
                ui.label("User:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.user)
                        .hint_text("root"),
                );
                ui.end_row();
                ui.label("Alias:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.alias)
                        .hint_text("deploy")
                        .desired_width(120.0),
                );
                ui.end_row();
                ui.label("Password:");
                ui.add(
                    egui::TextEdit::singleline(&mut slot.password)
                        .password(true)
                        .hint_text(""),
                );
                ui.end_row();
            });
        ui.checkbox(&mut slot.save_password, "Save password (encrypted in OS credential store)");
    }

    fn render_llm_settings(&mut self, ui: &mut egui::Ui) {
        ui.heading("LLM");
        ui.label("Model:");
        ui.add(
            egui::TextEdit::singleline(&mut self.model)
                .hint_text("e.g. glm-5.1"),
        );
        ui.label("API base URL:");
        ui.add(
            egui::TextEdit::singleline(&mut self.api_base_url)
                .hint_text("e.g. https://openrouter.ai/api/v1"),
        );
        ui.label("API key:");
        ui.add(
            egui::TextEdit::singleline(&mut self.api_key)
                .password(true)
                .hint_text("saved in OS credential store"),
        );
        ui.label("Temperature:");
        ui.add(
            egui::TextEdit::singleline(&mut self.temperature)
                .hint_text("empty = default (e.g. 0.3)"),
        );
        ui.label("Extra body (JSON):");
        ui.add(
            egui::TextEdit::multiline(&mut self.extra_body)
                .hint_text("e.g. {\"thinking\": {\"type\": \"disabled\"}}")
                .desired_rows(2)
                .desired_width(f32::INFINITY),
        );
    }

    fn do_launch(&mut self) {
        self.validation_error.clear();
        if !self.temperature.trim().is_empty()
            && !matches!(
                self.temperature.trim().parse::<f32>(),
                Ok(t) if t.is_finite() && (0.0..=2.0).contains(&t)
            )
        {
            self.validation_error = format!(
                "Invalid temperature: '{}'. Expected a number in [0.0, 2.0].",
                self.temperature
            );
        }
        if self.validation_error.is_empty()
            && !self.extra_body.trim().is_empty()
            && serde_json::from_str::<serde_json::Value>(&self.extra_body).is_err()
        {
            self.validation_error = "Invalid extra body JSON.".to_string();
        }
        if !self.validation_error.is_empty() {
            return;
        }
        let target = if self.target_mode == 0 {
            "local"
        } else {
            "ssh"
        };
        let ssh = if self.target_mode > 0 {
            let slot = &self.ssh_slots[self.target_mode - 1];
            Some(SshConnection {
                host: slot.host.clone(),
                port: slot.port.parse().unwrap_or(22),
                user: slot.user.clone(),
                password: slot.password.clone(),
                slot: self.target_mode.saturating_sub(1),
            })
        } else {
            None
        };
        let settings = Settings {
            model: self.model.clone(),
            api_base_url: self.api_base_url.clone(),
            ssh_profiles: self.ssh_slots.iter().map(|s| s.to_profile()).collect(),
            last_ssh: if self.target_mode > 0 {
                self.target_mode - 1
            } else {
                0
            },
            temperature: self.temperature.clone(),
            extra_body: self.extra_body.clone(),
        };
        settings.save();
        // Also persist LLM settings to config.toml in the app-data directory
        // so `filar-tui` (invoked without the GUI launcher) picks them up.
        save_config_toml(&settings);
        save_secret(api_key_cred_name(), &self.api_key);
        for (i, slot) in self.ssh_slots.iter().enumerate() {
            if slot.save_password && !slot.password.is_empty() {
                save_secret(&ssh_cred_name(i), &slot.password);
            } else {
                delete_secret(&ssh_cred_name(i));
            }
        }
        let session_id = self.selected_session.map(|i| self.sessions[i].id.clone());
        let cfg = LaunchConfig {
            target: target.to_string(),
            ssh,
            model: self.model.clone(),
            api_base_url: self.api_base_url.clone(),
            api_key: self.api_key.clone(),
            session_id,
            temperature: self.temperature.clone(),
            extra_body: self.extra_body.clone(),
        };
        save_pending_launch(&cfg);
        std::process::exit(0);
    }
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // Dark theme matching the TUI accent palette.
        configure_theme(ctx);

        // Fixed bottom panel: Launch/Cancel always visible, regardless of
        // window height. The rest of the content scrolls above it.
        egui::TopBottomPanel::bottom("bottom_buttons")
            .resizable(false)
            .show(ctx, |ui| {
                ui.horizontal(|ui| {
                    if ui.button("Launch").clicked() {
                        self.do_launch();
                    }
                if ui.button("Cancel").clicked() {
                    std::process::exit(0);
                }
            });
            if !self.validation_error.is_empty() {
                ui.colored_label(egui::Color32::RED, &self.validation_error);
            }
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                ui.add_space(4.0);
                ui.heading("Filar");
                ui.label("Terminal with an AI agent over SSH");
                ui.separator();

                self.render_session_list(ui);
                ui.separator();
                self.render_target_selector(ui);
                ui.separator();
                self.render_ssh_fields(ui);
                ui.separator();
                self.render_llm_settings(ui);
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run_launcher(config: &Config) {
    let sessions = SessionStore::with_default_dir()
        .ok()
        .and_then(|s| s.list().ok())
        .unwrap_or_default();

    let settings = Settings::load();

    tracing::info!(sessions = sessions.len(), "GUI launcher starting");

    let ssh_slots: Vec<SshSlot> = settings
        .ssh_profiles
        .iter()
        .enumerate()
        .map(|(i, p)| SshSlot::from_profile(p, i))
        .collect();

    // Load API key from credential store.
    let api_key = load_secret(api_key_cred_name());

    let app = LauncherApp {
        sessions,
        selected_session: None,
        target_mode: if settings.last_ssh > 0 && settings.last_ssh < SSH_SLOTS {
            settings.last_ssh + 1
        } else {
            0
        },
        model: if settings.model.is_empty() {
            config.llm.model.clone()
        } else {
            settings.model
        },
        api_base_url: if settings.api_base_url.is_empty() {
            config.llm.api_base_url.clone()
        } else {
            settings.api_base_url
        },
        api_key,
        ssh_slots,
        temperature: settings.temperature,
        extra_body: settings.extra_body,
        validation_error: String::new(),
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 620.0])
            .with_min_inner_size([440.0, 300.0])
            .with_title("Filar — Launcher")
            .with_icon(std::sync::Arc::new(load_icon())),
        ..Default::default()
    };

    let _ = eframe::run_native(
        "Filar",
        options,
        Box::new(|_cc| Ok(Box::new(app))),
    );
}

// ---------------------------------------------------------------------------
// Icon loading — embed PNG from pics/ folder at compile time
// ---------------------------------------------------------------------------

/// Load the window icon from the PNG file in `pics/filar_256.png`.
///
/// The PNG is embedded into the binary at compile time via `include_bytes!`,
/// then decoded to RGBA at runtime using the `image` crate.
fn load_icon() -> egui::IconData {
    let png_data = include_bytes!("../../../pics/filar_256.png");
    let img = image::load_from_memory(png_data)
        .expect("Failed to decode filar_256.png")
        .to_rgba8();
    let (width, height) = img.dimensions();
    egui::IconData {
        rgba: img.into_raw(),
        width,
        height,
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_config_serialization_excludes_secrets() {
        let cfg = LaunchConfig {
            target: "ssh".into(),
            ssh: Some(SshConnection {
                host: "10.0.0.5".into(),
                port: 22,
                user: "root".into(),
                password: "supersecret".into(),
                slot: 0,
            }),
            model: "glm".into(),
            api_base_url: "https://api.example.com".into(),
            api_key: "sk-test-key-12345".into(),
            session_id: None,
            temperature: String::new(),
            extra_body: String::new(),
        };
        let json = serde_json::to_string_pretty(&cfg).unwrap();
        assert!(!json.contains("supersecret"));
        assert!(!json.contains("sk-test-key-12345"));
        assert!(json.contains("10.0.0.5"));
        assert!(json.contains("glm"));
        // Round-trip: non-secret fields survive serialize→deserialize.
        let loaded: LaunchConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.target, "ssh");
        assert_eq!(loaded.model, "glm");
        assert!(loaded.ssh.is_some());
        assert_eq!(loaded.ssh.as_ref().unwrap().slot, 0);
        // Secrets must be absent after deserialization (serde(skip) → default).
        assert!(loaded.api_key.is_empty());
        assert!(loaded.ssh.as_ref().unwrap().password.is_empty());
    }

    #[test]
    fn ssh_connection_serialization_excludes_password() {
        let conn = SshConnection {
            host: "host".into(),
            port: 22,
            user: "admin".into(),
            password: "p@ssw0rd".into(),
            slot: 2,
        };
        let json = serde_json::to_string(&conn).unwrap();
        assert!(!json.contains("p@ssw0rd"));
        assert!(json.contains("host"));
        // Round-trip: non-secret fields survive, secret doesn't.
        let loaded: SshConnection = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.host, "host");
        assert_eq!(loaded.slot, 2);
        assert!(loaded.password.is_empty());
    }

    #[test]
    fn settings_save_pattern_preserves_data() {
        // Test the internal pattern: create_dir_all before write,
        // then write→read round-trip verifying data integrity.
        let dir = std::env::temp_dir().join(format!("filar_test_save_{}", std::process::id()));
        let file = dir.join("filar").join("settings.json");
        let _ = std::fs::remove_dir_all(&dir);

        assert!(!file.parent().unwrap().exists());
        // Replicate what Settings::save does: create_dir_all + write.
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        let s = Settings {
            model: "test-model".into(), api_base_url: "https://example.com".into(),
            ssh_profiles: vec![], last_ssh: 0, temperature: "0.5".into(),
            extra_body: String::new(),
        };
        std::fs::write(&file, serde_json::to_string_pretty(&s).unwrap()).unwrap();

        let loaded: Settings = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(loaded.model, "test-model");
        assert_eq!(loaded.temperature, "0.5");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
