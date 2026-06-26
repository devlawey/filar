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
        Ok(entry) => match entry.get_password() {
            Ok(s) => s,
            Err(_) => String::new(),
        },
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
    /// API key entered by the user.
    pub api_key: String,
    /// Session ID to restore, if the user picked a previous session.
    pub session_id: Option<String>,
}

/// SSH connection details from the GUI.
#[derive(Clone, Serialize, Deserialize)]
pub struct SshConnection {
    pub host: String,
    pub port: u16,
    pub user: String,
    pub password: String,
}

// ---------------------------------------------------------------------------
// Pending launch — used when GUI runs as a subprocess
// ---------------------------------------------------------------------------

fn pending_launch_path() -> Option<std::path::PathBuf> {
    let dir = filar_core::session::SessionStore::new()
        .ok()
        .map(|s| s.dir().to_path_buf())?;
    Some(dir.parent()?.join("pending_launch.json"))
}

pub fn save_pending_launch(cfg: &LaunchConfig) {
    if let Some(p) = pending_launch_path() {
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
    /// Whether the user checked "Save password" for this slot.
    /// The actual password is in the OS credential store, not in this file.
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
}

impl Settings {
    fn path() -> Option<std::path::PathBuf> {
        let dir = filar_core::session::SessionStore::new()
            .ok()
            .map(|s| s.dir().to_path_buf())?;
        Some(dir.parent()?.join("settings.json"))
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
            if let Ok(data) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(p, data);
            }
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
            password,
            save_password: p.save_password,
        }
    }

    fn to_profile(&self) -> SshProfile {
        SshProfile {
            host: self.host.clone(),
            port: self.port.clone(),
            user: self.user.clone(),
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
}

impl eframe::App for LauncherApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(ctx, |ui| {
            ui.add_space(4.0);
            ui.heading("Filar");
            ui.label("Terminal with an AI agent over SSH");
            ui.separator();

            // ── Session list ────────────────────────────────────────────
            ui.label("Recent sessions:");
            if self.sessions.is_empty() {
                ui.label("  (no saved sessions yet)");
            } else {
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
                        }
                    });
            }

            ui.separator();

            // ── Target selector ─────────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label("Target:");
                ui.radio_value(&mut self.target_mode, 0, "Local");
                for i in 1..=SSH_SLOTS {
                    ui.radio_value(&mut self.target_mode, i, format!("SSH{i}"));
                }
            });

            // ── SSH fields ──────────────────────────────────────────────
            if self.target_mode > 0 {
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

            ui.separator();

            // ── LLM settings ────────────────────────────────────────────
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

            ui.separator();

            // ── Buttons ─────────────────────────────────────────────────
            ui.horizontal(|ui| {
                let launch = ui.button("Launch").clicked();
                let cancel = ui.button("Cancel").clicked();

                if launch {
                    let target = if self.target_mode == 0 {
                        "local".to_string()
                    } else {
                        "ssh".to_string()
                    };

                    let ssh = if self.target_mode > 0 {
                        let slot = &self.ssh_slots[self.target_mode - 1];
                        Some(SshConnection {
                            host: slot.host.clone(),
                            port: slot.port.parse().unwrap_or(22),
                            user: slot.user.clone(),
                            password: slot.password.clone(),
                        })
                    } else {
                        None
                    };

                    // Save non-sensitive settings to settings.json.
                    let settings = Settings {
                        model: self.model.clone(),
                        api_base_url: self.api_base_url.clone(),
                        ssh_profiles: self
                            .ssh_slots
                            .iter()
                            .map(|s| s.to_profile())
                            .collect(),
                        last_ssh: if self.target_mode > 0 {
                            self.target_mode - 1
                        } else {
                            0
                        },
                    };
                    settings.save();

                    // Save API key to OS credential store (always).
                    save_secret(api_key_cred_name(), &self.api_key);

                    // Save/delete SSH passwords based on checkbox state.
                    for (i, slot) in self.ssh_slots.iter().enumerate() {
                        if slot.save_password && !slot.password.is_empty() {
                            save_secret(&ssh_cred_name(i), &slot.password);
                        } else {
                            delete_secret(&ssh_cred_name(i));
                        }
                    }

                    let session_id =
                        self.selected_session.map(|i| self.sessions[i].id.clone());
                    let cfg = LaunchConfig {
                        target,
                        ssh,
                        model: self.model.clone(),
                        api_base_url: self.api_base_url.clone(),
                        api_key: self.api_key.clone(),
                        session_id,
                    };
                    save_pending_launch(&cfg);
                    std::process::exit(0);
                }

                if cancel {
                    std::process::exit(0);
                }
            });
        });
    }
}

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run_launcher(config: &Config) {
    let sessions = SessionStore::new()
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
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([460.0, 620.0])
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
