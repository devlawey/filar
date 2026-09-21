//! Declarative catalog of fleet checks (#424).
//!
//! A *fleet check* is a named, human-authored description of one read-only
//! question to ask every host in a group: what to run, how to parse the
//! answer, and which columns of that answer are compared across hosts. The
//! model never composes these commands — it picks from this catalog, and a
//! person decides what the catalog contains.
//!
//! # Two layers, one order
//!
//! [`FleetCheckCatalog::builtin`] parses a TOML document compiled into the
//! binary with `include_str!`. Nothing is fetched, and nothing is read from
//! disk: the built-in set works on a machine that has never seen a config
//! file, which is what zero-install means on this side of the wire too.
//!
//! On top of that, [`FleetCheckCatalog::load`] reads an optional user
//! catalog — `fleet_checks.toml` next to the effective `config.toml` (see
//! [`user_catalog_path`]) — whose entries **augment** the built-ins. A user
//! entry that reuses a built-in name is rejected rather than silently
//! shadowing it: a catalog where the same name means different things on
//! two machines is worse than one that says so out loud.
//!
//! # Why a hand-editable file is not a new attack surface
//!
//! Every command declared here still goes through
//! `filar_transport::ReadOnlyExecutor` (#419), whose allowlist is compiled
//! into the binary and is not configurable. Writing `rm -rf /` into a check
//! is possible; *executing* it is not — the gate refuses the command before
//! it reaches the host. Editability buys expressiveness, not privilege.
//!
//! # Partial failure is the normal case
//!
//! Loading never returns `Err` for a bad *entry*. A malformed check is
//! collected into [`FleetCheckCatalog::rejected`] with a reason naming it,
//! and every other check keeps working — one typo in a user file must not
//! cost a person their whole catalog. A user file that is not valid TOML at
//! all is rejected as a unit, by the same mechanism, and the built-ins
//! survive it.

use std::collections::HashSet;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The built-in catalog, compiled into the binary. Never read from disk.
const BUILTIN_CATALOG_TOML: &str = include_str!("builtin_fleet_checks.toml");

/// Name of the user catalog file, looked for next to `config.toml`.
pub const USER_CATALOG_FILE: &str = "fleet_checks.toml";

/// Environment variable naming an explicit user catalog path, checked before
/// the config directory. Mirrors `FILAR_CONFIG` for the config file itself.
pub const USER_CATALOG_ENV: &str = "FILAR_FLEET_CHECKS";

/// Longest accepted check name, in bytes.
const MAX_NAME_LEN: usize = 64;

// ---------------------------------------------------------------------------
// Source
// ---------------------------------------------------------------------------

/// Where a check — or a rejection — came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckSource {
    /// The catalog compiled into the binary.
    Builtin,
    /// A user catalog file at this path.
    User(PathBuf),
}

impl fmt::Display for CheckSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Builtin => f.write_str("built-in catalog"),
            Self::User(path) => write!(f, "{}", path.display()),
        }
    }
}

// ---------------------------------------------------------------------------
// A single check
// ---------------------------------------------------------------------------

/// One validated fleet check.
///
/// Construction goes through the catalog, so every instance has already
/// passed [`validation`](FleetCheckCatalog::load): the name is a non-empty
/// identifier, the command is a non-empty single line, and `compare` is
/// consistent with `preprocessor`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FleetCheck {
    name: String,
    description: String,
    command: String,
    preprocessor: Option<String>,
    compare: Vec<String>,
    source: CheckSource,
}

impl FleetCheck {
    /// Stable identifier, unique across the merged catalog.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// One-line human description, shown when picking a check.
    pub fn description(&self) -> &str {
        &self.description
    }

    /// The command sent to each host, verbatim.
    ///
    /// Still subject to `filar_transport::check_read_only` — this string is
    /// a declaration, not a permission.
    pub fn command(&self) -> &str {
        &self.command
    }

    /// Name of the preprocessor that turns the output into a table, if any.
    ///
    /// `None` means the output is compared as raw text. The name is resolved
    /// by the consumer (`filar_agent::preprocess`), not here: this crate
    /// knows nothing about preprocessor implementations.
    pub fn preprocessor(&self) -> Option<&str> {
        self.preprocessor.as_deref()
    }

    /// Columns whose values form the compared value of each row.
    ///
    /// Empty exactly when [`preprocessor`](Self::preprocessor) is `None`, in
    /// which case the whole raw output is the unit of comparison.
    pub fn compare(&self) -> &[String] {
        &self.compare
    }

    /// Which catalog this check came from.
    pub fn source(&self) -> &CheckSource {
        &self.source
    }

    /// `true` when the check ships with the binary.
    pub fn is_builtin(&self) -> bool {
        matches!(self.source, CheckSource::Builtin)
    }
}

// ---------------------------------------------------------------------------
// Rejections
// ---------------------------------------------------------------------------

/// A catalog entry (or a whole file) that was refused, and why.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RejectedCheck {
    source: CheckSource,
    index: Option<usize>,
    name: Option<String>,
    reason: String,
}

impl RejectedCheck {
    /// Which catalog the rejected entry came from.
    pub fn source(&self) -> &CheckSource {
        &self.source
    }

    /// Zero-based position of the entry in its file, or `None` when the file
    /// as a whole was refused.
    pub fn index(&self) -> Option<usize> {
        self.index
    }

    /// The entry's declared name, when it had a readable one.
    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    /// Human-readable explanation of the first problem found.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for RejectedCheck {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match (self.index, &self.name) {
            (Some(index), Some(name)) => {
                write!(f, "check #{} ('{name}') in {}", index + 1, self.source)?
            }
            (Some(index), None) => write!(f, "check #{} in {}", index + 1, self.source)?,
            (None, _) => write!(f, "{}", self.source)?,
        }
        write!(f, ": {}", self.reason)
    }
}

impl std::error::Error for RejectedCheck {}

// ---------------------------------------------------------------------------
// Wire format
// ---------------------------------------------------------------------------

/// Top level of a catalog file: an array of `[[check]]` tables.
///
/// Entries are kept as raw tables so each one can be deserialised — and
/// rejected — on its own. Deserialising the array into typed checks in one
/// step would make a single bad entry cost the whole file.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CatalogFile {
    #[serde(default)]
    check: Vec<toml::Table>,
}

/// One `[[check]]` table, before validation.
///
/// `deny_unknown_fields` turns a misspelled key into a named error instead
/// of a silently ignored line — the difference between "your check does
/// nothing" and "your check has a typo on it".
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct RawCheck {
    name: String,
    description: String,
    command: String,
    #[serde(default)]
    preprocessor: Option<String>,
    #[serde(default)]
    compare: Vec<String>,
}

// ---------------------------------------------------------------------------
// Catalog
// ---------------------------------------------------------------------------

/// The merged set of fleet checks, plus everything that failed to load.
#[derive(Debug, Clone, Default)]
pub struct FleetCheckCatalog {
    checks: Vec<FleetCheck>,
    rejected: Vec<RejectedCheck>,
}

impl FleetCheckCatalog {
    /// The built-in catalog alone, with no user file consulted.
    ///
    /// The embedded document is authored in-tree and covered by tests, so in
    /// a released binary [`rejected`](Self::rejected) is empty; it is still
    /// reported rather than asserted, because a panic on startup is a worse
    /// answer than a catalog that is short one check.
    pub fn builtin() -> Self {
        let mut catalog = Self::default();
        catalog.extend_from_toml(BUILTIN_CATALOG_TOML, CheckSource::Builtin);
        catalog
    }

    /// The built-in catalog augmented with a user file.
    ///
    /// `user_path` of `None`, or a path with no file at it, yields the
    /// built-ins unchanged — an absent user catalog is the normal case, not
    /// an error. A file that exists but cannot be read or parsed is recorded
    /// in [`rejected`](Self::rejected) and changes nothing else.
    ///
    /// The open is attempted rather than guarded by `Path::exists`, which
    /// answers `false` for *any* failed metadata lookup — a catalog the
    /// process may not stat (say, `o-x` on a parent directory) would be
    /// indistinguishable from one that was never written, and the operator
    /// would get silence where the contract above promises a reason. Only
    /// [`NotFound`][std::io::ErrorKind::NotFound] means "no user catalog";
    /// every other error is reported.
    pub fn load(user_path: Option<&Path>) -> Self {
        let mut catalog = Self::builtin();
        let Some(path) = user_path else {
            return catalog;
        };
        let source = CheckSource::User(path.to_path_buf());
        match std::fs::read_to_string(path) {
            Ok(text) => catalog.extend_from_toml(&text, source),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => catalog.rejected.push(RejectedCheck {
                source,
                index: None,
                name: None,
                reason: format!("failed to read user catalog: {error}"),
            }),
        }
        catalog
    }

    /// [`load`](Self::load) against the path [`user_catalog_path`] resolves.
    pub fn load_default() -> Self {
        Self::load(user_catalog_path().as_deref())
    }

    /// Every accepted check: built-ins first, then user entries, each in
    /// file order.
    pub fn checks(&self) -> &[FleetCheck] {
        &self.checks
    }

    /// Entries and files that were refused, in the order they were met.
    pub fn rejected(&self) -> &[RejectedCheck] {
        &self.rejected
    }

    /// Look up a check by its exact name.
    pub fn get(&self, name: &str) -> Option<&FleetCheck> {
        self.checks.iter().find(|check| check.name == name)
    }

    /// Number of accepted checks.
    pub fn len(&self) -> usize {
        self.checks.len()
    }

    /// `true` when no check was accepted.
    pub fn is_empty(&self) -> bool {
        self.checks.is_empty()
    }

    /// Parse `text` and append whatever survives validation.
    ///
    /// A document that is not valid TOML is one rejection covering the file;
    /// a bad entry inside a valid document is one rejection covering that
    /// entry. Either way the checks already in the catalog are untouched.
    fn extend_from_toml(&mut self, text: &str, source: CheckSource) {
        let file: CatalogFile = match toml::from_str(text) {
            Ok(file) => file,
            Err(error) => {
                self.rejected.push(RejectedCheck {
                    source,
                    index: None,
                    name: None,
                    reason: format!("failed to parse catalog: {error}"),
                });
                return;
            }
        };

        for (index, table) in file.check.into_iter().enumerate() {
            // Keep the declared name for the error message even when the
            // rest of the entry is unusable.
            let declared_name = table
                .get("name")
                .and_then(toml::Value::as_str)
                .map(str::to_owned);

            let raw: RawCheck = match table.try_into() {
                Ok(raw) => raw,
                Err(error) => {
                    self.reject(source.clone(), index, declared_name, error.to_string());
                    continue;
                }
            };

            let check = match self.validate(raw, source.clone()) {
                Ok(check) => check,
                Err(reason) => {
                    self.reject(source.clone(), index, declared_name, reason);
                    continue;
                }
            };
            self.checks.push(check);
        }
    }

    fn reject(
        &mut self,
        source: CheckSource,
        index: usize,
        name: Option<String>,
        reason: String,
    ) {
        self.rejected.push(RejectedCheck {
            source,
            index: Some(index),
            name,
            reason,
        });
    }

    /// Turn one parsed entry into a [`FleetCheck`], or say why it cannot be.
    ///
    /// Takes `&self` because uniqueness is a property of the merged catalog,
    /// not of the entry: a name is only free if nothing loaded so far
    /// claimed it.
    fn validate(
        &self,
        raw: RawCheck,
        source: CheckSource,
    ) -> std::result::Result<FleetCheck, String> {
        let name = raw.name.trim();
        if name.is_empty() {
            return Err("name must not be empty".into());
        }
        if name.len() > MAX_NAME_LEN {
            return Err(format!(
                "name is {} bytes, the limit is {MAX_NAME_LEN}",
                name.len()
            ));
        }
        if !name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '_' | '.'))
        {
            return Err(format!(
                "name '{name}' must use only lowercase ASCII letters, digits, '-', '_' and '.'"
            ));
        }
        if let Some(existing) = self.get(name) {
            return Err(match existing.source() {
                CheckSource::Builtin => format!(
                    "name '{name}' is already used by a built-in check; \
                     user checks augment the built-ins, they do not replace them"
                ),
                CheckSource::User(path) => {
                    format!("name '{name}' is already used by a check from {}", path.display())
                }
            });
        }

        let description = raw.description.trim();
        if description.is_empty() {
            return Err("description must not be empty".into());
        }

        let command = raw.command.trim();
        if command.is_empty() {
            return Err("command must not be empty".into());
        }
        // A newline in a declared command would mean two commands, only the
        // first of which anything downstream reasons about.
        if command.chars().any(|c| c.is_control()) {
            return Err("command must be a single line without control characters".into());
        }

        let preprocessor = match raw.preprocessor.as_deref().map(str::trim) {
            None => None,
            Some("") => return Err("preprocessor must not be empty when set".into()),
            Some(name) => Some(name.to_owned()),
        };

        let mut compare = Vec::with_capacity(raw.compare.len());
        let mut seen = HashSet::new();
        for column in &raw.compare {
            let column = column.trim();
            if column.is_empty() {
                return Err("compare must not contain empty column names".into());
            }
            if !seen.insert(column) {
                return Err(format!("compare lists column '{column}' twice"));
            }
            compare.push(column.to_owned());
        }

        match (&preprocessor, compare.is_empty()) {
            // Declaring a preprocessor and then comparing nothing leaves the
            // parse with no consumer — almost certainly a half-written entry.
            (Some(name), true) => {
                return Err(format!(
                    "preprocessor '{name}' is set but compare is empty: \
                     a parsed check must name at least one column to compare"
                ))
            }
            // Without a preprocessor there are no columns to name, so a
            // column list here would be quietly ignored.
            (None, false) => {
                return Err(
                    "compare is set but no preprocessor is: a raw check compares \
                     its whole output and has no columns"
                        .into(),
                )
            }
            _ => {}
        }

        Ok(FleetCheck {
            name: name.to_owned(),
            description: description.to_owned(),
            command: command.to_owned(),
            preprocessor,
            compare,
            source,
        })
    }
}

// ---------------------------------------------------------------------------
// Path resolution
// ---------------------------------------------------------------------------

/// Where the user catalog is read from, if anywhere.
///
/// In order:
/// 1. [`USER_CATALOG_ENV`] (an explicit path, used as given);
/// 2. [`USER_CATALOG_FILE`] next to the effective `config.toml`
///    ([`crate::Config::default_path`]) — "next to the config" in the literal
///    sense, whichever of the four config locations won;
/// 3. `{OS data dir}/filar/`[`USER_CATALOG_FILE`], the directory the launcher
///    writes to, for the case where no config file exists at all.
///
/// The returned path need not exist; [`FleetCheckCatalog::load`] treats a
/// missing file as an empty user catalog.
pub fn user_catalog_path() -> Option<PathBuf> {
    if let Ok(explicit) = std::env::var(USER_CATALOG_ENV) {
        if !explicit.trim().is_empty() {
            return Some(PathBuf::from(explicit));
        }
    }
    if let Some(config) = crate::Config::default_path() {
        if let Some(dir) = config.parent() {
            return Some(dir.join(USER_CATALOG_FILE));
        }
    }
    let base = crate::default_base_dir().ok()?;
    Some(base.join("filar").join(USER_CATALOG_FILE))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Write `contents` to a uniquely named file and hand back its path plus
    /// a guard that removes the directory. Mirrors the pattern in
    /// `config.rs`: this workspace has no `tempfile` dev-dependency.
    struct TempCatalog {
        dir: PathBuf,
        path: PathBuf,
    }

    impl TempCatalog {
        fn new(label: &str, contents: &str) -> Self {
            let dir = std::env::temp_dir().join(format!(
                "filar_fleet_checks_{}_{label}_{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos()
            ));
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(USER_CATALOG_FILE);
            std::fs::write(&path, contents).unwrap();
            Self { dir, path }
        }
    }

    impl Drop for TempCatalog {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    // -- built-ins ---------------------------------------------------------

    #[test]
    fn builtin_catalog_loads_without_rejections() {
        let catalog = FleetCheckCatalog::builtin();
        assert!(
            catalog.rejected().is_empty(),
            "built-in catalog must be valid, got: {:?}",
            catalog.rejected()
        );
        assert!(!catalog.is_empty(), "built-in catalog must not be empty");
        assert!(catalog.checks().iter().all(FleetCheck::is_builtin));
    }

    #[test]
    fn builtin_catalog_has_unique_names_and_filled_fields() {
        let catalog = FleetCheckCatalog::builtin();
        let mut names = HashSet::new();
        for check in catalog.checks() {
            assert!(names.insert(check.name()), "duplicate name {}", check.name());
            assert!(!check.description().is_empty());
            assert!(!check.command().is_empty());
            // The invariant the schema exists to carry.
            assert_eq!(
                check.preprocessor().is_some(),
                !check.compare().is_empty(),
                "check '{}' must name compare columns iff it has a preprocessor",
                check.name()
            );
        }
    }

    #[test]
    fn builtin_catalog_covers_the_expected_checks() {
        let catalog = FleetCheckCatalog::builtin();
        for name in [
            "disk-usage",
            "block-devices",
            "processes",
            "listening-sockets",
            "kernel-version",
            "os-release",
        ] {
            assert!(catalog.get(name).is_some(), "missing built-in check '{name}'");
        }
        let disk = catalog.get("disk-usage").unwrap();
        assert_eq!(disk.preprocessor(), Some("df"));
        assert!(disk.compare().contains(&"mount".to_string()));
        assert_eq!(catalog.get("kernel-version").unwrap().preprocessor(), None);
    }

    // -- user catalog augments --------------------------------------------

    #[test]
    fn user_check_augments_the_builtins() {
        let file = TempCatalog::new(
            "augment",
            r#"
[[check]]
name = "sshd-config"
description = "Effective sshd configuration"
command = "cat /etc/ssh/sshd_config"
"#,
        );
        let builtin_len = FleetCheckCatalog::builtin().len();
        let catalog = FleetCheckCatalog::load(Some(&file.path));

        assert!(catalog.rejected().is_empty(), "{:?}", catalog.rejected());
        assert_eq!(catalog.len(), builtin_len + 1);
        // Built-ins survive, and the user entry is there beside them.
        assert!(catalog.get("disk-usage").unwrap().is_builtin());
        let user = catalog.get("sshd-config").expect("user check missing");
        assert!(!user.is_builtin());
        assert_eq!(user.source(), &CheckSource::User(file.path.clone()));
        assert_eq!(user.command(), "cat /etc/ssh/sshd_config");
    }

    #[test]
    fn missing_user_file_is_not_an_error() {
        let catalog = FleetCheckCatalog::load(Some(Path::new("/nonexistent/fleet_checks.toml")));
        assert!(catalog.rejected().is_empty());
        assert_eq!(catalog.len(), FleetCheckCatalog::builtin().len());
    }

    #[test]
    fn no_user_path_yields_the_builtins() {
        let catalog = FleetCheckCatalog::load(None);
        assert_eq!(catalog.len(), FleetCheckCatalog::builtin().len());
    }

    /// An unreadable catalog must be *reported*, not mistaken for an absent
    /// one. Reading a directory fails with a non-`NotFound` error on every
    /// supported platform (`IsADirectory` on Unix, `PermissionDenied` on
    /// Windows), which is the portable half of the contract.
    #[test]
    fn an_unreadable_user_catalog_is_reported() {
        let dir = std::env::temp_dir().join(format!(
            "filar_fleet_unreadable_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Guard(PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(dir.clone());

        // The catalog "path" is the directory itself.
        let catalog = FleetCheckCatalog::load(Some(&dir));
        assert_eq!(catalog.len(), FleetCheckCatalog::builtin().len());
        assert_eq!(catalog.rejected().len(), 1);
        assert!(
            catalog.rejected()[0]
                .reason()
                .contains("failed to read user catalog"),
            "got: {}",
            catalog.rejected()[0].reason()
        );
    }

    /// The case `Path::exists` used to swallow: a path whose *parent*
    /// component is a regular file. `exists()` answers `false` (the stat
    /// fails with `ENOTDIR`), so the old guard treated a real I/O fault as
    /// "no user catalog" and logged nothing. Unix-only because the error a
    /// non-directory component produces is `ENOTDIR` there and `NotFound`
    /// on Windows, where the distinction this test makes does not exist.
    #[cfg(unix)]
    #[test]
    fn a_path_under_a_regular_file_is_reported_not_treated_as_absent() {
        let dir = std::env::temp_dir().join(format!(
            "filar_fleet_notadir_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        struct Guard(PathBuf);
        impl Drop for Guard {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _guard = Guard(dir.clone());

        let regular = dir.join("not-a-directory");
        std::fs::write(&regular, "x").unwrap();
        let path = regular.join(USER_CATALOG_FILE);
        assert!(!path.exists(), "precondition: exists() must answer false");

        let catalog = FleetCheckCatalog::load(Some(&path));
        assert_eq!(catalog.len(), FleetCheckCatalog::builtin().len());
        assert_eq!(
            catalog.rejected().len(),
            1,
            "an I/O fault must not be silently read as an absent catalog"
        );
    }

    /// The other half: a path with genuinely nothing at it stays silent.
    #[test]
    fn a_not_found_catalog_stays_silent() {
        let path = std::env::temp_dir().join(format!(
            "filar_fleet_absent_{}_{}/{USER_CATALOG_FILE}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let catalog = FleetCheckCatalog::load(Some(&path));
        assert!(catalog.rejected().is_empty(), "{:?}", catalog.rejected());
        assert_eq!(catalog.len(), FleetCheckCatalog::builtin().len());
    }

    // -- broken entries ----------------------------------------------------

    /// The DoD case: one bad entry is reported, the rest keep working.
    #[test]
    fn broken_entry_is_rejected_and_the_others_survive() {
        let file = TempCatalog::new(
            "broken",
            r#"
[[check]]
name = "good-before"
description = "First good check"
command = "uptime"

[[check]]
name = "no-command"
description = "Missing the command entirely"

[[check]]
name = "good-after"
description = "Second good check"
command = "whoami"
"#,
        );
        let catalog = FleetCheckCatalog::load(Some(&file.path));

        assert!(catalog.get("good-before").is_some());
        assert!(catalog.get("good-after").is_some());
        assert!(catalog.get("no-command").is_none());
        // Built-ins are untouched by a bad user entry.
        assert!(catalog.get("disk-usage").is_some());

        assert_eq!(catalog.rejected().len(), 1);
        let rejected = &catalog.rejected()[0];
        assert_eq!(rejected.name(), Some("no-command"));
        assert_eq!(rejected.index(), Some(1));
        assert!(
            rejected.reason().contains("command"),
            "reason should name the missing field, got: {}",
            rejected.reason()
        );
        // The rendered message must be usable as-is in the UI.
        let rendered = rejected.to_string();
        assert!(rendered.contains("check #2 ('no-command')"), "{rendered}");
        assert!(rendered.contains(&file.path.display().to_string()), "{rendered}");
    }

    #[test]
    fn unparseable_file_is_rejected_as_a_unit_and_builtins_survive() {
        let file = TempCatalog::new("garbage", "this is not = valid toml [[[");
        let catalog = FleetCheckCatalog::load(Some(&file.path));

        assert_eq!(catalog.len(), FleetCheckCatalog::builtin().len());
        assert_eq!(catalog.rejected().len(), 1);
        let rejected = &catalog.rejected()[0];
        assert_eq!(rejected.index(), None);
        assert!(rejected.reason().contains("failed to parse catalog"));
    }

    #[test]
    fn unknown_field_is_named_rather_than_ignored() {
        let file = TempCatalog::new(
            "typo",
            r#"
[[check]]
name = "typo-check"
description = "Has a misspelled key"
command = "uptime"
preprocesor = "df"
"#,
        );
        let catalog = FleetCheckCatalog::load(Some(&file.path));
        assert!(catalog.get("typo-check").is_none());
        assert_eq!(catalog.rejected().len(), 1);
        assert!(
            catalog.rejected()[0].reason().contains("preprocesor"),
            "reason should name the unknown key, got: {}",
            catalog.rejected()[0].reason()
        );
    }

    #[test]
    fn user_entry_may_not_reuse_a_builtin_name() {
        let file = TempCatalog::new(
            "shadow",
            r#"
[[check]]
name = "disk-usage"
description = "Shadowing attempt"
command = "df -h"
"#,
        );
        let catalog = FleetCheckCatalog::load(Some(&file.path));
        // The built-in is the one that stayed.
        assert!(catalog.get("disk-usage").unwrap().is_builtin());
        assert_eq!(catalog.rejected().len(), 1);
        assert!(catalog.rejected()[0].reason().contains("built-in"));
    }

    #[test]
    fn duplicate_names_within_the_user_file_are_rejected_once() {
        let file = TempCatalog::new(
            "dupe",
            r#"
[[check]]
name = "mine"
description = "First"
command = "uptime"

[[check]]
name = "mine"
description = "Second"
command = "whoami"
"#,
        );
        let catalog = FleetCheckCatalog::load(Some(&file.path));
        assert_eq!(catalog.get("mine").unwrap().description(), "First");
        assert_eq!(catalog.rejected().len(), 1);
        assert_eq!(catalog.rejected()[0].index(), Some(1));
    }

    #[test]
    fn empty_and_malformed_scalars_are_rejected() {
        let cases: [(&str, &str, &str); 5] = [
            (
                "empty-name",
                r#"name = "  "
description = "d"
command = "uptime""#,
                "name must not be empty",
            ),
            (
                "bad-charset",
                r#"name = "Disk Usage"
description = "d"
command = "uptime""#,
                "lowercase ASCII",
            ),
            (
                "empty-description",
                r#"name = "x"
description = ""
command = "uptime""#,
                "description must not be empty",
            ),
            (
                "empty-command",
                r#"name = "x"
description = "d"
command = "   ""#,
                "command must not be empty",
            ),
            (
                "newline-command",
                r#"name = "x"
description = "d"
command = "uptime\nwhoami""#,
                "single line",
            ),
        ];

        for (label, body, expected) in cases {
            let file = TempCatalog::new(label, &format!("[[check]]\n{body}\n"));
            let catalog = FleetCheckCatalog::load(Some(&file.path));
            assert_eq!(catalog.rejected().len(), 1, "{label}");
            assert!(
                catalog.rejected()[0].reason().contains(expected),
                "{label}: expected '{expected}', got '{}'",
                catalog.rejected()[0].reason()
            );
        }
    }

    #[test]
    fn preprocessor_and_compare_must_agree() {
        let parsed_without_columns = TempCatalog::new(
            "no-columns",
            r#"
[[check]]
name = "parsed"
description = "Parsed but compares nothing"
command = "df"
preprocessor = "df"
"#,
        );
        let catalog = FleetCheckCatalog::load(Some(&parsed_without_columns.path));
        assert_eq!(catalog.rejected().len(), 1);
        assert!(catalog.rejected()[0]
            .reason()
            .contains("at least one column"));

        let raw_with_columns = TempCatalog::new(
            "raw-columns",
            r#"
[[check]]
name = "raw"
description = "Raw but names columns"
command = "uptime"
compare = ["whatever"]
"#,
        );
        let catalog = FleetCheckCatalog::load(Some(&raw_with_columns.path));
        assert_eq!(catalog.rejected().len(), 1);
        assert!(catalog.rejected()[0]
            .reason()
            .contains("no preprocessor is"));
    }

    #[test]
    fn duplicate_compare_columns_are_rejected() {
        let file = TempCatalog::new(
            "dupe-columns",
            r#"
[[check]]
name = "twice"
description = "Names one column twice"
command = "df"
preprocessor = "df"
compare = ["mount", "mount"]
"#,
        );
        let catalog = FleetCheckCatalog::load(Some(&file.path));
        assert_eq!(catalog.rejected().len(), 1);
        assert!(catalog.rejected()[0].reason().contains("twice"));
    }

    #[test]
    fn empty_user_catalog_adds_nothing_and_rejects_nothing() {
        let file = TempCatalog::new("empty", "# no checks here\n");
        let catalog = FleetCheckCatalog::load(Some(&file.path));
        assert_eq!(catalog.len(), FleetCheckCatalog::builtin().len());
        assert!(catalog.rejected().is_empty());
    }

    /// A check may declare a command the read-only gate will refuse — the
    /// catalog is a declaration, not a permission. The refusal happens in
    /// `filar-transport`, which this crate cannot reach; the matching test
    /// lives in `crates/transport/tests/fleet_catalog_readonly.rs`.
    #[test]
    fn a_dangerous_command_is_accepted_by_the_catalog_itself() {
        let file = TempCatalog::new(
            "dangerous",
            r#"
[[check]]
name = "dangerous"
description = "Declares a write the transport will refuse"
command = "rm -rf /var/log"
"#,
        );
        let catalog = FleetCheckCatalog::load(Some(&file.path));
        assert!(catalog.rejected().is_empty());
        assert_eq!(catalog.get("dangerous").unwrap().command(), "rm -rf /var/log");
    }
}
