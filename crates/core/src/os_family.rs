//! Which family of operating system a host belongs to (#425).
//!
//! A real fleet is not one distribution. A command that is correct on Debian
//! fails on Alma, and the answer is an alternative command rather than a
//! refusal or a skipped host — so the first thing a fleet operation needs is
//! to know what it is talking to.
//!
//! # How the family is decided
//!
//! From `/etc/os-release` alone, which every current Linux distribution
//! ships and which the read-only allowlist already permits reading
//! ([`OS_RELEASE_COMMAND`]). `ID` is consulted first, then `ID_LIKE` — the
//! field the spec exists for: a derivative names its parents there, so
//! Alma, Rocky and Oracle land in the RHEL family without this module
//! having to enumerate every downstream rebuild.
//!
//! # Unknown is a value, not a failure
//!
//! A host whose `/etc/os-release` is missing, unreadable or names something
//! this module does not place yields [`OsFamily::Unknown`]. That is an
//! ordinary answer: a check with a `default` command still runs there, and
//! one without is simply not applicable. Guessing a family from a partial
//! match would be worse than admitting ignorance — the wrong variant runs
//! the wrong command on a real host.

use std::fmt;

/// The command whose output [`OsFamily::from_os_release`] parses.
///
/// Pinned here so the parser and the invocation cannot drift apart. It is
/// inside the read-only allowlist (`cat`), so it runs on a fleet host under
/// the same gate as any check.
pub const OS_RELEASE_COMMAND: &str = "cat /etc/os-release";

/// A family of related distributions, as far as command compatibility goes.
///
/// Deliberately coarse: the point is picking a command variant, and within
/// a family the commands this project cares about behave the same. A finer
/// split would multiply the variants a catalog author has to write without
/// making any of them more correct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub enum OsFamily {
    /// Debian, Ubuntu, Raspbian, Mint and anything declaring `ID_LIKE=debian`.
    Debian,
    /// RHEL, CentOS, Fedora, Alma, Rocky, Oracle Linux, Amazon Linux.
    Rhel,
    /// Alpine — busybox userland, which is why it is its own family rather
    /// than a footnote: busybox `df` rejects `--output`, busybox `ps` takes
    /// different flags, and so on.
    Alpine,
    /// Not determined. Either the host did not answer, or it named
    /// something this module does not place.
    #[default]
    Unknown,
}

impl OsFamily {
    /// The key a catalog entry uses for this family.
    ///
    /// [`Unknown`][Self::Unknown] has no key on purpose: a catalog cannot
    /// declare a command "for hosts we failed to identify". Such a host
    /// gets the `default` variant or nothing.
    pub fn key(&self) -> Option<&'static str> {
        match self {
            Self::Debian => Some("debian"),
            Self::Rhel => Some("rhel"),
            Self::Alpine => Some("alpine"),
            Self::Unknown => None,
        }
    }

    /// Every key a catalog may name, excluding `default`.
    pub const KEYS: [&'static str; 3] = ["debian", "rhel", "alpine"];

    /// Parse the family out of `/etc/os-release` contents.
    ///
    /// `ID` decides when it is one this module places. Otherwise `ID_LIKE`
    /// is consulted, whose value is a space-separated list of closer-to-the
    /// -root distributions, most closely related first — so its tokens are
    /// read in order and the first placeable one wins. Anything else, and
    /// any unreadable input, is [`Unknown`][Self::Unknown].
    ///
    /// Values may be quoted (`ID="alpine"`) or bare (`ID=alpine`); both
    /// spellings are in the wild and both are accepted. Comparison is
    /// case-insensitive, since the spec's lowercase rule is a rule real
    /// files break.
    pub fn from_os_release(contents: &str) -> Self {
        let mut id_like: Option<String> = None;

        for line in contents.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = unquote(value.trim());
            match key.trim() {
                // `ID` is authoritative when we can place it. When we
                // cannot, fall through to ID_LIKE rather than concluding
                // Unknown: that is exactly the derivative case.
                "ID" => {
                    if let Some(family) = Self::from_token(value) {
                        return family;
                    }
                }
                "ID_LIKE" => id_like = Some(value.to_owned()),
                _ => {}
            }
        }

        id_like
            .as_deref()
            .and_then(|likes| likes.split_whitespace().find_map(Self::from_token))
            .unwrap_or(Self::Unknown)
    }

    /// Place a single `ID`/`ID_LIKE` token, case-insensitively.
    ///
    /// The lists are the distributions that actually appear in these
    /// fields. An unrecognised token yields `None` rather than a guess.
    fn from_token(token: &str) -> Option<Self> {
        let token = token.trim().to_ascii_lowercase();
        match token.as_str() {
            "debian" | "ubuntu" | "raspbian" | "linuxmint" | "pop" | "devuan" | "kali"
            | "elementary" => Some(Self::Debian),
            "rhel" | "centos" | "fedora" | "almalinux" | "rocky" | "ol" | "oracle" | "amzn"
            | "scientific" => Some(Self::Rhel),
            "alpine" => Some(Self::Alpine),
            _ => None,
        }
    }
}

impl fmt::Display for OsFamily {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Debian => "debian",
            Self::Rhel => "rhel",
            Self::Alpine => "alpine",
            Self::Unknown => "unknown",
        })
    }
}

/// Strip one layer of matching quotes, as `/etc/os-release` permits.
fn unquote(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' || first == b'\'') && first == last {
            return &value[1..value.len() - 1];
        }
    }
    value
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Real `/etc/os-release` heads, trimmed to the fields that matter.
    const DEBIAN: &str = r#"PRETTY_NAME="Debian GNU/Linux 12 (bookworm)"
NAME="Debian GNU/Linux"
VERSION_ID="12"
ID=debian
HOME_URL="https://www.debian.org/"
"#;

    const UBUNTU: &str = r#"PRETTY_NAME="Ubuntu 24.04.1 LTS"
NAME="Ubuntu"
VERSION_ID="24.04"
ID=ubuntu
ID_LIKE=debian
"#;

    const ALMA: &str = r#"NAME="AlmaLinux"
VERSION="9.4 (Seafoam Ocelot)"
ID="almalinux"
ID_LIKE="rhel centos fedora"
VERSION_ID="9.4"
"#;

    const ROCKY: &str = r#"NAME="Rocky Linux"
ID="rocky"
ID_LIKE="rhel centos fedora"
"#;

    const ALPINE: &str = r#"NAME="Alpine Linux"
ID=alpine
VERSION_ID=3.20.3
PRETTY_NAME="Alpine Linux v3.20"
"#;

    const RHEL: &str = r#"NAME="Red Hat Enterprise Linux"
ID="rhel"
ID_LIKE="fedora"
VERSION_ID="9.4"
"#;

    #[test]
    fn debian_family_is_detected() {
        assert_eq!(OsFamily::from_os_release(DEBIAN), OsFamily::Debian);
        assert_eq!(OsFamily::from_os_release(UBUNTU), OsFamily::Debian);
    }

    #[test]
    fn rhel_family_is_detected() {
        assert_eq!(OsFamily::from_os_release(RHEL), OsFamily::Rhel);
        assert_eq!(OsFamily::from_os_release(ALMA), OsFamily::Rhel);
        assert_eq!(OsFamily::from_os_release(ROCKY), OsFamily::Rhel);
    }

    #[test]
    fn alpine_family_is_detected() {
        assert_eq!(OsFamily::from_os_release(ALPINE), OsFamily::Alpine);
    }

    /// The case `ID_LIKE` exists for: a derivative nobody enumerated.
    #[test]
    fn an_unknown_id_falls_back_to_id_like() {
        let derivative = "ID=some-vendor-linux\nID_LIKE=\"rhel fedora\"\n";
        assert_eq!(OsFamily::from_os_release(derivative), OsFamily::Rhel);
    }

    /// `ID_LIKE` is ordered closest-first, so the first placeable token wins.
    #[test]
    fn id_like_is_read_in_order() {
        let both = "ID=weird\nID_LIKE=\"debian rhel\"\n";
        assert_eq!(OsFamily::from_os_release(both), OsFamily::Debian);
    }

    #[test]
    fn quoted_and_bare_values_both_parse() {
        assert_eq!(OsFamily::from_os_release("ID=\"alpine\""), OsFamily::Alpine);
        assert_eq!(OsFamily::from_os_release("ID=alpine"), OsFamily::Alpine);
        assert_eq!(OsFamily::from_os_release("ID='alpine'"), OsFamily::Alpine);
    }

    #[test]
    fn case_is_ignored() {
        assert_eq!(OsFamily::from_os_release("ID=Debian"), OsFamily::Debian);
        assert_eq!(OsFamily::from_os_release("ID=\"ALPINE\""), OsFamily::Alpine);
    }

    /// Nothing recognisable is `Unknown`, never a guess — the wrong family
    /// would run the wrong command on a real host.
    #[test]
    fn unplaceable_input_is_unknown() {
        for input in [
            "",
            "   \n\n",
            "cat: /etc/os-release: No such file or directory",
            "ID=plan9\nID_LIKE=inferno\n",
            "NAME=\"Something\"\nVERSION_ID=\"1\"\n",
            "not a key-value file at all",
        ] {
            assert_eq!(
                OsFamily::from_os_release(input),
                OsFamily::Unknown,
                "input: {input:?}"
            );
        }
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let noisy = "# a comment\n\n  \nID=alpine\n# trailing\n";
        assert_eq!(OsFamily::from_os_release(noisy), OsFamily::Alpine);
    }

    #[test]
    fn keys_match_the_display_form_and_unknown_has_none() {
        assert_eq!(OsFamily::Debian.key(), Some("debian"));
        assert_eq!(OsFamily::Rhel.key(), Some("rhel"));
        assert_eq!(OsFamily::Alpine.key(), Some("alpine"));
        assert_eq!(OsFamily::Unknown.key(), None);
        for family in [OsFamily::Debian, OsFamily::Rhel, OsFamily::Alpine] {
            assert_eq!(family.key(), Some(family.to_string().as_str()));
            assert!(OsFamily::KEYS.contains(&family.key().unwrap()));
        }
        assert_eq!(OsFamily::Unknown.to_string(), "unknown");
    }

    #[test]
    fn the_detection_command_is_read_only_shaped() {
        // The gate itself is tested in filar-transport; here we only pin
        // that the command stays the simple `cat` the parser expects.
        assert_eq!(OS_RELEASE_COMMAND, "cat /etc/os-release");
    }

    #[test]
    fn default_is_unknown() {
        assert_eq!(OsFamily::default(), OsFamily::Unknown);
    }
}
