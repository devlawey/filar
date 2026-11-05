//! `NO_PROXY` for the Kubernetes API server (#627).
//!
//! `kube-rs` picks the proxy from the kubeconfig's `proxy-url`, else
//! `HTTPS_PROXY` / `https_proxy`, and never reads `NO_PROXY`. `kubectl`
//! (client-go) does, so filar applies it here with the same rules as
//! client-go and curl:
//!
//! - a comma-separated list, case-insensitive, blanks ignored;
//! - `*` matches every host;
//! - an exact host name, or a domain suffix (`corp`, `.corp` and `*.corp` all
//!   match `corp` and `api.corp`);
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
    let domain = name.trim_start_matches("*.").trim_start_matches('.');
    if domain.is_empty() {
        return false;
    }
    host == domain || host.strip_suffix(domain).is_some_and(|rest| rest.ends_with('.'))
}

/// `name:port` or `[v6]:port` → (name, port); anything else is all name.
fn split_port(entry: &str) -> (&str, Option<u16>) {
    if let Some(rest) = entry.strip_prefix('[') {
        if let Some((addr, tail)) = rest.split_once(']') {
            return (addr, tail.strip_prefix(':').and_then(|p| p.parse().ok()));
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

/// `NO_PROXY`, else `no_proxy`, when set and non-empty.
pub(super) fn no_proxy_from_env() -> Option<String> {
    ["NO_PROXY", "no_proxy"]
        .into_iter()
        .filter_map(|name| std::env::var(name).ok())
        .find(|value| !value.trim().is_empty())
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
            assert!(bypasses_proxy(no_proxy, "corp", 443), "{no_proxy}");
            assert!(!bypasses_proxy(no_proxy, "notcorp", 443), "{no_proxy}");
        }
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
