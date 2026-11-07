//! Kubernetes `Secret` metadata without values (#646).
//!
//! A `Secret` read from the API is turned into [`SecretInfo`] right here,
//! inside the transport: the type has no field that could hold a value.
//! What leaves is the type, key names with value sizes, times, labels,
//! annotations without the last applied copy, owners, `immutable` and, for
//! `kubernetes.io/tls`, the public certificate's subject, SAN, issuer and
//! validity. `tls.key` is never parsed and never leaves.

use std::collections::BTreeMap;
use std::time::{SystemTime, UNIX_EPOCH};

use k8s_openapi::api::core::v1::{Pod, Secret};
use x509_cert::der::{Decode, DecodePem};
use x509_cert::ext::pkix::name::GeneralName;
use x509_cert::ext::pkix::SubjectAltName;
use x509_cert::Certificate;

/// One key of a Secret: its name and the size of its value in bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretKeyInfo {
    /// Key name (`password`, `tls.crt`).
    pub name: String,
    /// Size of the decoded value in bytes.
    pub bytes: usize,
}

/// The public part of a TLS Secret's certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TlsCertificateInfo {
    /// Subject (`CN=shop.example.com`).
    pub subject: String,
    /// Subject alternative names (DNS and IP).
    pub sans: Vec<String>,
    /// Issuer.
    pub issuer: String,
    /// `notBefore`, RFC 3339 UTC.
    pub not_before: String,
    /// `notAfter`, RFC 3339 UTC.
    pub not_after: String,
    /// Seconds until `notAfter`; negative when expired.
    pub seconds_left: i64,
}

impl TlsCertificateInfo {
    /// Expired, or expiring within 30 days.
    pub fn status(&self) -> &'static str {
        if self.seconds_left < 0 {
            "expired"
        } else if self.seconds_left < 30 * 24 * 3600 {
            "expires within 30 days"
        } else {
            "valid"
        }
    }
}

/// Metadata of a Kubernetes `Secret` — never its values.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretInfo {
    /// Namespace.
    pub namespace: String,
    /// Name.
    pub name: String,
    /// `type` (`Opaque`, `kubernetes.io/tls`, `kubernetes.io/dockerconfigjson`).
    pub type_: String,
    /// Keys with value sizes, sorted.
    pub keys: Vec<SecretKeyInfo>,
    /// Age in whole seconds.
    pub age_seconds: Option<u64>,
    /// `creationTimestamp`, RFC 3339.
    pub created: Option<String>,
    /// Latest `managedFields` time, RFC 3339: the last change.
    pub last_modified: Option<String>,
    /// Labels.
    pub labels: BTreeMap<String, String>,
    /// Annotations without `kubectl.kubernetes.io/last-applied-configuration`.
    pub annotations: BTreeMap<String, String>,
    /// Owners (`Certificate shop-tls`).
    pub owners: Vec<String>,
    /// `immutable`.
    pub immutable: bool,
    /// For `kubernetes.io/tls`: the parsed `tls.crt`, when it parses.
    pub tls: Option<TlsCertificateInfo>,
}

/// How a pod refers to a Secret (#646).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SecretRef {
    /// Secret name.
    pub secret: String,
    /// Key, when one key is referred to.
    pub key: Option<String>,
    /// Where: `env DB_PASSWORD (container main)`, `envFrom (container main)`,
    /// `volume certs`, `imagePullSecrets`.
    pub via: String,
    /// `optional: true` on the reference.
    pub optional: bool,
}

const LAST_APPLIED: &str = "kubectl.kubernetes.io/last-applied-configuration";

/// Turn a Secret into its metadata; the values are dropped here.
pub(super) fn secret_info(secret: Secret) -> SecretInfo {
    let meta = secret.metadata;
    let mut keys: BTreeMap<String, usize> = BTreeMap::new();
    let mut tls = None;
    if let Some(data) = &secret.data {
        for (key, value) in data {
            keys.insert(key.clone(), value.0.len());
            if key == "tls.crt" {
                tls = parse_certificate(&value.0);
            }
        }
    }
    if let Some(data) = &secret.string_data {
        for (key, value) in data {
            keys.insert(key.clone(), value.len());
        }
    }
    let last_modified = meta
        .managed_fields
        .iter()
        .flatten()
        .filter_map(|field| field.time.as_ref())
        .map(|time| time.0)
        .max()
        .map(|time| time.to_string());
    let created = meta.creation_timestamp.as_ref().map(|t| t.0.to_string());
    let age_seconds = meta.creation_timestamp.as_ref().map(|t| {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        now.saturating_sub(t.0.as_second()).max(0) as u64
    });
    let mut annotations = meta.annotations.unwrap_or_default();
    annotations.remove(LAST_APPLIED);
    SecretInfo {
        namespace: meta.namespace.unwrap_or_default(),
        name: meta.name.unwrap_or_default(),
        type_: secret.type_.unwrap_or_else(|| "Opaque".into()),
        keys: keys
            .into_iter()
            .map(|(name, bytes)| SecretKeyInfo { name, bytes })
            .collect(),
        age_seconds,
        created,
        last_modified,
        labels: meta.labels.unwrap_or_default(),
        annotations,
        owners: meta
            .owner_references
            .unwrap_or_default()
            .into_iter()
            .map(|owner| format!("{} {}", owner.kind, owner.name))
            .collect(),
        immutable: secret.immutable.unwrap_or(false),
        tls,
    }
}

/// Parse the first certificate of a PEM (or DER) `tls.crt`.
pub fn parse_certificate(bytes: &[u8]) -> Option<TlsCertificateInfo> {
    let certificate = Certificate::from_pem(bytes)
        .ok()
        .or_else(|| Certificate::from_der(bytes).ok())?;
    let tbs = certificate.tbs_certificate();
    let validity = tbs.validity();
    let not_before = validity.not_before.to_unix_duration().as_secs() as i64;
    let not_after = validity.not_after.to_unix_duration().as_secs() as i64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let mut sans = Vec::new();
    if let Ok(Some((_, san))) = tbs.get_extension::<SubjectAltName>() {
        for name in san.0 {
            match name {
                GeneralName::DnsName(dns) => sans.push(format!("DNS:{}", dns.as_str())),
                GeneralName::IpAddress(ip) => {
                    let bytes = ip.as_bytes();
                    let text = match bytes.len() {
                        4 => std::net::Ipv4Addr::new(bytes[0], bytes[1], bytes[2], bytes[3])
                            .to_string(),
                        16 => {
                            let mut octets = [0u8; 16];
                            octets.copy_from_slice(bytes);
                            std::net::Ipv6Addr::from(octets).to_string()
                        }
                        _ => continue,
                    };
                    sans.push(format!("IP:{text}"));
                }
                _ => {}
            }
        }
    }
    Some(TlsCertificateInfo {
        subject: tbs.subject().to_string(),
        sans,
        issuer: tbs.issuer().to_string(),
        not_before: rfc3339(not_before),
        not_after: rfc3339(not_after),
        seconds_left: not_after - now,
    })
}

fn rfc3339(seconds: i64) -> String {
    k8s_openapi::jiff::Timestamp::from_second(seconds)
        .map(|t| t.to_string())
        .unwrap_or_else(|_| seconds.to_string())
}

/// Every reference a pod's spec makes to a Secret: env, envFrom, volumes,
/// projected volumes and `imagePullSecrets`.
pub(super) fn pod_secret_refs(pod: &Pod) -> Vec<SecretRef> {
    let mut refs = Vec::new();
    let Some(spec) = pod.spec.as_ref() else {
        return refs;
    };
    for pull in spec.image_pull_secrets.iter().flatten() {
        refs.push(SecretRef {
            secret: pull.name.clone(),
            key: None,
            via: "imagePullSecrets".into(),
            optional: false,
        });
    }
    let containers = spec
        .init_containers
        .iter()
        .flatten()
        .chain(spec.containers.iter());
    for container in containers {
        for env in container.env.iter().flatten() {
            if let Some(selector) = env
                .value_from
                .as_ref()
                .and_then(|from| from.secret_key_ref.as_ref())
            {
                refs.push(SecretRef {
                    secret: selector.name.clone(),
                    key: Some(selector.key.clone()),
                    via: format!("env {} (container {})", env.name, container.name),
                    optional: selector.optional.unwrap_or(false),
                });
            }
        }
        for from in container.env_from.iter().flatten() {
            if let Some(source) = from.secret_ref.as_ref() {
                refs.push(SecretRef {
                    secret: source.name.clone(),
                    key: None,
                    via: format!("envFrom (container {})", container.name),
                    optional: source.optional.unwrap_or(false),
                });
            }
        }
    }
    for volume in spec.volumes.iter().flatten() {
        if let Some(source) = volume.secret.as_ref() {
            let name = source.secret_name.clone().unwrap_or_default();
            let keys: Vec<String> = source
                .items
                .iter()
                .flatten()
                .map(|i| i.key.clone())
                .collect();
            if keys.is_empty() {
                refs.push(SecretRef {
                    secret: name,
                    key: None,
                    via: format!("volume {}", volume.name),
                    optional: source.optional.unwrap_or(false),
                });
            } else {
                for key in keys {
                    refs.push(SecretRef {
                        secret: name.clone(),
                        key: Some(key),
                        via: format!("volume {}", volume.name),
                        optional: source.optional.unwrap_or(false),
                    });
                }
            }
        }
        for source in volume
            .projected
            .iter()
            .flat_map(|p| p.sources.iter().flatten())
        {
            if let Some(secret) = source.secret.as_ref() {
                refs.push(SecretRef {
                    secret: secret.name.clone(),
                    key: None,
                    via: format!("projected volume {}", volume.name),
                    optional: secret.optional.unwrap_or(false),
                });
            }
        }
    }
    refs
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::ByteString;

    /// A self-signed certificate for `shop.example.com` (SAN DNS
    /// shop.example.com, www.shop.example.com and IP 10.0.0.1), valid
    /// 2026-01-01 to 2027-01-01. Public data only.
    const CERT: &str = include_str!("testdata/shop-tls.crt");
    /// A stand-in for a private key (not a real one): must never appear in
    /// anything this module returns.
    const KEY: &str = "tls-key-material-that-must-never-leave-the-transport";

    fn secret(type_: &str, data: &[(&str, &[u8])]) -> Secret {
        Secret {
            metadata: k8s_openapi::apimachinery::pkg::apis::meta::v1::ObjectMeta {
                name: Some("db".into()),
                namespace: Some("shop".into()),
                annotations: Some(BTreeMap::from([
                    (
                        LAST_APPLIED.to_string(),
                        "{\"data\":{\"password\":\"c3VwZXJzZWNyZXQ=\"}}".to_string(),
                    ),
                    ("team".to_string(), "web".to_string()),
                ])),
                ..Default::default()
            },
            type_: Some(type_.into()),
            data: Some(
                data.iter()
                    .map(|(k, v)| (k.to_string(), ByteString(v.to_vec())))
                    .collect(),
            ),
            string_data: Some(BTreeMap::from([(
                "extra".to_string(),
                "plain-extra-value".to_string(),
            )])),
            ..Default::default()
        }
    }

    #[test]
    fn a_secret_becomes_metadata_without_a_single_value() {
        let info = secret_info(secret(
            "Opaque",
            &[("password", b"supersecret"), ("user", b"admin")],
        ));
        let keys: Vec<_> = info
            .keys
            .iter()
            .map(|k| (k.name.as_str(), k.bytes))
            .collect();
        assert_eq!(keys, [("extra", 17), ("password", 11), ("user", 5)]);
        assert_eq!(info.type_, "Opaque");
        assert_eq!(
            info.annotations.get("team").map(String::as_str),
            Some("web")
        );
        assert!(!info.annotations.contains_key(LAST_APPLIED));
        let printed = format!("{info:?}");
        for value in [
            "supersecret",
            "c3VwZXJzZWNyZXQ",
            "admin",
            "plain-extra-value",
        ] {
            assert!(!printed.contains(value), "{value} leaked: {printed}");
        }
    }

    #[test]
    fn a_tls_secret_shows_its_certificate_and_never_its_key() {
        let info = secret_info(secret(
            "kubernetes.io/tls",
            &[("tls.crt", CERT.as_bytes()), ("tls.key", KEY.as_bytes())],
        ));
        let tls = info.tls.as_ref().expect("certificate parsed");
        assert_eq!(tls.subject, "CN=shop.example.com");
        assert_eq!(tls.issuer, "CN=shop.example.com");
        assert_eq!(
            tls.sans,
            [
                "DNS:shop.example.com",
                "DNS:www.shop.example.com",
                "IP:10.0.0.1"
            ]
        );
        assert_eq!(tls.not_before, "2026-01-01T00:00:00Z");
        assert_eq!(tls.not_after, "2027-01-01T00:00:00Z");
        let printed = format!("{info:?}");
        assert!(!printed.contains(KEY), "{printed}");
        assert!(!printed.contains("BEGIN CERTIFICATE"), "{printed}");
        assert!(parse_certificate(b"not a certificate").is_none());
    }

    #[test]
    fn certificate_status_counts_thirty_days() {
        let mut tls = parse_certificate(CERT.as_bytes()).unwrap();
        tls.seconds_left = -1;
        assert_eq!(tls.status(), "expired");
        tls.seconds_left = 3600;
        assert_eq!(tls.status(), "expires within 30 days");
        tls.seconds_left = 90 * 24 * 3600;
        assert_eq!(tls.status(), "valid");
    }
}
