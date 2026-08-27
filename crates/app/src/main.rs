//! `filar` — terminal with an AI agent over SSH.
//!
//! Entry point: initialise logging, load configuration, then either launch the
//! GUI launcher (no CLI args) or go straight to the TUI (with `--target`,
//! `--llm`, `--session` args).

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use filar_agent::OpenAiCompatClient;
use filar_agent::LlmClient;
use filar_core::{secrets, default_base_dir, ChatBlock, Config, CoreError, SecretProvider, SessionStore, StaticSecretProvider};
use filar_transport::{LocalExecutor, SshExecutor, SshTransportConfig};
use filar_tui::TuiConfig;

// ---------------------------------------------------------------------------
// CLI argument parsing
// ---------------------------------------------------------------------------

/// Parsed CLI arguments.
#[derive(Default)]
struct Args {
    target: Option<String>,
    llm: Option<String>,
    session: Option<String>,
    gui_only: bool,
}

impl Args {
    /// Returns `true` if no arguments were provided (triggers GUI launcher).
    fn is_empty(&self) -> bool {
        self.target.is_none() && self.llm.is_none() && self.session.is_none() && !self.gui_only
    }
}

/// Parse `--target`, `--llm`, `--session` from `std::env::args`.
fn parse_args() -> Args {
    let mut args = Args::default();
    let mut iter = std::env::args().skip(1);
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--target" => {
                args.target = iter.next();
            }
            "--llm" => {
                args.llm = iter.next();
            }
            "--session" => {
                args.session = iter.next();
            }
            "--gui-only" => {
                args.gui_only = true;
            }
            "--help" | "-h" => {
                eprintln!("Usage: filar [--target <name>] [--llm <profile>] [--session <id>]");
                eprintln!();
                eprintln!("With no arguments, launches the GUI launcher.");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --target <name>   Connect to this target ('local' or an SSH target name)");
                eprintln!("  --llm <profile>   Use this LLM profile ('default' or a name from config)");
                eprintln!("  --session <id>    Restore a previous session by ID");
                eprintln!("  -h, --help        Show this help message");
                std::process::exit(0);
            }
            other => {
                warn!(arg = other, "unknown argument, ignoring");
            }
        }
    }
    args
}

// ---------------------------------------------------------------------------
// Startup profile resolution
// ---------------------------------------------------------------------------

/// Resolve which LLM profile to use at startup.
///
/// Priority: CLI `--llm` flag > GUI launcher selection > first profile in config.
/// If the resolved profile doesn't exist in `profiles` (deleted/renamed), falls
/// back to the first available profile with a warning.
///
/// This is a free function so it can be unit-tested independently of `main`.
pub fn resolve_startup_profile(
    profiles: &[filar_core::LlmProfile],
    cli_llm: Option<&str>,
    gui_selected: Option<&str>,
) -> String {
    let first_in_config = profiles
        .first()
        .map(|p| p.name.clone())
        .unwrap_or_else(|| "default".into());
    let cli_profile = cli_llm.filter(|n| *n != "default");
    let candidate = cli_profile
        .or(gui_selected)
        .unwrap_or(&first_in_config);
    if profiles.iter().any(|p| p.name == candidate) {
        candidate.to_string()
    } else {
        warn!(candidate = %candidate, "selected startup profile not found, falling back to first in config");
        first_in_config
    }
}

/// Build an [`SshTarget`](filar_core::SshTarget) from a GUI [`SshConnection`](filar_gui::SshConnection).
///
/// When the password is empty (normal after `pending_launch.json` deserialize —
/// it is `#[serde(skip)]`), reads it from the OS keyring under
/// [`filar_core::ssh_cred_name`] — the same key the launcher writes.
/// Target `name` uses [`filar_core::ssh_target_display_name`] so later TUI
/// reconnects (`ssh_target:{name}`) hit the same entry.
///
/// # Sync keyring
///
/// `KeyringSecretProvider::get` is synchronous. Callers must invoke this
/// **once during startup**, before the TUI event loop is running (same
/// pattern as the GUI API-key lookup a few lines below). Do not call it from
/// a hot async path without `spawn_blocking`.
fn resolve_gui_ssh_target(s: &filar_gui::SshConnection) -> filar_core::SshTarget {
    let name = filar_core::ssh_target_display_name(s.slot, &s.alias);
    let password = if s.password.is_empty() {
        let cred = filar_core::KeyringSecretProvider::new();
        let key = filar_core::ssh_cred_name(s.slot, &s.alias);
        cred.get(&key)
            .inspect_err(|e| tracing::debug!(error = %e, %key, "no saved SSH password in keyring"))
            .ok()
    } else {
        Some(s.password.clone())
    };
    filar_core::SshTarget {
        name,
        host: s.host.clone(),
        port: s.port,
        user: s.user.clone(),
        auth: filar_core::SshAuth::Password { password },
        host_key_policy: filar_core::HostKeyPolicy::Tofu,
    }
}

// ---------------------------------------------------------------------------
// LLM client factory
// ---------------------------------------------------------------------------

/// Build an `OpenAiCompatClient` from an LLM profile and secret provider.
///
/// Key resolution order: in-memory `StaticSecretProvider` → OS credential store
/// (keyring) → environment variable. On first keyring hit, the key is cached in
/// `sp` to avoid repeated OS credential store calls.
///
/// An **empty** [`LlmProfile::key_env`] means the profile is keyless (local /
/// air-gapped OpenAI-compatible server): key resolution is skipped and the
/// client is created without an API key (no `Authorization` header).
///
/// This is a free function (not an inline closure) so that it can be
/// unit-tested independently from `main`.
pub fn build_llm_client_from_profile(
    profile: &filar_core::LlmProfile,
    sp: &filar_core::StaticSecretProvider,
    llm_timeout_secs: u64,
) -> std::result::Result<Arc<dyn LlmClient>, CoreError> {
    let key = if !profile.requires_api_key() {
        String::new()
    } else {
        let key = sp.get(&profile.key_env).unwrap_or_default();
        let key = if key.is_empty() {
            let keyring = filar_core::KeyringSecretProvider::new();
            let kr_key = keyring.get(&profile.key_env).ok().unwrap_or_default();
            if !kr_key.is_empty() {
                sp.insert(&profile.key_env, &kr_key);
                kr_key
            } else {
                std::env::var(&profile.key_env).unwrap_or_default()
            }
        } else {
            key
        };
        if key.is_empty() {
            return Err(filar_core::CoreError::Secret(format!(
                "no API key found for profile {}",
                profile.name
            )));
        }
        key
    };
    let llm_config: filar_core::LlmConfig = profile.into();
    Ok(Arc::new(
        OpenAiCompatClient::new_with_key(
            &llm_config,
            Duration::from_secs(llm_timeout_secs),
            &key,
        )
        .inspect_err(|_| warn!("LLM client construction failed"))
        .map_err(|_| filar_core::CoreError::Other("failed to construct LLM client".into()))?,
    ))
}

/// Check whether an LLM profile has a resolvable API key (for `Ctrl+L`).
///
/// Returns `None` when the profile is usable (keyless or key found). Returns
/// `Some(message)` when a required key is missing.
fn check_profile_api_key(
    profile: &filar_core::LlmProfile,
    sp: &filar_core::StaticSecretProvider,
) -> Option<String> {
    if !profile.requires_api_key() {
        return None;
    }
    let key = sp.get(&profile.key_env).unwrap_or_default();
    if !key.is_empty() {
        return None;
    }
    let keyring = filar_core::KeyringSecretProvider::new();
    match keyring.get(&profile.key_env) {
        Ok(k) if !k.is_empty() => {
            sp.insert(&profile.key_env, &k);
            None
        }
        _ => {
            if std::env::var(&profile.key_env)
                .map(|v| !v.is_empty())
                .unwrap_or(false)
            {
                return None;
            }
            Some(format!(
                "no API key found (checked memory, OS store, env '{}')",
                profile.key_env
            ))
        }
    }
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    match run().await {
        Ok(()) => {}
        Err(e) => {
            eprintln!("\n========================================");
            eprintln!("  ERROR: {e:#}");
            eprintln!("========================================\n");
            eprintln!("Press Enter to exit...");
            let mut input = String::new();
            let _ = std::io::stdin().read_line(&mut input);
            std::process::exit(1);
        }
    }
}

async fn run() -> anyhow::Result<()> {
    // ── Logging ────────────────────────────────────────────────────────
    // Logs always go to a rolling file. The *second* sink depends on the mode:
    //
    // - TUI path (default): the terminal is owned by the ratatui interface, so
    //   the subscriber must NOT write to it. Instead, WARN/ERROR records are
    //   mirrored into the chat as `System` blocks via a channel the runner
    //   polls. Startup/teardown errors still reach the terminal through
    //   explicit `eprintln!` (before raw mode / after teardown).
    // - `--gui-only` subprocess: there is no TUI here, so terminal output is
    //   fine and useful — this path keeps the stderr layer unchanged.
    let make_filter =
        || EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // Log directory: base/filar/logs (same base as SessionStore).
    let log_dir = default_base_dir()
        .ok()
        .map(|base| base.join("filar").join("logs"))
        .unwrap_or_else(|| {
            // Fallback: ./logs in the current directory.
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join("logs")
        });

    // Create the log directory if it doesn't exist. Logging is best-effort, so
    // a failure here degrades gracefully (file sink may be inert) rather than
    // aborting startup — but it must not be swallowed silently.
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!(
            "warning: could not create log directory {}: {e}",
            log_dir.display()
        );
    }

    // File appender (daily rotation), non-blocking writer.
    let file_appender = tracing_appender::rolling::daily(&log_dir, "filar.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false); // No ANSI colours in the log file.

    // Peek at `--gui-only` before parsing args in full: the subscriber is
    // global and installed once, so we must pick the right second sink now.
    let gui_only = std::env::args().any(|a| a == "--gui-only");
    let mut log_rx: Option<tokio::sync::mpsc::UnboundedReceiver<String>> = None;
    if gui_only {
        let stderr_layer = tracing_subscriber::fmt::layer().with_writer(std::io::stderr);
        tracing_subscriber::registry()
            .with(make_filter())
            .with(file_layer)
            .with(stderr_layer)
            .init();
    } else {
        // Chat mirror layer for WARN/ERROR — the receiver is handed to the TUI.
        let (chat_log_layer, rx) = filar_tui::chat_log_layer();
        log_rx = Some(rx);
        tracing_subscriber::registry()
            .with(make_filter())
            .with(file_layer)
            .with(chat_log_layer)
            .init();
    }

    // Keep the guard alive for the entire program.
    let _guard = guard;

    info!(log_dir = %log_dir.display(), "filar starting up");

    // ── Config ─────────────────────────────────────────────────────────
    // FILAR_CONFIG env var → appdata → CWD → exe dir → built-in defaults.
    let config = Config::load_default().map_err(|e| anyhow::anyhow!(e))?;

    info!(
        model = %config.llm.model,
        targets = config.ssh_targets.len(),
        llm_profiles = config.llm_profiles.len(),
        confirm_mode = ?config.confirm_mode,
        "configuration loaded"
    );

    // ── Parse CLI args ─────────────────────────────────────────────────
    let args = parse_args();
    let cli_llm_name = args.llm.clone();

    // ── GUI-only mode (subprocess) ──────────────────────────────────
    if args.gui_only {
        info!("running in GUI-only mode (subprocess)");
        filar_gui::run_launcher(&config);
        return Ok(());
    }

    // ── Determine launch parameters ──────────────────────────────────
    // When no CLI args, check for pending launch from a previous GUI
    // session, or spawn the GUI as a subprocess.
    let (target_name, session_id, llm_config, api_key, key_env_name, ssh_target, gui_selected_profile, launch_profiles, launch_ssh_targets, launch_save_dir, launch_arbiter_profile) = if args.is_empty() {
        // Check if the GUI subprocess already saved a launch config.
        let launch = filar_gui::load_pending_launch().or_else(|| {
            // Spawn GUI subprocess.
            info!("spawning GUI subprocess");
            let exe = std::env::current_exe()
                .ok()?;
            let status = std::process::Command::new(&exe)
                .arg("--gui-only")
                .status()
                .ok()?;

            if !status.success() {
                info!("GUI subprocess exited without success");
                return None;
            }

            // Read the pending launch config.
            filar_gui::load_pending_launch()
        });

        match launch {
            Some(launch) => {
                let temperature = if launch.temperature.trim().is_empty() {
                    None
                } else {
                    match launch.temperature.trim().parse::<f32>() {
                        Ok(t) => Some(t),
                        Err(_) => {
                            anyhow::bail!(
                                "Invalid temperature value: '{}'. Expected a number like 0.3.",
                                launch.temperature
                            );
                        }
                    }
                };
                let extra_body = if launch.extra_body.trim().is_empty() {
                    None
                } else {
                    match serde_json::from_str(&launch.extra_body) {
                        Ok(v) => Some(v),
                        Err(e) => {
                            anyhow::bail!(
                                "Invalid extra body JSON: {e}"
                            );
                        }
                    }
                };
                let llm_config = filar_core::LlmConfig {
                    model: launch.model,
                    api_base_url: launch.api_base_url,
                    max_tokens: config.llm.max_tokens,
                    temperature,
                    top_p: None,
                    extra_body,
                };
                llm_config.validate().map_err(|e| anyhow::anyhow!(e))?;

                // Build SshTarget if the user selected SSH in the GUI.
                // Read password from OS credential store if not passed in
                // (since the struct now excludes secrets from serialization).
                // Keyring key MUST match GUI/`ssh_cred_name` (#290), not legacy `ssh{slot}`.
                let ssh_target = launch.ssh.map(|s| {
                    resolve_gui_ssh_target(&s)
                });

                // Empty key_env = keyless local profile (#320): skip keyring lookup.
                let key_env_name = launch.key_env.clone();
                let api_key = if key_env_name.trim().is_empty() {
                    String::new()
                } else if launch.api_key.is_empty() {
                    let cred = filar_core::KeyringSecretProvider::new();
                    cred.get(&key_env_name)
                        .inspect_err(|e| tracing::warn!(error = %e, key_name = %key_env_name, "failed to read API key from OS credential store"))
                        .unwrap_or_default()
                } else {
                    launch.api_key
                };

                (
                    launch.target,
                    launch.session_id,
                    llm_config,
                    api_key,
                    key_env_name,
                    ssh_target,
                    launch.selected_profile,
                    launch.profiles,
                    launch.ssh_targets,
                    launch.save_dir,
                    // Explicit `None` means "same as session" from the launcher —
                    // do not fall back to CWD/app-data config.toml (#360).
                    launch.arbiter_profile,
                )
            }
            None => {
                info!("GUI launcher cancelled, exiting");
                return Ok(());
            }
        }
    } else {
        // CLI mode — use config profiles and env vars.
        let target = args.target.unwrap_or_else(|| "local".into());
        let startup_profile = resolve_startup_profile(&config.llm_profiles, args.llm.as_deref(), None);
        let profile_ref = if startup_profile == config.llm_profiles.first().map(|p| &p.name).cloned().unwrap_or_default() {
            None
        } else {
            Some(startup_profile.as_str())
        };
        let (llm_config, key_env) = config
            .select_llm(profile_ref)
            .map_err(|e| anyhow::anyhow!(e))?;
        let key = if key_env.trim().is_empty() {
            String::new()
        } else {
            secrets::api_key(&key_env).map_err(|e| {
                anyhow::anyhow!("{e}. Set the {key_env} environment variable or use the GUI launcher.")
            })?
        };

        // Look up SSH target from config if not local.
        let ssh_target = if target != "local" {
            config.ssh_target(&target).cloned()
        } else {
            None
        };

        (target, args.session, llm_config, key, key_env, ssh_target, None, config.llm_profiles.clone(), config.ssh_targets.clone(), config.save_dir.clone(), config.arbiter_profile.clone())
    };

    // Validate API key unless the profile is explicitly keyless (empty key_env).
    let requires_key = !key_env_name.trim().is_empty();
    if api_key.is_empty() && requires_key {
        anyhow::bail!("API key is required. Enter it in the GUI launcher or set the {key_env_name} environment variable. For local models, clear Key env on the profile.");
    }

    // ── Create SecretProvider ──────────────────────────────────────────
    // The StaticSecretProvider holds the API key and will also hold dynamic
    // $FILAR_SECRET_N variables added at runtime (via Ctrl+P in the TUI).
    let secret_provider = Arc::new(StaticSecretProvider::new());
    if !api_key.is_empty() {
        let insert_name = if key_env_name.trim().is_empty() {
            secrets::env_vars::GLM_API_KEY
        } else {
            key_env_name.as_str()
        };
        secret_provider.insert(insert_name, &api_key);
        // Keep GLM_API_KEY alias for the initial client when using default name.
        if insert_name != secrets::env_vars::GLM_API_KEY {
            secret_provider.insert(secrets::env_vars::GLM_API_KEY, &api_key);
        }
    }

    let llm: Arc<dyn LlmClient> = Arc::new(if api_key.is_empty() {
        OpenAiCompatClient::new_with_key(
            &llm_config,
            Duration::from_secs(config.timeouts.llm_secs),
            "",
        )?
    } else {
        OpenAiCompatClient::new_with_provider(
            &llm_config,
            Duration::from_secs(config.timeouts.llm_secs),
            secrets::env_vars::GLM_API_KEY,
            &*secret_provider,
        )?
    });

    info!(model = %llm_config.model, keyless = api_key.is_empty(), "LLM client initialised");

    // ── Create executor (local or SSH) ─────────────────────────────────
    let command_timeout = Duration::from_secs(config.timeouts.command_secs);
    let executor: Arc<dyn filar_transport::CommandExecutor> = if target_name == "local" {
        info!("initialising local command executor");
        Arc::new(LocalExecutor::with_timeout(command_timeout).await.map_err(|e| {
            warn!(error = %e, "failed to create local executor");
            anyhow::anyhow!(e)
        })?)
    } else if let Some(ref target) = ssh_target {
        info!(host = %target.host, port = target.port, user = %target.user, "connecting via SSH");
        let ssh = SshExecutor::connect_with_config(
            target,
            SshTransportConfig::default().with_command_timeout(command_timeout),
        )
        .await
        .map_err(|e| {
            warn!(error = %e, "SSH connection failed");
            anyhow::anyhow!(e)
        })?;
        Arc::new(ssh)
    } else {
        anyhow::bail!(
            "SSH target '{target_name}' not found. Use the GUI launcher to enter SSH connection details."
        );
    };

    // ── Load session if specified ──────────────────────────────────────
    #[derive(Default)]
    struct LoadedSession {
        messages: Vec<ChatBlock>,
        input_history: Vec<String>,
        llm_profile: Option<String>,
        tokens_in: u64,
        tokens_out: u64,
        cost_usd: Option<f64>,
        per_profile: HashMap<String, filar_core::ProfileUsage>,
        last_served_model: Option<String>,
        model_per_profile: HashMap<String, String>,
        ssh_info: Option<String>,
        confirm_mode: Option<filar_core::CommandConfirmMode>,
    }
    let loaded = if let Some(ref sid) = session_id {
        info!(session_id = %sid, "loading session");
        match SessionStore::with_default_dir() {
            Ok(store) => match store.load(sid) {
                Ok(Some(session)) => {
                    info!(messages = session.messages.len(), "session loaded");
                    LoadedSession {
                        messages: session.messages,
                        input_history: session.input_history,
                        llm_profile: session.llm_profile,
                        tokens_in: session.tokens_in,
                        tokens_out: session.tokens_out,
                        cost_usd: session.cost_usd,
                        per_profile: session.per_profile,
                        last_served_model: session.last_served_model,
                        model_per_profile: session.model_per_profile,
                        ssh_info: session.ssh_info,
                        confirm_mode: session.confirm_mode,
                    }
                }
                Ok(None) => {
                    warn!(session_id = %sid, "session not found");
                    LoadedSession::default()
                }
                Err(e) => {
                    warn!(error = %e, "failed to load session");
                    LoadedSession::default()
                }
            },
            Err(e) => {
                warn!(error = %e, "failed to initialise session store");
                LoadedSession::default()
            }
        }
    } else {
        LoadedSession::default()
    };

    // ── Launch TUI ─────────────────────────────────────────────────────
    let default_profile_name =
        resolve_startup_profile(&launch_profiles, cli_llm_name.as_deref(), gui_selected_profile.as_deref());
    let tui_config = TuiConfig {
        target_name: target_name.clone(),
        confirm_mode: loaded.confirm_mode.unwrap_or(config.confirm_mode),
        llm_profile: default_profile_name.clone(),
        initial_messages: loaded.messages,
        initial_input_history: loaded.input_history,
        initial_llm_profile: loaded.llm_profile,
        initial_tokens_in: loaded.tokens_in,
        initial_tokens_out: loaded.tokens_out,
        initial_cost_usd: loaded.cost_usd,
        initial_per_profile: loaded.per_profile,
        initial_last_served_model: loaded.last_served_model,
        initial_model_per_profile: loaded.model_per_profile,
        ssh_target: ssh_target.clone(),
        is_local: ssh_target.is_none(),
        initial_ssh_info: loaded.ssh_info,
        secret_provider: secret_provider.clone(),
        log_rx,
        profiles: launch_profiles,
        default_profile_name,
        llm_factory: {
            let llm_timeout_secs = config.timeouts.llm_secs;
            Arc::new(move |profile: &filar_core::LlmProfile, sp: &filar_core::StaticSecretProvider| {
                build_llm_client_from_profile(profile, sp, llm_timeout_secs)
            })
        },
        key_checker: {
            let key_checker_provider = secret_provider.clone();
            Arc::new(move |profile: &filar_core::LlmProfile| {
                check_profile_api_key(profile, &key_checker_provider)
            })
        },
        ssh_targets: launch_ssh_targets,
        save_dir: launch_save_dir,
        command_timeout,
        arbiter_enabled: config.arbiter_enabled,
        // GUI: from pending_launch (including explicit None = same as session).
        // CLI: from config.toml (see tuple construction above).
        arbiter_profile: launch_arbiter_profile,
    };

    info!("launching TUI");
    filar_tui::run(llm, executor, tui_config)
        .await
        .map_err(|e| {
            warn!(error = %e, "TUI error");
            anyhow::anyhow!(e)
        })?;

    info!("filar shutting down");
    Ok(())
}

#[cfg(test)]
mod tests {
    use filar_core::{LlmProfile, SecretProvider, StaticSecretProvider};

    use super::{
        build_llm_client_from_profile, check_profile_api_key, resolve_gui_ssh_target,
    };

    // ── GUI→TUI SSH keyring handoff (#290) ──────────────────────────────

    #[test]
    fn resolve_gui_ssh_uses_cred_name_not_legacy_ssh_slot() {
        // Empty password → keyring lookup uses ssh_target:…, never ssh{N}.
        let conn = filar_gui::SshConnection {
            host: "10.0.0.1".into(),
            port: 22,
            user: "root".into(),
            password: String::new(),
            slot: 0,
            alias: String::new(),
        };
        let target = resolve_gui_ssh_target(&conn);
        assert_eq!(target.name, "SSH1", "empty alias → SSH{{slot+1}}");
        assert_eq!(
            filar_core::ssh_cred_name(conn.slot, &conn.alias),
            "ssh_target:SSH1",
            "keyring key must match GUI save contract"
        );
        // Legacy key `ssh0` must NOT be what we document/use.
        assert_ne!(
            filar_core::ssh_cred_name(conn.slot, &conn.alias),
            format!("ssh{}", conn.slot)
        );
    }

    #[test]
    fn resolve_gui_ssh_alias_sets_target_name_and_cred_key() {
        let conn = filar_gui::SshConnection {
            host: "example.com".into(),
            port: 2222,
            user: "admin".into(),
            password: "in-memory".into(),
            slot: 2,
            alias: "prod-web".into(),
        };
        let target = resolve_gui_ssh_target(&conn);
        assert_eq!(target.name, "prod-web");
        assert_eq!(target.host, "example.com");
        assert_eq!(target.port, 2222);
        assert_eq!(target.user, "admin");
        assert_eq!(
            filar_core::ssh_cred_name(conn.slot, &conn.alias),
            "ssh_target:prod-web"
        );
        match target.auth {
            filar_core::SshAuth::Password { password: Some(p) } => {
                assert_eq!(p, "in-memory", "in-memory password must win over keyring");
            }
            other => panic!("expected Password auth, got {other:?}"),
        }
    }

    // ── Key resolution tests ────────────────────────────────────────────

    #[test]
    fn key_resolve_prefers_in_memory() {
        let sp = StaticSecretProvider::new();
        sp.insert("MY_KEY", "mem-key");
        let result = resolve_test_key("MY_KEY", &sp, None);
        assert_eq!(result, "mem-key", "must prefer in-memory cache");
    }

    #[test]
    fn key_resolve_falls_back_to_env() {
        std::env::set_var("FILAR_TEST_KEY_171", "env-key");
        let sp = StaticSecretProvider::new();
        let result = resolve_test_key("FILAR_TEST_KEY_171", &sp, None);
        assert_eq!(result, "env-key", "must fall back to env var");
        std::env::remove_var("FILAR_TEST_KEY_171");
    }

    #[test]
    fn key_resolve_caches_keyring_result() {
        let sp = StaticSecretProvider::new();
        let result = resolve_test_key("KR_KEY_1", &sp, Some("kr-secret"));
        assert_eq!(result, "kr-secret");
        if !result.is_empty() {
            sp.insert("KR_KEY_1", &result);
        }
        let cached = sp.get("KR_KEY_1").unwrap_or_default();
        assert_eq!(cached, "kr-secret", "key must be cached after first keyring read");
    }

    #[test]
    fn key_resolve_memory_overrides_keyring() {
        let sp = StaticSecretProvider::new();
        sp.insert("MY_KEY_2", "mem-val");
        let result = resolve_test_key("MY_KEY_2", &sp, Some("kr-val"));
        assert_eq!(result, "mem-val", "memory must take precedence over keyring");
    }

    #[test]
    fn key_resolve_env_rejects_empty() {
        std::env::set_var("FILAR_TEST_EMPTY", "");
        let sp = StaticSecretProvider::new();
        let result = resolve_test_key("FILAR_TEST_EMPTY", &sp, None);
        assert!(result.is_empty(), "empty env var must be treated as missing");
        std::env::remove_var("FILAR_TEST_EMPTY");
    }

    fn resolve_test_key(
        key_env: &str,
        sp: &StaticSecretProvider,
        keyring_value: Option<&str>,
    ) -> String {
        let key = sp.get(key_env).unwrap_or_default();
        if !key.is_empty() {
            return key;
        }
        if let Some(v) = keyring_value {
            if !v.is_empty() {
                return v.to_string();
            }
        }
        std::env::var(key_env).unwrap_or_default()
    }

    // ── Factory regression tests (#183) ─────────────────────────────────

    #[test]
    fn factory_with_valid_key_returns_ok() {
        let key_value = "sk-fake-key-for-testing-12345";
        let sp = StaticSecretProvider::new();
        sp.insert("TEST_KEY_ENV", key_value);
        let profile = LlmProfile {
            name: "test-profile".into(),
            model: "test-model".into(),
            api_base_url: "https://example.com/api".into(),
            max_tokens: 1024,
            key_env: "TEST_KEY_ENV".into(),
            temperature: None,
            top_p: None,
            extra_body: None,
        };
        let result = build_llm_client_from_profile(&profile, &sp, 60);
        assert!(result.is_ok(), "factory must succeed with a valid key");
    }

    #[test]
    fn factory_with_missing_key_returns_err() {
        let sp = StaticSecretProvider::new();
        let profile = LlmProfile {
            name: "no-key-profile".into(),
            model: "test-model".into(),
            api_base_url: "https://example.com/api".into(),
            max_tokens: 1024,
            key_env: "NONEXISTENT_KEY".into(),
            temperature: None,
            top_p: None,
            extra_body: None,
        };
        let result = build_llm_client_from_profile(&profile, &sp, 60);
        assert!(result.is_err(), "factory must fail when no key is available");
    }

    #[test]
    fn factory_empty_key_env_is_keyless_ok() {
        let sp = StaticSecretProvider::new();
        let profile = LlmProfile {
            name: "ollama-local".into(),
            model: "llama3.1".into(),
            api_base_url: "http://localhost:11434/v1".into(),
            max_tokens: 1024,
            key_env: String::new(),
            temperature: None,
            top_p: None,
            extra_body: None,
        };
        let result = build_llm_client_from_profile(&profile, &sp, 60);
        assert!(
            result.is_ok(),
            "empty key_env must create a keyless client without error"
        );
    }

    #[test]
    fn key_checker_allows_empty_key_env() {
        let sp = StaticSecretProvider::new();
        let profile = LlmProfile {
            name: "local".into(),
            model: "m".into(),
            api_base_url: "http://127.0.0.1:11434/v1".into(),
            max_tokens: 1024,
            key_env: String::new(),
            temperature: None,
            top_p: None,
            extra_body: None,
        };
        assert!(
            check_profile_api_key(&profile, &sp).is_none(),
            "key_checker must accept keyless profiles"
        );
    }

    #[test]
    fn key_checker_rejects_missing_key_when_key_env_set() {
        let sp = StaticSecretProvider::new();
        let profile = LlmProfile {
            name: "cloud".into(),
            model: "m".into(),
            api_base_url: "https://example.com/v1".into(),
            max_tokens: 1024,
            key_env: "FILAR_TEST_MISSING_KEY_ENV_XYZ".into(),
            temperature: None,
            top_p: None,
            extra_body: None,
        };
        let msg = check_profile_api_key(&profile, &sp);
        assert!(msg.is_some(), "key_checker must fail when required key is missing");
        assert!(
            msg.unwrap().contains("no API key found"),
            "message must mention missing key"
        );
    }

    #[test]
    fn factory_error_does_not_contain_key_value() {
        let key_value = "sk-or-v1-super-secret-key-that-must-not-leak";
        let sp = StaticSecretProvider::new();
        let profile = LlmProfile {
            name: "leak-test-profile".into(),
            model: "test-model".into(),
            api_base_url: "https://example.com/api".into(),
            max_tokens: 1024,
            key_env: "LEAK_TEST_KEY".into(),
            temperature: None,
            top_p: None,
            extra_body: None,
        };
        // Test 1: key is absent → error must NOT contain the test value.
        let result = build_llm_client_from_profile(&profile, &sp, 60);
        if let Err(ref e) = result {
            let msg = format!("{e}");
            assert!(
                !msg.contains(key_value),
                "error must NOT contain the key value '{key_value}', but got: {msg}"
            );
            assert!(
                msg.contains("no API key found"),
                "error must mention that the key is missing, got: {msg}"
            );
        }
        // Test 2: insert the key → factory must succeed (no error to check).
        sp.insert("LEAK_TEST_KEY", key_value);
        let result2 = build_llm_client_from_profile(&profile, &sp, 60);
        assert!(result2.is_ok(), "factory must succeed after inserting key");
    }

    // ── Startup profile tests (#194) ───────────────────────────────────

    use super::resolve_startup_profile;

    fn make_profile(name: &str) -> LlmProfile {
        LlmProfile {
            name: name.into(),
            model: name.into(),
            api_base_url: String::new(),
            max_tokens: 1024,
            key_env: String::new(),
            temperature: None,
            top_p: None,
            extra_body: None,
        }
    }

    #[test]
    fn startup_profile_uses_first_in_config_when_no_cli_or_gui() {
        let profiles = vec![make_profile("glm"), make_profile("deepseek")];
        let result = resolve_startup_profile(&profiles, None, None);
        assert_eq!(result, "glm");
    }

    #[test]
    fn startup_profile_uses_cli_over_gui() {
        let profiles = vec![make_profile("glm"), make_profile("deepseek")];
        let result = resolve_startup_profile(&profiles, Some("deepseek"), Some("glm"));
        assert_eq!(result, "deepseek");
    }

    #[test]
    fn startup_profile_uses_gui_when_no_cli() {
        let profiles = vec![make_profile("glm"), make_profile("deepseek")];
        let result = resolve_startup_profile(&profiles, None, Some("deepseek"));
        assert_eq!(result, "deepseek");
    }

    #[test]
    fn startup_profile_ignores_cli_default() {
        let profiles = vec![make_profile("glm"), make_profile("deepseek")];
        let result = resolve_startup_profile(&profiles, Some("default"), Some("deepseek"));
        assert_eq!(result, "deepseek");
    }

    #[test]
    fn startup_profile_falls_back_when_not_found() {
        let profiles = vec![make_profile("glm")];
        let result = resolve_startup_profile(&profiles, Some("deleted_profile"), None);
        assert_eq!(result, "glm");
    }

    #[test]
    fn startup_profile_empty_profiles_returns_default() {
        let profiles: Vec<LlmProfile> = vec![];
        let result = resolve_startup_profile(&profiles, None, None);
        assert_eq!(result, "default");
    }
}
