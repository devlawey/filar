//! `NO_PROXY` for the Kubernetes API server (#627).
//!
//! `kube-rs` picks the proxy from the kubeconfig's `proxy-url`, else
//! `HTTPS_PROXY` / `https_proxy`, and never reads `NO_PROXY`. `kubectl`
//! (client-go) does, so filar applies it here with the same rules as
//! client-go and curl:
//!
//! - a comma-separated list, case-insensitive, blanks ignored;
//! - `*` matches every host;
//! - a host name matches itself and its subdomains (`corp` matches `corp` and
//!   `api.corp`); a leading dot or `*.` means subdomains only (`.corp` and
//!   `*.corp` match `api.corp`, not `corp`) — as in Go's `httpproxy`, which
//!   client-go uses;
//! - an IP address, or a CIDR block (`10.0.0.0/8`, `fd00::/8`);
//! - an entry with a port (`host:6443`, `[::1]:6443`) matches that port only.
//!
//! A `proxy-url` set on the cluster in the kubeconfig was chosen for that
//! cluster explicitly and is not overridden by `NO_PROXY` — client-go does
//! the same.

use std::net::IpAddr;

use kube::config::Kubeconfig;
use kube::Config;

/// Whether `host` (as in a URL, IPv6 with or without brackets) on `port`
/// is exempt from the proxy under `no_proxy`.
pub fn bypasses_proxy(no_proxy: &str, host: &str, port: u16) -> bool {
    let host = host.trim_start_matches('[').trim_end_matches(']').to_ascii_lowercase();
    let host_ip: Option<IpAddr> = host.parse().ok();
    no_proxy
        .split(',')
        .map(|entry| entry.trim().to_ascii_lowercase())
        .filter(|entry| !entry.is_empty())
        .any(|entry| entry_matches(&entry, &host, host_ip, port))
}

fn entry_matches(entry: &str, host: &str, host_ip: Option<IpAddr>, port: u16) -> bool {
    if entry == "*" {
        return true;
    }
    if let Some(net) = cidr(entry) {
        return host_ip.is_some_and(|ip| net.contains(ip));
    }
    if let Ok(ip) = entry.parse::<IpAddr>() {
        return host_ip == Some(ip);
    }
    let (name, entry_port) = split_port(entry);
    if entry_port.is_some_and(|p| p != port) {
        return false;
    }
    if let Ok(ip) = name.parse::<IpAddr>() {
        return host_ip == Some(ip);
    }
    let subdomains_only = name.starts_with('.') || name.starts_with("*.");
    let domain = name.trim_start_matches("*.").trim_start_matches('.');
    if domain.is_empty() {
        return false;
    }
    let subdomain = host.strip_suffix(domain).is_some_and(|rest| rest.ends_with('.'));
    subdomain || (!subdomains_only && host == domain)
}

/// `name:port` or `[v6]:port` → (name, port); anything else is all name.
fn split_port(entry: &str) -> (&str, Option<u16>) {
    if let Some(rest) = entry.strip_prefix('[') {
        if let Some((addr, tail)) = rest.split_once(']') {
            return match tail {
                "" => (addr, None),
                // A port that does not parse matches no port at all, rather
                // than every port (#627 review).
                tail => match tail.strip_prefix(':').and_then(|p| p.parse().ok()) {
                    Some(port) => (addr, Some(port)),
                    None => (addr, Some(0)),
                },
            };
        }
    }
    match entry.rsplit_once(':') {
        Some((name, port)) if !name.contains(':') => match port.parse() {
            Ok(port) => (name, Some(port)),
            Err(_) => (entry, None),
        },
        _ => (entry, None),
    }
}

struct Cidr {
    base: IpAddr,
    prefix: u32,
}

fn cidr(entry: &str) -> Option<Cidr> {
    let (addr, prefix) = entry.split_once('/')?;
    let base: IpAddr = addr.parse().ok()?;
    let prefix: u32 = prefix.parse().ok()?;
    let max = if base.is_ipv4() { 32 } else { 128 };
    (prefix <= max).then_some(Cidr { base, prefix })
}

impl Cidr {
    fn contains(&self, ip: IpAddr) -> bool {
        match (self.base, ip) {
            (IpAddr::V4(base), IpAddr::V4(ip)) => {
                let mask = u32::MAX.checked_shl(32 - self.prefix).unwrap_or(0);
                u32::from(base) & mask == u32::from(ip) & mask
            }
            (IpAddr::V6(base), IpAddr::V6(ip)) => {
                let mask = u128::MAX.checked_shl(128 - self.prefix).unwrap_or(0);
                u128::from(base) & mask == u128::from(ip) & mask
            }
            _ => false,
        }
    }
}

/// The non-empty `proxy-url` of the cluster behind `context` in the
/// kubeconfig, if the user set one for that cluster explicitly.
pub(super) fn kubeconfig_proxy(kubeconfig: &Kubeconfig, context: &str) -> Option<String> {
    let cluster = kubeconfig
        .contexts
        .iter()
        .find(|c| c.name == context)?
        .context
        .as_ref()?
        .cluster
        .clone();
    kubeconfig
        .clusters
        .iter()
        .find(|c| c.name == cluster)?
        .cluster
        .as_ref()?
        .proxy_url
        .clone()
        .filter(|p| !p.trim().is_empty())
}

/// The proxy settings of the environment: `HTTPS_PROXY` / `https_proxy`
/// and `NO_PROXY` / `no_proxy`, upper case first, empty values ignored.
#[derive(Debug, Clone, Default)]
pub(super) struct ProxyEnv {
    pub https_proxy: Option<String>,
    pub no_proxy: Option<String>,
}

impl ProxyEnv {
    /// Read from the process environment.
    pub(super) fn from_process() -> Self {
        let first = |names: [&str; 2]| {
            names
                .into_iter()
                .filter_map(|name| std::env::var(name).ok())
                .find(|value| !value.trim().is_empty())
        };
        Self {
            https_proxy: first(["HTTPS_PROXY", "https_proxy"]),
            no_proxy: first(["NO_PROXY", "no_proxy"]),
        }
    }
}

/// Set the proxy of `config` (loaded from the kubeconfig): the cluster's own
/// `proxy-url` when it has one, else the environment's `HTTPS_PROXY` unless
/// the server is in `NO_PROXY`. Errors only on an unparsable `HTTPS_PROXY`.
pub(super) fn choose_proxy(
    config: &mut Config,
    explicit_proxy: bool,
    env: &ProxyEnv,
) -> Result<(), ()> {
    if explicit_proxy {
        return Ok(());
    }
    // kube-rs has already read the process environment; the choice is made
    // again from `env` so that it is the same one the caller passed.
    config.proxy_url = env
        .https_proxy
        .as_deref()
        .map(|proxy| proxy.parse())
        .transpose()
        .map_err(|_| ())?;
    apply_no_proxy(config, false, env.no_proxy.as_deref());
    Ok(())
}

/// Drop a proxy that came from the environment when the API server is in
/// `no_proxy`. A proxy the kubeconfig names for the cluster is kept.
pub(super) fn apply_no_proxy(config: &mut Config, explicit_proxy: bool, no_proxy: Option<&str>) {
    if explicit_proxy || config.proxy_url.is_none() {
        return;
    }
    let Some(no_proxy) = no_proxy else {
        return;
    };
    let url = &config.cluster_url;
    let Some(host) = url.host() else {
        return;
    };
    let port = url
        .port_u16()
        .unwrap_or(if url.scheme_str() == Some("http") { 80 } else { 443 });
    if bypasses_proxy(no_proxy, host, port) {
        config.proxy_url = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_and_domain_suffixes() {
        for no_proxy in ["corp", ".corp", "*.corp", " CORP "] {
            assert!(bypasses_proxy(no_proxy, "api.corp", 443), "{no_proxy}");
            assert!(!bypasses_proxy(no_proxy, "notcorp", 443), "{no_proxy}");
        }
        assert!(bypasses_proxy("corp", "corp", 443), "a bare name matches itself");
        assert!(!bypasses_proxy(".corp", "corp", 443), "a leading dot is subdomains only");
        assert!(!bypasses_proxy("*.corp", "corp", 443), "so is *.");
        assert!(bypasses_proxy("k8s.example.com", "K8S.example.com", 6443));
        assert!(!bypasses_proxy("k8s.example.com", "api.example.com", 6443));
    }

    #[test]
    fn ips_cidrs_and_ipv6() {
        assert!(bypasses_proxy("127.0.0.1,localhost", "127.0.0.1", 18443));
        assert!(!bypasses_proxy("127.0.0.1", "127.0.0.2", 443));
        assert!(bypasses_proxy("10.0.0.0/8", "10.20.30.40", 6443));
        assert!(!bypasses_proxy("10.0.0.0/8", "11.0.0.1", 6443));
        assert!(!bypasses_proxy("10.0.0.0/8", "ten.corp", 6443), "a name is not in a block");
        assert!(bypasses_proxy("0.0.0.0/0", "8.8.8.8", 443));
        assert!(bypasses_proxy("::1", "[::1]", 6443));
        assert!(bypasses_proxy("fd00::/8", "fd12::5", 6443));
        assert!(!bypasses_proxy("10.0.0.0/33", "10.0.0.1", 443), "bad prefix matches nothing");
    }

    #[test]
    fn ports_star_and_blanks() {
        assert!(bypasses_proxy("api.corp:6443", "api.corp", 6443));
        assert!(!bypasses_proxy("api.corp:6443", "api.corp", 443));
        assert!(bypasses_proxy("[::1]:6443", "::1", 6443));
        assert!(!bypasses_proxy("[::1]:6443", "::1", 443));
        assert!(!bypasses_proxy("[::1]:65536", "::1", 6443), "a bad port matches no port");
        assert!(!bypasses_proxy("[::1]x", "::1", 6443));
        assert!(bypasses_proxy("[::1]", "::1", 6443));
        assert!(bypasses_proxy("127.0.0.1:18443", "127.0.0.1", 18443));
        assert!(bypasses_proxy("*", "anything.example", 443));
        assert!(!bypasses_proxy("", "127.0.0.1", 443));
        assert!(!bypasses_proxy(" , ,", "127.0.0.1", 443));
        assert!(!bypasses_proxy(".", "corp", 443));
    }

    fn proxied(url: &str) -> Config {
        let mut config = Config::new(url.parse().unwrap());
        config.proxy_url = Some("http://proxy:3128".parse().unwrap());
        config
    }

    #[test]
    fn env_proxy_is_dropped_only_for_exempt_servers() {
        let mut local = proxied("https://127.0.0.1:6443");
        apply_no_proxy(&mut local, false, Some("127.0.0.1"));
        assert!(local.proxy_url.is_none(), "exempt server goes direct");

        let mut remote = proxied("https://k8s.example.com");
        apply_no_proxy(&mut remote, false, Some("127.0.0.1"));
        assert!(remote.proxy_url.is_some(), "other servers keep the proxy");

        let mut plain = proxied("http://127.0.0.1");
        apply_no_proxy(&mut plain, false, Some("127.0.0.1:80"));
        assert!(plain.proxy_url.is_none(), "http defaults to port 80");

        let mut unset = proxied("https://127.0.0.1:6443");
        apply_no_proxy(&mut unset, false, None);
        assert!(unset.proxy_url.is_some());
    }

    #[test]
    fn a_kubeconfig_proxy_wins_over_no_proxy() {
        let mut config = proxied("https://127.0.0.1:6443");
        apply_no_proxy(&mut config, true, Some("*"));
        assert!(config.proxy_url.is_some());
    }

    #[test]
    fn the_kubeconfig_proxy_is_found_through_the_context() {
        let yaml = r#"
apiVersion: v1
kind: Config
clusters:
- name: with-proxy
  cluster: { server: "https://a:6443", proxy-url: "http://p:3128" }
- name: plain
  cluster: { server: "https://b:6443" }
contexts:
- name: ctx-a
  context: { cluster: with-proxy, user: u }
- name: ctx-b
  context: { cluster: plain, user: u }
users:
- name: u
  user: {}
"#;
        let kubeconfig = Kubeconfig::from_yaml(yaml).unwrap();
        assert_eq!(kubeconfig_proxy(&kubeconfig, "ctx-a").as_deref(), Some("http://p:3128"));
        assert_eq!(kubeconfig_proxy(&kubeconfig, "ctx-b"), None);
        assert_eq!(kubeconfig_proxy(&kubeconfig, "missing"), None);
    }
}
