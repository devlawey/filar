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

/// Try to load the API key for a profile from the OS credential store.
fn load_secret_for_profile(profile: &filar_core::LlmProfile) -> String {
    load_secret(&profile.key_env)
}

/// Generate a unique profile name with the given prefix.
/// Finds the first free number (profile-1, profile-2, ...) not already in use.
fn unique_profile_name(existing: &[LlmProfileData], prefix: &str) -> String {
    let mut n = 1;
    loop {
        let candidate = format!("{prefix}-{n}");
        if !existing.iter().any(|p| p.name == candidate) {
            return candidate;
        }
        n += 1;
    }
}

/// Credential key for SSH slot N. Uses the same naming convention as
/// the TUI runner: `ssh_target:{name}` where name is the alias (or
/// `SSH{slot+1}` if no alias is set).
///
/// **Contract:** the `name` produced here MUST match the `target.name`
/// field in `build_ssh_targets_from_profiles`, because the runner looks
/// up the password under `format!("ssh_target:{}", target.name)`.
/// Both functions use `SSH{slot+1}` as the fallback for empty alias.
fn ssh_cred_name(slot: usize, alias: &str) -> String {
    let name = if alias.is_empty() {
        format!("SSH{}", slot + 1)
    } else {
        alias.to_string()
    };
    format!("ssh_target:{name}")
}

/// Migration: fix duplicate profile names and key_env entries in a loaded list.
fn deduplicate_profiles(profiles: &mut Vec<LlmProfileData>) {
    let mut seen_names = std::collections::BTreeSet::new();
    for p in profiles.iter_mut() {
        let mut name = p.name.clone();
        while seen_names.contains(&name) {
            name = format!("{name}_dup");
        }
        if name != p.name {
            tracing::warn!(old = %p.name, new = %name, "deduplicated colliding profile name on load");
            p.name = name;
        }
        seen_names.insert(p.name.clone());
    }
    // Fix key_env collisions: ensure each profile has a unique key_env.
    let mut seen_envs = std::collections::BTreeSet::new();
    for p in profiles.iter_mut() {
        let mut env = p.key_env.clone();
        while seen_envs.contains(&env) {
            env = format!("api_key_{env}");
        }
        if env != p.key_env {
            tracing::warn!(old = %p.key_env, new = %env, profile = %p.name, "deduplicated colliding key_env on load");
            p.key_env = env;
        }
        seen_envs.insert(p.key_env.clone());
    }
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
    /// Selected profile name (if using the Models tab).
    #[serde(default)]
    pub selected_profile: Option<String>,
    /// Env var / credential name for the API key.
    #[serde(default = "default_glm_key_env_gui")]
    pub key_env: String,
    /// Directory for Ctrl+S session exports (`None` = CWD).
    #[serde(default)]
    pub save_dir: Option<std::path::PathBuf>,
    /// Full LLM profile list (for Ctrl+L cycling in the TUI).
    #[serde(default)]
    pub profiles: Vec<filar_core::LlmProfile>,
    /// Full SSH target list (for Ctrl+O cycling in the TUI).
    #[serde(default)]
    pub ssh_targets: Vec<filar_core::SshTarget>,
}

fn default_glm_key_env_gui() -> String {
    "GLM_API_KEY".to_string()
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
    /// Named LLM profiles (for Models tab).
    #[serde(default)]
    profiles: Vec<filar_core::LlmProfile>,
    /// Index of the default (last-selected) profile.
    #[serde(default)]
    selected_profile: usize,
    /// Directory for Ctrl+S session exports (`None` = CWD).
    #[serde(default)]
    save_dir: Option<std::path::PathBuf>,
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

// ---------------------------------------------------------------------------
// SSH target merge
// ---------------------------------------------------------------------------

/// Merge launcher SSH profiles into the existing `[[ssh_targets]]` list.
///
/// Launcher-created targets are identified by name `"SSH{n}"` (1..5) or by
/// their `alias`. Non-empty profiles become `SshTarget` entries; empty slots
/// are skipped. Manually-added targets (not matching `"SSH{n}"`) are preserved.
///
/// This is a free function so it can be unit-tested independently of
/// `save_config_toml`.
/// Build `[[ssh_targets]]` from launcher SSH profiles — full rewrite.
///
/// Replaces ALL previous targets; no merge, no stale entries survive.
/// Each non-empty profile produces one `SshTarget`; empty slots are skipped.
fn build_ssh_targets_from_profiles(profiles: &[SshProfile]) -> Vec<filar_core::SshTarget> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut targets: Vec<filar_core::SshTarget> = Vec::new();
    for (i, profile) in profiles.iter().enumerate() {
        if profile.host.is_empty() { continue; }
        let name = if profile.alias.is_empty() {
            format!("SSH{}", i + 1)
        } else {
            profile.alias.clone()
        };
        if seen.contains(&name) { continue; }
        seen.insert(name.clone());
        let port: u16 = profile.port.parse().unwrap_or_else(|_| {
            tracing::warn!(port = %profile.port, slot = i + 1, "invalid SSH port — using 22");
            22
        });
        let port = if port == 0 { 22 } else { port };
        targets.push(filar_core::SshTarget {
            name,
            host: profile.host.clone(),
            port,
            user: profile.user.clone(),
            auth: match profile.save_password {
                true => filar_core::SshAuth::Password { password: None },
                false => filar_core::SshAuth::Key { path: None },
            },
            host_key_policy: filar_core::HostKeyPolicy::Tofu,
        });
    }
    targets
}

/// Write `[llm]` and `[[ssh_targets]]` settings to `config.toml` in
/// `%APPDATA%\filar\` so `filar-tui` invoked without the GUI launcher picks
/// them up.
///
/// If a `config.toml` already exists, the `[llm]` and `[[ssh_targets]]`
/// sections are merged; unrelated sections are preserved.
fn save_config_toml(settings: &Settings) {
    let base = match filar_core::default_base_dir() {
        Ok(b) => b,
        Err(_) => return,
    };
    let app_dir = base.join("filar");
    let path = app_dir.join("config.toml");

    // Load existing config to preserve non-LLM sections.
    let mut config: filar_core::Config = if path.exists() {
        match filar_core::Config::load(&path) {
            Ok(cfg) => cfg,
            Err(e) => {
                tracing::warn!(path = %path.display(), error = %e, "config.toml exists but is invalid, not overwriting");
                return;
            }
        }
    } else {
        filar_core::Config::default()
    };

    // Update primary LLM section (backward compat) only. Launch-specific
    // data (profiles, ssh_targets, save_dir) is passed to the TUI via
    // `pending_launch.json` (#255) — no longer duplicated into config.toml.
    if !settings.model.is_empty() {
        config.llm.model = settings.model.clone();
    }
    if !settings.api_base_url.is_empty() {
        config.llm.api_base_url = settings.api_base_url.clone();
    }
    if !settings.temperature.is_empty() {
        config.llm.temperature = settings.temperature.parse().ok();
    }
    if !settings.extra_body.is_empty() {
        config.llm.extra_body = serde_json::from_str(&settings.extra_body).ok();
    }
    // NOTE: launch-specific sections (llm_profiles, ssh_targets, save_dir) are
    // intentionally left untouched. The GUI no longer writes them (#255) — it
    // passes them via `pending_launch.json` — but must NOT clear them either,
    // because direct-TUI launches still read them from config.toml as fallback.

    if let Err(e) = std::fs::create_dir_all(&app_dir) {
        tracing::warn!(path = %app_dir.display(), error = %e, "failed to create config directory");
        return;
    }
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
            load_secret(&ssh_cred_name(slot_idx, &p.alias))
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
    target_mode: usize,
    ssh_slots: Vec<SshSlot>,
    /// All configured profiles.
    profiles: Vec<LlmProfileData>,
    /// Currently selected profile index.
    selected_profile: usize,
    validation_error: String,
    /// Directory for Ctrl+S session exports (`None` = CWD).
    save_dir: Option<std::path::PathBuf>,
}

/// Local copy of an LLM profile for GUI editing.
#[derive(Clone)]
struct LlmProfileData {
    name: String,
    model: String,
    api_base_url: String,
    key_env: String,
    api_key: String,
    temperature: String,
    extra_body: String,
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
        ui.heading("Models");
        let profile_names: Vec<String> = self.profiles.iter().map(|p| p.name.clone()).collect();
        if profile_names.is_empty() {
            ui.label("No profiles defined. Click Add to create one.");
            if ui.button("Add Profile").clicked() {
                self.profiles.push(LlmProfileData {
                    name: "default".into(), model: String::new(),
                    api_base_url: String::new(), key_env: "GLM_API_KEY".into(),
                    api_key: String::new(), temperature: String::new(),
                    extra_body: String::new(),
                });
                self.selected_profile = 0;
                self.save_profiles();
            }
            return;
        }
        ui.horizontal(|ui| {
            egui::ComboBox::from_label("Profile")
                .selected_text(&profile_names[self.selected_profile])
                .show_ui(ui, |ui| {
                    for (i, name) in profile_names.iter().enumerate() {
                        if ui.selectable_label(i == self.selected_profile, name).clicked() {
                            self.selected_profile = i;
                        }
                    }
                });
            if ui.button("+").on_hover_text("Add profile").clicked() {
                let name = unique_profile_name(&self.profiles, "profile");
                let key_env = format!("api_key_{name}");
                self.profiles.push(LlmProfileData {
                    name, key_env,
                    model: String::new(), api_base_url: String::new(),
                    api_key: String::new(), temperature: String::new(),
                    extra_body: String::new(),
                });
                self.selected_profile = self.profiles.len() - 1;
                self.save_profiles();
            }
            if self.profiles.len() > 1 && ui.button("X").on_hover_text("Delete profile").clicked() {
                let removed = &self.profiles[self.selected_profile];
                delete_secret(&removed.key_env);
                self.profiles.remove(self.selected_profile);
                self.selected_profile = self.selected_profile.min(self.profiles.len().saturating_sub(1));
                self.save_profiles();
            }
        });
        let p = &mut self.profiles[self.selected_profile];
        ui.horizontal(|ui| { ui.label("Name:"); ui.text_edit_singleline(&mut p.name); });
        ui.horizontal(|ui| { ui.label("Model:"); ui.text_edit_singleline(&mut p.model); });
        ui.horizontal(|ui| { ui.label("API URL:"); ui.text_edit_singleline(&mut p.api_base_url); });
        ui.horizontal(|ui| {
            ui.label("API key:");
            ui.add(egui::TextEdit::singleline(&mut p.api_key).password(true).hint_text("saved in OS credential store"));
        });
        ui.horizontal(|ui| { ui.label("Key env:"); ui.text_edit_singleline(&mut p.key_env); });
        ui.horizontal(|ui| { ui.label("Temp:"); ui.text_edit_singleline(&mut p.temperature); });
        ui.label("Extra body (JSON):");
        ui.add(egui::TextEdit::multiline(&mut p.extra_body)
            .hint_text("e.g. {\"thinking\":{\"type\":\"disabled\"}}")
            .desired_rows(2).desired_width(f32::INFINITY));
    }

    /// Folder picker for the Ctrl+S session export directory.
    fn render_save_dir_field(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label("Save directory:");
            let mut display = match &self.save_dir {
                Some(p) => p.display().to_string(),
                None => "CWD (where filar was launched)".to_string(),
            };
            ui.add(
                egui::TextEdit::singleline(&mut display)
                    .desired_width(240.0)
                    .interactive(false),
            );
            if ui.button("Browse…").clicked() {
                if let Some(dir) = rfd::FileDialog::new().pick_folder() {
                    self.save_dir = Some(dir);
                }
            }
            if self.save_dir.is_some() && ui.button("Reset").clicked() {
                self.save_dir = None;
            }
        });
    }

    fn save_profiles(&mut self) {
        let p = self.profiles.get(self.selected_profile);
        Settings {
            model: p.map_or_else(String::new, |x| x.model.clone()),
            api_base_url: p.map_or_else(String::new, |x| x.api_base_url.clone()),
            ssh_profiles: self.ssh_slots.iter().map(|s| s.to_profile()).collect(),
            last_ssh: if self.target_mode > 0 { self.target_mode - 1 } else { 0 },
            temperature: p.map_or_else(String::new, |x| x.temperature.clone()),
            extra_body: p.map_or_else(String::new, |x| x.extra_body.clone()),
            profiles: self.profiles.iter().map(|d| filar_core::LlmProfile {
                name: d.name.clone(), model: d.model.clone(), api_base_url: d.api_base_url.clone(),
                key_env: d.key_env.clone(), max_tokens: 4096,
                temperature: d.temperature.trim().parse().ok(),
                top_p: None, extra_body: serde_json::from_str(&d.extra_body).ok(),
            }).collect(),
            selected_profile: self.selected_profile,
            save_dir: self.save_dir.clone(),
        }.save();
    }

    fn do_launch(&mut self) {
        self.validation_error.clear();
        let Some(p) = self.profiles.get(self.selected_profile) else {
            self.validation_error = "No profile selected".to_string();
            return;
        };
        // Validate unique names and non-empty name.
        if p.name.trim().is_empty() {
            self.validation_error = "Profile name must not be empty.".to_string();
            return;
        }
        for (i, other) in self.profiles.iter().enumerate() {
            if i != self.selected_profile && other.name == p.name {
                self.validation_error = format!("Duplicate profile name: \"{}\". Names must be unique.", p.name);
                return;
            }
        }
        if !p.temperature.trim().is_empty()
            && !matches!(p.temperature.trim().parse::<f32>(), Ok(t) if t.is_finite() && (0.0..=2.0).contains(&t))
        {
            self.validation_error = format!("Invalid temperature: '{}'. Expected [0.0, 2.0].", p.temperature);
        }
        if self.validation_error.is_empty()
            && !p.extra_body.trim().is_empty()
            && serde_json::from_str::<serde_json::Value>(&p.extra_body).is_err()
        {
            self.validation_error = "Invalid extra body JSON.".to_string();
        }
        if !self.validation_error.is_empty() {
            return;
        }
        let target = if self.target_mode == 0 { "local" } else { "ssh" };
        let ssh = if self.target_mode > 0 {
            let slot = &self.ssh_slots[self.target_mode - 1];
            Some(SshConnection { host: slot.host.clone(), port: slot.port.parse().unwrap_or(22), user: slot.user.clone(), password: slot.password.clone(), slot: self.target_mode.saturating_sub(1) })
        } else { None };

        let settings = Settings {
            model: p.model.clone(), api_base_url: p.api_base_url.clone(),
            ssh_profiles: self.ssh_slots.iter().map(|s| s.to_profile()).collect(),
            last_ssh: if self.target_mode > 0 { self.target_mode - 1 } else { 0 },
            temperature: p.temperature.clone(), extra_body: p.extra_body.clone(),
            profiles: self.profiles.iter().map(|d| filar_core::LlmProfile {
                name: d.name.clone(), model: d.model.clone(), api_base_url: d.api_base_url.clone(),
                key_env: d.key_env.clone(), max_tokens: 4096,
                temperature: d.temperature.trim().parse().ok(),
                top_p: None, extra_body: serde_json::from_str(&d.extra_body).ok(),
            }).collect(),
            selected_profile: self.selected_profile,
            save_dir: self.save_dir.clone(),
        };
        settings.save();
        save_config_toml(&settings);
        for prof in &self.profiles {
            if !prof.api_key.is_empty() { save_secret(&prof.key_env, &prof.api_key); }
        }
        for (i, slot) in self.ssh_slots.iter().enumerate() {
            if slot.save_password && !slot.password.is_empty() { save_secret(&ssh_cred_name(i, &slot.alias), &slot.password); }
            else { delete_secret(&ssh_cred_name(i, &slot.alias)); }
        }
        let session_id = self.selected_session.map(|i| self.sessions[i].id.clone());
        let ssh_targets = build_ssh_targets_from_profiles(
            &self.ssh_slots.iter().map(|s| s.to_profile()).collect::<Vec<_>>(),
        );
        let cfg = LaunchConfig {
            target: target.to_string(), ssh,
            model: p.model.clone(), api_base_url: p.api_base_url.clone(),
            api_key: p.api_key.clone(), session_id,
            temperature: p.temperature.clone(), extra_body: p.extra_body.clone(),
            selected_profile: Some(p.name.clone()), key_env: p.key_env.clone(),
            save_dir: self.save_dir.clone(),
            profiles: self.profiles.iter().map(|d| filar_core::LlmProfile {
                name: d.name.clone(), model: d.model.clone(), api_base_url: d.api_base_url.clone(),
                key_env: d.key_env.clone(), max_tokens: 4096,
                temperature: d.temperature.trim().parse().ok(),
                top_p: None, extra_body: serde_json::from_str(&d.extra_body).ok(),
            }).collect(),
            ssh_targets,
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
                ui.separator();
                self.render_save_dir_field(ui);
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

    // Build profile list. Migrate old flat settings into a default profile
    // on first upgrade, preserving existing api_key.
    let mut profiles: Vec<LlmProfileData> = settings
        .profiles
        .iter()
        .map(|p| LlmProfileData {
            name: p.name.clone(),
            model: p.model.clone(),
            api_base_url: p.api_base_url.clone(),
            key_env: p.key_env.clone(),
            api_key: load_secret_for_profile(p),
            temperature: p.temperature.map(|t| t.to_string()).unwrap_or_default(),
            extra_body: p.extra_body
                .as_ref()
                .map(|v| v.to_string())
                .unwrap_or_default(),
        })
        .collect();

    // If no profiles exist, create a default from flat fields / config.
    if profiles.is_empty() {
        let api_key = load_secret(api_key_cred_name());
        profiles.push(LlmProfileData {
            name: "default".into(),
            model: if settings.model.is_empty() { config.llm.model.clone() } else { settings.model.clone() },
            api_base_url: if settings.api_base_url.is_empty() { config.llm.api_base_url.clone() } else { settings.api_base_url.clone() },
            key_env: api_key_cred_name().to_string(),
            api_key,
            temperature: settings.temperature.clone(),
            extra_body: settings.extra_body.clone(),
        });
    }

    // Repair any collisions that survived from v0.7.0 pre-fix code.
    let profiles_before = profiles
        .iter()
        .map(|p| (p.name.clone(), p.key_env.clone()))
        .collect::<Vec<_>>();
    deduplicate_profiles(&mut profiles);
    let profiles_after: Vec<_> = profiles
        .iter()
        .map(|p| (p.name.clone(), p.key_env.clone()))
        .collect();
    if profiles_before != profiles_after {
        // Persist the repaired config immediately so the migration survives.
        Settings {
            model: settings.model.clone(),
            api_base_url: settings.api_base_url.clone(),
            ssh_profiles: settings.ssh_profiles.clone(),
            last_ssh: settings.last_ssh,
            temperature: settings.temperature.clone(),
            extra_body: settings.extra_body.clone(),
            profiles: profiles.iter().map(|d| filar_core::LlmProfile {
                name: d.name.clone(), model: d.model.clone(), api_base_url: d.api_base_url.clone(),
                key_env: d.key_env.clone(), max_tokens: 4096,
                temperature: d.temperature.trim().parse().ok(),
                top_p: None, extra_body: serde_json::from_str(&d.extra_body).ok(),
            }).collect(),
            selected_profile: settings.selected_profile.min(profiles.len().saturating_sub(1)),
            save_dir: settings.save_dir.clone(),
        }.save();
        tracing::info!("persisted deduplicated profile config on startup");
    }

    let selected_profile = settings.selected_profile.min(profiles.len().saturating_sub(1));

    let app = LauncherApp {
        sessions,
        selected_session: None,
        target_mode: if settings.last_ssh > 0 && settings.last_ssh < SSH_SLOTS {
            settings.last_ssh + 1
        } else {
            0
        },
        ssh_slots,
        profiles,
        selected_profile,
        validation_error: String::new(),
        save_dir: settings.save_dir.clone(),
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
            selected_profile: None,
            key_env: "GLM_API_KEY".into(),
            save_dir: None,
            profiles: vec![],
            ssh_targets: vec![],
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
            profiles: vec![],
            selected_profile: 0,
            save_dir: None,
        };
        std::fs::write(&file, serde_json::to_string_pretty(&s).unwrap()).unwrap();

        let loaded: Settings = serde_json::from_str(&std::fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(loaded.model, "test-model");
        assert_eq!(loaded.temperature, "0.5");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_save_preserves_existing_sections() {
        let dir = std::env::temp_dir().join(format!("filar_test_cfg_{}", std::process::id()));
        let app_dir = dir.join("filar");
        let path = app_dir.join("config.toml");
        let _ = std::fs::remove_dir_all(&dir);

        // Write a config with non-LLM sections that must survive.
        let existing = "[llm]\nmodel = \"old\"\napi_base_url = \"https://old.example.com\"\n\n[[ssh_targets]]\nname = \"dev\"\nhost = \"10.0.0.1\"\nuser = \"root\"\nauth = { type = \"agent\" }\n";
        std::fs::create_dir_all(&app_dir).unwrap();
        std::fs::write(&path, existing).unwrap();

        // Simulate what save_config_toml does after loading the config.
        let mut config: filar_core::Config = filar_core::Config::load(&path).unwrap();
        config.llm.model = "new".into();
        let saved = toml::to_string_pretty(&config).unwrap();
        std::fs::write(&path, &saved).unwrap();

        let result = std::fs::read_to_string(&path).unwrap();
        assert!(result.contains("model = \"new\""), "model must be updated");
        assert!(result.contains("[[ssh_targets]]"), "ssh_targets must survive");
        assert!(result.contains("dev"), "ssh target name must survive");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_save_writes_launcher_ssh_profiles() {
        let profiles = vec![
            SshProfile { host: "10.0.0.1".into(), port: "2222".into(), user: "admin".into(), alias: String::new(), save_password: false },
            SshProfile { host: "10.0.0.2".into(), port: "22".into(), user: "root".into(), alias: "prod-web".into(), save_password: true },
            SshProfile::default(), // empty slot — skipped
            SshProfile::default(),
            SshProfile::default(),
        ];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert_eq!(result.len(), 2, "only non-empty profiles become targets");
        assert_eq!(result[0].name, "SSH1", "no alias → uses slot name");
        assert_eq!(result[0].host, "10.0.0.1");
        assert_eq!(result[0].port, 2222);
        assert_eq!(result[1].name, "prod-web", "alias overrides slot name");
        assert_eq!(result[1].host, "10.0.0.2");
        assert_eq!(result[1].port, 22);
        // Auth type follows save_password flag from the profile.
        assert!(matches!(result[0].auth, filar_core::SshAuth::Key { .. }), "save_password=false → Key auth");
        assert!(matches!(result[1].auth, filar_core::SshAuth::Password { .. }), "save_password=true → Password auth");
    }

    #[test]
    fn build_rewrites_all_targets_no_preservation() {
        let profiles = vec![
            SshProfile { host: "10.0.0.1".into(), port: "22".into(), user: "admin".into(), alias: String::new(), save_password: false },
            SshProfile::default(), SshProfile::default(), SshProfile::default(), SshProfile::default(),
        ];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert_eq!(result.len(), 1, "full rewrite only produces current profiles");
        assert_eq!(result[0].name, "SSH1");
    }

    #[test]
    fn build_removes_old_target_when_alias_changes() {
        let profiles = vec![
            SshProfile { host: "10.0.0.1".into(), port: "22".into(), user: "admin".into(), alias: "prod-api".into(), save_password: false },
            SshProfile::default(), SshProfile::default(), SshProfile::default(), SshProfile::default(),
        ];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert!(result.iter().any(|t| t.name == "prod-api"), "new alias target must be added");
        assert!(result.iter().all(|t| t.name != "prod-web"), "old alias target with matching host must be removed");
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn build_removes_old_target_by_host_match() {
        // Profile with no alias — uses slot name SSH1. Old stale target
        // with same host/port/user but different name must be removed.
        let profiles = vec![
            SshProfile { host: "10.0.0.1".into(), port: "22".into(), user: "admin".into(), alias: String::new(), save_password: false },
            SshProfile::default(), SshProfile::default(), SshProfile::default(), SshProfile::default(),
        ];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert_eq!(result.len(), 1, "only SSH1 must remain, stale target removed");
        assert_eq!(result[0].name, "SSH1");
    }

    #[test]
    fn build_clears_when_all_profiles_empty() {
        // All profiles cleared (empty).
        let profiles = vec![SshProfile::default(); SSH_SLOTS];
        let result = build_ssh_targets_from_profiles(&profiles);
        assert!(result.is_empty(), "all launcher targets must be removed when slots are cleared");
    }

    #[test]
    fn unique_profile_name_fills_gaps() {
        let existing = vec![
            LlmProfileData { name: "profile-1".into(), model: String::new(), api_base_url: String::new(), key_env: String::new(), api_key: String::new(), temperature: String::new(), extra_body: String::new() },
            LlmProfileData { name: "profile-3".into(), model: String::new(), api_base_url: String::new(), key_env: String::new(), api_key: String::new(), temperature: String::new(), extra_body: String::new() },
        ];
        let name = unique_profile_name(&existing, "profile");
        assert_eq!(name, "profile-2", "must find first free number, not len+1");
    }

    #[test]
    fn unique_profile_name_first_free() {
        let existing = vec![] as Vec<LlmProfileData>;
        let name = unique_profile_name(&existing, "profile");
        assert_eq!(name, "profile-1");
    }

    #[test]
    fn deduplicate_profiles_renames_collisions() {
        let mut profiles = vec![
            LlmProfileData { name: "dup".into(), model: String::new(), api_base_url: String::new(), key_env: "k1".into(), api_key: String::new(), temperature: String::new(), extra_body: String::new() },
            LlmProfileData { name: "dup".into(), model: String::new(), api_base_url: String::new(), key_env: "k2".into(), api_key: String::new(), temperature: String::new(), extra_body: String::new() },
        ];
        deduplicate_profiles(&mut profiles);
        assert_ne!(profiles[0].name, profiles[1].name, "names must differ after dedup");
    }

    #[test]
    fn deduplicate_three_identical_names_completes() {
        let mut profiles = vec![
            LlmProfileData { name: "x".into(), model: String::new(), api_base_url: String::new(), key_env: "k1".into(), api_key: String::new(), temperature: String::new(), extra_body: String::new() },
            LlmProfileData { name: "x".into(), model: String::new(), api_base_url: String::new(), key_env: "k2".into(), api_key: String::new(), temperature: String::new(), extra_body: String::new() },
            LlmProfileData { name: "x".into(), model: String::new(), api_base_url: String::new(), key_env: "k3".into(), api_key: String::new(), temperature: String::new(), extra_body: String::new() },
        ];
        deduplicate_profiles(&mut profiles);
        assert_ne!(profiles[0].name, profiles[1].name);
        assert_ne!(profiles[1].name, profiles[2].name);
        assert_ne!(profiles[0].name, profiles[2].name, "all three must be unique after dedup");
    }

    #[test]
    fn ssh_cred_name_empty_alias_uses_slot() {
        assert_eq!(ssh_cred_name(0, ""), "ssh_target:SSH1");
        assert_eq!(ssh_cred_name(4, ""), "ssh_target:SSH5");
    }

    #[test]
    fn ssh_cred_name_nonempty_alias_uses_alias() {
        assert_eq!(ssh_cred_name(0, "VPS DE"), "ssh_target:VPS DE");
        assert_eq!(ssh_cred_name(2, "prod-web"), "ssh_target:prod-web");
    }

    #[test]
    fn ssh_cred_name_special_chars_preserved() {
        assert_eq!(ssh_cred_name(0, "my server!"), "ssh_target:my server!");
        assert_eq!(ssh_cred_name(1, "сервер"), "ssh_target:сервер");
    }
}





