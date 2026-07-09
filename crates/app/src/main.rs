//! `filar` — terminal with an AI agent over SSH.
//!
//! Entry point: initialise logging, load configuration, then either launch the
//! GUI launcher (no CLI args) or go straight to the TUI (with `--target`,
//! `--llm`, `--session` args).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::EnvFilter;

use filar_agent::glm::GlmClient;
use filar_agent::LlmClient;
use filar_core::{secrets, Config, SessionStore, StaticSecretProvider};
use filar_transport::{LocalExecutor, SshExecutor};
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
    // Set up dual logging: stderr (for CLI mode) + file (for TUI mode).
    // The file is at %APPDATA%/filar/filar.log on Windows.
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info"));

    // Determine log directory.
    let log_dir = SessionStore::new()
        .ok()
        .and_then(|s| s.dir().parent().map(|p| p.to_path_buf()))
        .unwrap_or_else(|| {
            // Fallback: current directory.
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
        });

    // Create the log directory if it doesn't exist.
    let _ = std::fs::create_dir_all(&log_dir);

    // Set up file appender (daily rotation).
    let file_appender = tracing_appender::rolling::daily(&log_dir, "filar.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);

    // Build subscriber with both stderr and file layers.
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr);
    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(file_writer)
        .with_ansi(false); // No ANSI colors in log file.

    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .with(file_layer)
        .init();

    // Keep the guard alive for the entire program.
    let _guard = guard;

    info!(log_dir = %log_dir.display(), "filar starting up");

    // ── Config ─────────────────────────────────────────────────────────
    // Try: FILAR_CONFIG env var → current dir → exe dir → built-in defaults.
    let config = if let Ok(path) = std::env::var("FILAR_CONFIG") {
        let path = PathBuf::from(path);
        info!(path = %path.display(), "loading configuration from FILAR_CONFIG");
        Config::load(&path).map_err(|e| anyhow::anyhow!(e))?
    } else if std::path::Path::new("config.toml").exists() {
        info!("loading config.toml from current directory");
        Config::load("config.toml").map_err(|e| anyhow::anyhow!(e))?
    } else if let Ok(exe) = std::env::current_exe() {
        let exe_dir = exe.parent().unwrap_or(std::path::Path::new("."));
        let path = exe_dir.join("config.toml");
        if path.exists() {
            info!(path = %path.display(), "loading config from exe directory");
            Config::load(&path).map_err(|e| anyhow::anyhow!(e))?
        } else {
            info!("no config.toml found, using built-in defaults");
            Config::default()
        }
    } else {
        info!("no config.toml found, using built-in defaults");
        Config::default()
    };

    info!(
        model = %config.llm.model,
        targets = config.ssh_targets.len(),
        llm_profiles = config.llm_profiles.len(),
        confirm_mode = ?config.confirm_mode,
        "configuration loaded"
    );

    // ── Parse CLI args ─────────────────────────────────────────────────
    let args = parse_args();

    // ── GUI-only mode (subprocess) ──────────────────────────────────
    if args.gui_only {
        info!("running in GUI-only mode (subprocess)");
        filar_gui::run_launcher(&config);
        return Ok(());
    }

    // ── Determine launch parameters ──────────────────────────────────
    // When no CLI args, check for pending launch from a previous GUI
    // session, or spawn the GUI as a subprocess.
    let (target_name, session_id, llm_config, api_key, ssh_target) = if args.is_empty() {
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
                let llm_config = filar_core::LlmConfig {
                    model: launch.model,
                    api_base_url: launch.api_base_url,
                    max_tokens: config.llm.max_tokens,
                };

                // Build SshTarget if the user selected SSH in the GUI.
                let ssh_target = launch.ssh.map(|s| filar_core::SshTarget {
                    name: "gui-ssh".to_string(),
                    host: s.host,
                    port: s.port,
                    user: s.user,
                    auth: filar_core::SshAuth::Password {
                        password: if s.password.is_empty() { None } else { Some(s.password) },
                    },
                    host_key_policy: filar_core::HostKeyPolicy::Tofu,
                });

                (
                    launch.target,
                    launch.session_id,
                    llm_config,
                    launch.api_key,
                    ssh_target,
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
        let llm_name = args.llm.unwrap_or_else(|| "default".into());
        let profile_ref = if llm_name == "default" { None } else { Some(llm_name.as_str()) };
        let (llm_config, key_env) = config
            .select_llm(profile_ref)
            .map_err(|e| anyhow::anyhow!(e))?;
        let key = secrets::api_key(&key_env).map_err(|e| {
            anyhow::anyhow!("{e}. Set the {key_env} environment variable or use the GUI launcher.")
        })?;

        // Look up SSH target from config if not local.
        let ssh_target = if target != "local" {
            config.ssh_target(&target).cloned()
        } else {
            None
        };

        (target, args.session, llm_config, key, ssh_target)
    };

    // Validate API key.
    if api_key.is_empty() {
        anyhow::bail!("API key is required. Enter it in the GUI launcher or set the GLM_API_KEY environment variable.");
    }

    // ── Create SecretProvider ──────────────────────────────────────────
    // The StaticSecretProvider holds the API key and will also hold dynamic
    // $FILAR_SECRET_N variables added at runtime (via Ctrl+P in the TUI).
    let secret_provider = Arc::new(StaticSecretProvider::new());
    secret_provider.insert(secrets::env_vars::GLM_API_KEY, &api_key);

    let llm: Arc<dyn LlmClient> = Arc::new(GlmClient::new_with_provider(
        &llm_config,
        Duration::from_secs(config.timeouts.llm_secs),
        secrets::env_vars::GLM_API_KEY,
        &*secret_provider,
    )?);

    info!(model = %llm_config.model, "LLM client initialised");

    // ── Create executor (local or SSH) ─────────────────────────────────
    let executor: Arc<dyn filar_transport::CommandExecutor> = if target_name == "local" {
        info!("initialising local command executor");
        Arc::new(LocalExecutor::new().await.map_err(|e| {
            warn!(error = %e, "failed to create local executor");
            anyhow::anyhow!(e)
        })?)
    } else if let Some(ref target) = ssh_target {
        info!(host = %target.host, port = target.port, user = %target.user, "connecting via SSH");
        let ssh = SshExecutor::connect(target).await.map_err(|e| {
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
    let initial_messages = if let Some(ref sid) = session_id {
        info!(session_id = %sid, "loading session");
        match SessionStore::new() {
            Ok(store) => match store.load(sid) {
                Ok(Some(session)) => {
                    info!(messages = session.messages.len(), "session loaded");
                    session.messages
                }
                Ok(None) => {
                    warn!(session_id = %sid, "session not found");
                    vec![]
                }
                Err(e) => {
                    warn!(error = %e, "failed to load session");
                    vec![]
                }
            },
            Err(e) => {
                warn!(error = %e, "failed to initialise session store");
                vec![]
            }
        }
    } else {
        vec![]
    };

    // ── Launch TUI ─────────────────────────────────────────────────────
    let tui_config = TuiConfig {
        target_name: target_name.clone(),
        confirm_mode: config.confirm_mode,
        llm_profile: target_name,
        initial_messages,
        ssh_target: ssh_target.clone(),
        is_local: ssh_target.is_none(),
        secret_provider,
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
