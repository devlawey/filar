//! SSH user authentication shared by the command executor and the
//! interactive terminal.
//!
//! One entry point, [`authenticate`], dispatches on [`SshAuth`]: a key file,
//! a password, or keys held by a running SSH agent. With the agent, filar
//! only asks it for signatures — a private key never leaves the agent and is
//! never read by filar.

use std::borrow::Cow;
use std::sync::Arc;

use russh::client::{self, Handle};
use russh::AgentAuthError;
use russh::keys::agent::client::{AgentClient, AgentStream};
use russh::keys::agent::AgentIdentity;
use russh::keys::*;
use tracing::{debug, info};

use filar_core::{CoreError, Result, SecretProvider, SshAuth, SshTarget};

use crate::ssh::resolve_ssh_password;

/// Hint appended to "agent unavailable" errors.
const START_AGENT_HINT: &str = "start one and load a key: eval \"$(ssh-agent)\" && ssh-add";

/// Hint appended to "agent has no keys" errors.
const ADD_KEY_HINT: &str = "load a key with: ssh-add [path-to-key]";

/// Default named pipe of the Windows OpenSSH agent service.
#[cfg_attr(not(windows), allow(dead_code))]
const WINDOWS_OPENSSH_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

/// A connected agent client, whatever its underlying stream.
type DynAgent = AgentClient<Box<dyn AgentStream + Send + Unpin + 'static>>;

/// Authenticate `session` as `target.user` with the target's auth method.
///
/// `secrets` supplies the SSH password (`"SSH_PASSWORD"`) for password auth
/// when no explicit password is set on the target — no direct env reads here.
pub(crate) async fn authenticate<H: client::Handler>(
    session: &mut Handle<H>,
    target: &SshTarget,
    secrets: &dyn SecretProvider,
) -> Result<()> {
    match &target.auth {
        SshAuth::Key { path, passphrase } => {
            let key_path = crate::key::resolve_key_path(path.as_deref());
            // An explicit passphrase (keyring / prompt, filled by the caller)
            // wins over the provider's `SSH_KEY_PASSPHRASE`.
            let passphrase = passphrase.clone().or_else(|| {
                secrets
                    .get(filar_core::secrets::env_vars::SSH_KEY_PASSPHRASE)
                    .ok()
                    .filter(|p| !p.is_empty())
            });
            // Reading (and decrypting) the key is blocking; keep it off the
            // runtime.
            let key_pair = tokio::task::spawn_blocking({
                let key_path = key_path.clone();
                let name = target.name.clone();
                move || crate::key::load_key(&key_path, passphrase.as_deref(), &name)
            })
            .await
            .map_err(|e| CoreError::Other(format!("SSH key loader task failed: {e}")))??;

            let hash = session
                .best_supported_rsa_hash()
                .await
                .map_err(|e| CoreError::Other(format!("RSA hash negotiation failed: {e}")))?
                .flatten();

            let auth_res = session
                .authenticate_publickey(
                    &target.user,
                    PrivateKeyWithHashAlg::new(Arc::new(key_pair), hash),
                )
                .await
                .map_err(|e| CoreError::Other(format!("publickey auth failed: {e}")))?;

            if !auth_res.success() {
                return Err(CoreError::Other("publickey authentication rejected".into()));
            }
            info!("SSH authenticated via key");
        }
        SshAuth::Password { password } => {
            let password = resolve_ssh_password(password, secrets)?;

            let auth_res = session
                .authenticate_password(&target.user, &password)
                .await
                .map_err(|e| CoreError::Other(format!("password auth failed: {e}")))?;

            if !auth_res.success() {
                return Err(CoreError::Other("password authentication rejected".into()));
            }
            info!("SSH authenticated via password");
        }
        SshAuth::Agent => {
            let mut agent = connect_agent().await?;
            authenticate_with_agent(session, &target.user, &mut agent).await?;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Agent
// ---------------------------------------------------------------------------

/// Where the user pointed filar at the SSH agent.
#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentEndpoint {
    /// `SSH_AUTH_SOCK` names it: a Unix-domain socket, or a named pipe on
    /// Windows (OpenSSH for Windows honours the variable too). Only this
    /// endpoint is used — no fallback to another agent.
    Configured(String),
    /// `SSH_AUTH_SOCK` is unset. Unix: no agent. Windows: the OpenSSH
    /// agent's standard pipe, then Pageant.
    Unset,
}

/// Pick the agent endpoint from the value of `SSH_AUTH_SOCK`.
fn agent_endpoint(auth_sock: Option<String>) -> AgentEndpoint {
    match auth_sock.filter(|s| !s.trim().is_empty()) {
        Some(path) => AgentEndpoint::Configured(path),
        None => AgentEndpoint::Unset,
    }
}

/// Error for an agent that cannot be reached; `detail` says why.
fn agent_unavailable(detail: &str) -> CoreError {
    CoreError::Other(format!("SSH agent unavailable: {detail}; {START_AGENT_HINT}"))
}

/// Connect to the running SSH agent.
async fn connect_agent() -> Result<DynAgent> {
    connect_agent_at(agent_endpoint(std::env::var("SSH_AUTH_SOCK").ok())).await
}

#[cfg(unix)]
async fn connect_agent_at(endpoint: AgentEndpoint) -> Result<DynAgent> {
    let path = match endpoint {
        AgentEndpoint::Configured(p) => p,
        AgentEndpoint::Unset => return Err(agent_unavailable("SSH_AUTH_SOCK is not set")),
    };
    let agent = AgentClient::connect_uds(&path)
        .await
        .map_err(|e| agent_unavailable(&format!("cannot open agent socket {path}: {e}")))?;
    debug!(socket = %path, "connected to SSH agent");
    Ok(agent.dynamic())
}

#[cfg(windows)]
async fn connect_agent_at(endpoint: AgentEndpoint) -> Result<DynAgent> {
    let (pipe, configured) = match endpoint {
        AgentEndpoint::Configured(p) => (p, true),
        AgentEndpoint::Unset => (WINDOWS_OPENSSH_AGENT_PIPE.to_string(), false),
    };
    match AgentClient::connect_named_pipe(&pipe).await {
        Ok(agent) => {
            debug!(pipe = %pipe, "connected to OpenSSH agent");
            Ok(agent.dynamic())
        }
        // An agent the user named explicitly fails closed: switching to
        // another agent would sign with a different set of keys.
        Err(pipe_err) if configured => Err(agent_unavailable(&format!(
            "cannot open agent pipe {pipe} from SSH_AUTH_SOCK ({pipe_err})"
        ))),
        Err(pipe_err) => {
            // Fall back to PuTTY's Pageant, the other common Windows agent.
            match AgentClient::connect_pageant().await {
                Ok(agent) => {
                    debug!("connected to Pageant");
                    Ok(agent.dynamic())
                }
                Err(_) => Err(agent_unavailable(&format!(
                    "cannot open agent pipe {pipe} ({pipe_err}) and Pageant is not running; \
                     on Windows enable the \"OpenSSH Authentication Agent\" service and run ssh-add"
                ))),
            }
        }
    }
}

/// Offer each of the agent's keys to the server in turn until one is
/// accepted.
///
/// Errors: the agent holds no keys, or the server accepted none of them.
async fn authenticate_with_agent<H, S>(
    session: &mut Handle<H>,
    user: &str,
    agent: &mut AgentClient<S>,
) -> Result<()>
where
    H: client::Handler,
    S: AgentStream + Send + Unpin,
{
    let identities = agent_identities(agent).await?;

    let rsa_hash = session
        .best_supported_rsa_hash()
        .await
        .map_err(|e| CoreError::Other(format!("RSA hash negotiation failed: {e}")))?
        .flatten();

    let total = identities.len();
    for identity in identities {
        let key = identity.public_key().into_owned();
        let hash = if matches!(key.algorithm(), Algorithm::Rsa { .. }) {
            rsa_hash
        } else {
            None
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256);
        // A failed signature ends the attempt: russh still waits for that
        // signature on this session, so a later key cannot be offered on it.
        let res = session
            .authenticate_publickey_with(user, key, hash, agent)
            .await
            .map_err(|e| agent_sign_error(&e, &fingerprint.to_string()))?;
        if res.success() {
            info!(%fingerprint, "SSH authenticated via agent");
            return Ok(());
        }
        debug!(%fingerprint, "server rejected agent key");
    }

    Err(CoreError::Other(format!(
        "SSH agent authentication rejected: the server accepted none of the agent's {total} key(s)"
    )))
}

/// Error for a signature the agent did not produce for key `fingerprint`.
///
/// Typical causes: a key added with `ssh-add -c` whose confirmation was
/// declined, or a hardware key that was not touched in time.
fn agent_sign_error(err: &AgentAuthError, fingerprint: &str) -> CoreError {
    match err {
        AgentAuthError::Key(e) => CoreError::Other(format!(
            "SSH agent did not sign with key {fingerprint} ({e}); the remaining agent keys \
             were not tried. Confirm the key if the agent asks (ssh-add -c, hardware key), \
             or remove it from the agent: ssh-add -d <key>"
        )),
        AgentAuthError::Send(e) => {
            CoreError::Other(format!("SSH agent auth failed: connection lost ({e})"))
        }
    }
}

/// The agent's identities, or an error with a hint if it holds none.
async fn agent_identities<S>(agent: &mut AgentClient<S>) -> Result<Vec<AgentIdentity>>
where
    S: AgentStream + Send + Unpin,
{
    let identities = agent
        .request_identities()
        .await
        .map_err(|e| agent_unavailable(&format!("cannot list agent keys: {e}")))?;
    if identities.is_empty() {
        return Err(CoreError::Other(format!("SSH agent has no keys; {ADD_KEY_HINT}")));
    }
    debug!(count = identities.len(), keys = ?describe(&identities), "agent identities");
    Ok(identities)
}

/// Comments of the agent's identities, for debug logging (no key material).
fn describe(identities: &[AgentIdentity]) -> Vec<Cow<'_, str>> {
    identities
        .iter()
        .map(|i| match i {
            AgentIdentity::PublicKey { comment, .. } | AgentIdentity::Certificate { comment, .. } => {
                Cow::Borrowed(comment.as_str())
            }
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_set_auth_sock_is_the_configured_endpoint() {
        assert_eq!(
            agent_endpoint(Some("/tmp/ssh-X/agent.1".into())),
            AgentEndpoint::Configured("/tmp/ssh-X/agent.1".into())
        );
    }

    #[test]
    fn an_unset_or_blank_auth_sock_is_unset() {
        assert_eq!(agent_endpoint(None), AgentEndpoint::Unset);
        assert_eq!(agent_endpoint(Some("  ".into())), AgentEndpoint::Unset);
    }

    #[test]
    fn a_refused_signature_names_the_key_and_the_way_out() {
        let err = AgentAuthError::Key(russh::keys::Error::AgentFailure);
        let msg = agent_sign_error(&err, "SHA256:abc").to_string();
        assert!(msg.contains("did not sign with key SHA256:abc"), "{msg}");
        assert!(msg.contains("remaining agent keys were not tried"), "{msg}");
        assert!(msg.contains("ssh-add -d"), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn no_auth_sock_is_an_agent_unavailable_error_with_a_hint() {
        let err = connect_agent_at(AgentEndpoint::Unset).await.err().expect("error");
        let msg = err.to_string();
        assert!(msg.contains("SSH agent unavailable"), "{msg}");
        assert!(msg.contains("SSH_AUTH_SOCK"), "{msg}");
        assert!(msg.contains("ssh-add"), "{msg}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_missing_socket_is_an_agent_unavailable_error_with_a_hint() {
        let path = std::env::temp_dir().join(format!("filar-no-agent-{}", uuid::Uuid::new_v4()));
        let err = connect_agent_at(AgentEndpoint::Configured(path.display().to_string()))
            .await
            .err()
            .expect("error");
        let msg = err.to_string();
        assert!(msg.contains("cannot open agent socket"), "{msg}");
        assert!(msg.contains("eval \"$(ssh-agent)\""), "{msg}");
    }

    /// A fake agent that answers one REQUEST_IDENTITIES with an empty list.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_agent_without_keys_is_an_error_with_an_ssh_add_hint() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        // Not `temp_dir()`: macOS's `$TMPDIR` is long enough to push a socket
        // path past `SUN_LEN` (104 bytes), and `bind` then fails.
        let id = uuid::Uuid::new_v4().simple().to_string();
        let dir = std::path::PathBuf::from(format!("/tmp/filar-agent-{}", &id[..12]));
        std::fs::create_dir_all(&dir).expect("temp dir");
        let sock = dir.join("agent.sock");
        let listener = tokio::net::UnixListener::bind(&sock).expect("bind");

        let server = tokio::spawn(async move {
            let (mut conn, _) = listener.accept().await.expect("accept");
            let mut len = [0u8; 4];
            conn.read_exact(&mut len).await.expect("read len");
            let mut body = vec![0u8; u32::from_be_bytes(len) as usize];
            conn.read_exact(&mut body).await.expect("read body");
            assert_eq!(body, [11], "SSH_AGENTC_REQUEST_IDENTITIES");
            // SSH_AGENT_IDENTITIES_ANSWER (12) with a zero key count.
            conn.write_all(&[0, 0, 0, 5, 12, 0, 0, 0, 0]).await.expect("write");
        });

        let mut agent = connect_agent_at(AgentEndpoint::Configured(sock.display().to_string()))
            .await
            .expect("connect to fake agent");
        let err = agent_identities(&mut agent).await.expect_err("error");
        let msg = err.to_string();
        assert!(msg.contains("SSH agent has no keys"), "{msg}");
        assert!(msg.contains("ssh-add"), "{msg}");

        server.await.expect("fake agent");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
