//! Output preprocessors: machine-format command output becomes a typed table
//! (issue #420).
//!
//! **Why.** Twelve fleet hosts × output truncated at 10 000 chars ≈ tens of
//! thousands of tokens for one step — the context dies on the model's second
//! question. The fleet design (decisions 34/36) is that the *code* compares
//! results and the model receives one condensed difference table, not twelve
//! raw dumps. A preprocessor is the per-command half of that: it knows how to
//! read, say, `df` output and returns typed rows and fields instead of a wall
//! of text. Single-host mode benefits on its own — the model stops paying
//! tokens to parse tables that already have a machine format.
//!
//! **Contract.** A preprocessor is a *pure function of the output it is
//! given*: no network, no filesystem, no environment, no logging. The same
//! input always yields the same verdict. This is not a style preference: the
//! output may contain anything the host printed, and a preprocessor runs on
//! every command, so any side effect would be both a security and a
//! reproducibility problem. [`OutputPreprocessor::matches`] decides *whether*
//! the command is yours; [`OutputPreprocessor::preprocess`] parses the
//! output. A parse failure is not an error of the caller: it means "no typed
//! table this time" and [`PreprocessorRegistry::preprocess`] answers with
//! [`PreprocessOutcome::Raw`] — the raw text keeps flowing to the model.
//!
//! **Fail closed.** A wrong table is worse than raw text: a silently misread
//! column would have the model reason about numbers that mean something
//! else. Every check here therefore refuses rather than guesses — an
//! unexpected header, a short line (the fleet truncation cuts mid-line), a
//! non-GNU flavour all degrade to raw. Truncated or garbage output must
//! never panic and never invent structure.
//!
//! **Overlap.** Preprocessors are consulted in registration order. The first
//! one that *claims* a command ([`matches`][OutputPreprocessor::matches])
//! and *parses* its output wins; a claimant that fails is set aside and the
//! next claimant is asked (a strict built-in must not hide a more tolerant
//! custom one). Only when every claimant fails does the registry return raw,
//! carrying the last error. Built-ins are registered first
//! ([`PreprocessorRegistry::with_builtins`]), so a custom preprocessor for a
//! command the built-ins already handle is consulted only when the built-in
//! declines or fails; build with [`PreprocessorRegistry::new`] to control
//! the set explicitly.
//!
//! **Scope.** This module is the framework — the trait, the registry, the
//! fallback — plus the command families built on top of it: disk and block
//! devices (`df`, `lsblk`, issue #421), packages and application versions
//! (`dpkg-query`, `rpm`, `nginx -v`, `php -v`, issue #422), and services,
//! processes, sockets, the journal and network addresses (`systemctl`, `ps`,
//! `ss`, `journalctl`, `ip`, issue #423). The agent-loop / fleet integration
//! comes in later issues.

use std::fmt;

/// A typed table: column names plus rows with exactly one field per column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreprocessedOutput {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

impl PreprocessedOutput {
    /// Build a table, validating its shape.
    ///
    /// A table must have at least one column and every row must have exactly
    /// one field per column. A ragged table is a parser bug, not data, and is
    /// rejected here rather than surfacing as an index panic downstream.
    pub fn new(columns: Vec<String>, rows: Vec<Vec<String>>) -> Result<Self, PreprocessError> {
        if columns.is_empty() {
            return Err(PreprocessError::new("a table needs at least one column"));
        }
        if let Some(row) = rows.iter().find(|row| row.len() != columns.len()) {
            return Err(PreprocessError::new(format!(
                "ragged table: a row has {} field(s), expected {} (one per column)",
                row.len(),
                columns.len()
            )));
        }
        Ok(Self { columns, rows })
    }

    /// Column names, in output order.
    pub fn columns(&self) -> &[String] {
        &self.columns
    }

    /// Table rows; each row has one field per column.
    pub fn rows(&self) -> &[Vec<String>] {
        &self.rows
    }

    /// Number of data rows (not counting the header).
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    /// Returns `true` if the table has no data rows.
    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    /// Position of a column by name, if present.
    pub fn column_index(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|column| column == name)
    }
}

/// Why a preprocessor could not produce a typed table.
///
/// Carries the reason for the degradation. Raw output is not an error path of
/// the caller: the outcome of a failed parse is
/// [`PreprocessOutcome::Raw`], and the text the model sees stays the same.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreprocessError {
    reason: String,
}

impl PreprocessError {
    /// Create an error with a human-readable reason.
    pub fn new(reason: impl Into<String>) -> Self {
        Self {
            reason: reason.into(),
        }
    }

    /// The reason text.
    pub fn reason(&self) -> &str {
        &self.reason
    }
}

impl fmt::Display for PreprocessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.reason)
    }
}

impl std::error::Error for PreprocessError {}

/// Why the registry fell back to raw text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RawFallback {
    /// No registered preprocessor claimed the command.
    NoPreprocessor,
    /// Every claimant failed to parse this output.
    Unparseable(PreprocessError),
}

/// What [`PreprocessorRegistry::preprocess`] produced for one command output.
#[must_use]
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PreprocessOutcome {
    /// A preprocessor claimed the command and parsed the output.
    Structured {
        /// Name of the preprocessor that produced the table.
        preprocessor: &'static str,
        /// The typed table.
        table: PreprocessedOutput,
    },
    /// No table this time — use the raw output as before.
    Raw {
        /// Why the fallback happened.
        reason: RawFallback,
    },
}

impl PreprocessOutcome {
    /// Returns `true` if a typed table was produced.
    pub fn is_structured(&self) -> bool {
        matches!(self, Self::Structured { .. })
    }
}

/// Reads the output of one command family into a typed table.
///
/// Implementations must be **pure functions of their arguments** — no
/// network, no filesystem, no environment, no logging, no hidden state that
/// would make repeated calls differ. Both methods must be safe on arbitrary
/// input: a truncated, empty or garbage output is an ordinary case
/// ([`preprocess`][Self::preprocess] returns [`PreprocessError`]), never a
/// panic.
pub trait OutputPreprocessor: Send + Sync {
    /// Stable identifier, used in [`PreprocessOutcome::Structured`] and logs.
    fn name(&self) -> &'static str;

    /// Returns `true` if this preprocessor owns `command`.
    ///
    /// Must be cheap and side-effect free: it is consulted for every command,
    /// and `false` is the normal answer. A command whose output cannot be
    /// described faithfully (say, `df -i` with its inode columns) must not be
    /// claimed.
    fn matches(&self, command: &str) -> bool;

    /// Parse `output` of `command` into a typed table.
    ///
    /// Only called for commands [`matches`][Self::matches] accepted. Returning
    /// `Err` degrades to raw text and is the correct answer for anything that
    /// does not look exactly like the expected format — never guess.
    fn preprocess(
        &self,
        command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError>;
}

/// Holds the preprocessors consulted for command output, in priority order.
pub struct PreprocessorRegistry {
    preprocessors: Vec<Box<dyn OutputPreprocessor>>,
}

impl PreprocessorRegistry {
    /// Create an empty registry — every command falls back to raw text.
    pub fn new() -> Self {
        Self {
            preprocessors: Vec::new(),
        }
    }

    /// Create a registry with the built-in preprocessor set.
    pub fn with_builtins() -> Self {
        let mut registry = Self::new();
        registry.register(Box::new(DfPreprocessor));
        registry.register(Box::new(LsblkPreprocessor));
        registry.register(Box::new(DpkgPreprocessor));
        registry.register(Box::new(RpmPreprocessor));
        registry.register(Box::new(VersionPreprocessor));
        registry.register(Box::new(SystemctlPreprocessor));
        registry.register(Box::new(PsPreprocessor));
        registry.register(Box::new(SsPreprocessor));
        registry.register(Box::new(JournalctlPreprocessor));
        registry.register(Box::new(IpPreprocessor));
        registry
    }

    /// Append a preprocessor; later registrations have lower priority.
    pub fn register(&mut self, preprocessor: Box<dyn OutputPreprocessor>) -> &mut Self {
        self.preprocessors.push(preprocessor);
        self
    }

    /// Number of registered preprocessors.
    pub fn len(&self) -> usize {
        self.preprocessors.len()
    }

    /// Returns `true` if no preprocessor is registered.
    pub fn is_empty(&self) -> bool {
        self.preprocessors.is_empty()
    }

    /// Run `command`'s `output` through the registered preprocessors.
    ///
    /// Never fails: every path ends in a [`PreprocessOutcome`]. In
    /// registration order, the first preprocessor that claims the command and
    /// parses the output wins; a claimant that fails is set aside and the
    /// next claimant is asked. When only failures remain, the outcome is
    /// [`RawFallback::Unparseable`] with the last error; with no claimant at
    /// all it is [`RawFallback::NoPreprocessor`].
    pub fn preprocess(&self, command: &str, output: &str) -> PreprocessOutcome {
        let mut last_error: Option<PreprocessError> = None;
        for preprocessor in &self.preprocessors {
            if !preprocessor.matches(command) {
                continue;
            }
            match preprocessor.preprocess(command, output) {
                Ok(table) => {
                    return PreprocessOutcome::Structured {
                        preprocessor: preprocessor.name(),
                        table,
                    };
                }
                Err(error) => last_error = Some(error),
            }
        }
        match last_error {
            Some(error) => PreprocessOutcome::Raw {
                reason: RawFallback::Unparseable(error),
            },
            None => PreprocessOutcome::Raw {
                reason: RawFallback::NoPreprocessor,
            },
        }
    }
}

impl Default for PreprocessorRegistry {
    /// Same as [`with_builtins`][Self::with_builtins].
    fn default() -> Self {
        Self::with_builtins()
    }
}

impl fmt::Debug for PreprocessorRegistry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PreprocessorRegistry")
            .field(
                "preprocessors",
                &self
                    .preprocessors
                    .iter()
                    .map(|preprocessor| preprocessor.name())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

/// Columns produced by [`DfPreprocessor`].
const DF_COLUMNS: [&str; 6] = ["filesystem", "size", "used", "avail", "use_percent", "mount"];

/// Characters whose presence means the command is more than a simple
/// invocation: compound separators, pipelines, redirection, substitution.
const SHELL_META: &[char] = &[
    ';', '|', '&', '<', '>', '`', '$', '(', ')', '"', '\'', '\n', '\r',
];

/// The one `--output` field list [`DfPreprocessor`] also accepts (issue
/// #421): the same six fields, in the same order, that its positional parse
/// already produces. Requesting exactly this list gets a caller a
/// guaranteed column set — immune to whatever extra columns a given `df`
/// build defaults to — while still parsing with the existing positional
/// logic, since the header and field shape it produces are byte-for-byte
/// the plain, un-flagged `df` output. Any other field list still changes
/// the columns and is declined, as before.
const DF_CANONICAL_OUTPUT: &str = "source,size,used,avail,pcent,target";

/// Reference preprocessor for GNU coreutils `df`.
///
/// Claims `df` only as a single simple command — a pipeline, a compound
/// segment or a redirect has output that is not a pure `df` table. Options
/// that change the column set are not claimed either: inode mode
/// (`-i`/`--inodes`, and `i` in a short cluster), type mode
/// (`-T`/`--print-type`) and custom field lists (`--output`) — long options
/// under any unambiguous abbreviation as well (`--ino`, `--out=…`) — with
/// one exception: `--output=`[`DF_CANONICAL_OUTPUT`] requests the same
/// six columns this preprocessor already knows how to read, so it is
/// claimed rather than declined (see [`DF_CANONICAL_OUTPUT`]).
/// Everything else — filters (`-x`, `-t`, `-l`, `-a`), display sizes (`-h`,
/// `-H`, `-k`, `-B`), `-P` — keeps the six-column shape and is accepted.
///
/// The parse is strict by design: the header must start with `Filesystem`,
/// every data line must have at least six whitespace-separated fields with
/// the use-percent field ending in `%` (GNU prints `-` for filesystems that
/// have no size) and the mount field starting with `/`. The mount point is
/// the last field group ([`fields[5..]`] joined), so mount points with spaces
/// survive. Any mismatch — truncation cuts mid-line, another locale or
/// `df` flavour (e.g. busybox on Alpine rejecting `--output` outright) —
/// returns an error and the caller degrades to raw text.
pub struct DfPreprocessor;

impl OutputPreprocessor for DfPreprocessor {
    fn name(&self) -> &'static str {
        "df"
    }

    fn matches(&self, command: &str) -> bool {
        if command.contains(SHELL_META) {
            return false;
        }
        let mut tokens = command.split_whitespace();
        let Some(program) = tokens.next() else {
            return false;
        };
        if base_name(program) != "df" {
            return false;
        }
        !tokens.any(option_changes_format)
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        let mut lines = output.lines().filter(|line| !line.trim().is_empty());
        let Some(header) = lines.next() else {
            return Err(PreprocessError::new("output is empty"));
        };
        if !header.trim_start().starts_with("Filesystem") {
            return Err(PreprocessError::new(format!(
                "first non-empty line is not a df header: {}",
                preview(header)
            )));
        }
        let mut rows = Vec::new();
        for (index, line) in lines.enumerate() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < DF_COLUMNS.len() {
                return Err(PreprocessError::new(format!(
                    "data line {} has {} field(s), fewer than the {} a df row has (truncated output?)",
                    index + 1,
                    fields.len(),
                    DF_COLUMNS.len()
                )));
            }
            let use_percent = fields[4];
            if use_percent != "-" && !use_percent.ends_with('%') {
                return Err(PreprocessError::new(format!(
                    "data line {} has no use percentage in the fifth field: {}",
                    index + 1,
                    preview(line)
                )));
            }
            if !fields[5].starts_with('/') {
                return Err(PreprocessError::new(format!(
                    "data line {} has a mount point not starting with `/`: {}",
                    index + 1,
                    preview(line)
                )));
            }
            rows.push(vec![
                fields[0].to_string(),
                fields[1].to_string(),
                fields[2].to_string(),
                fields[3].to_string(),
                use_percent.trim_end_matches('%').to_string(),
                fields[5..].join(" "),
            ]);
        }
        if rows.is_empty() {
            return Err(PreprocessError::new(
                "df printed a header but no data rows (truncated output?)",
            ));
        }
        let columns = DF_COLUMNS.iter().map(|column| column.to_string()).collect();
        PreprocessedOutput::new(columns, rows)
    }
}

/// Last path component of a program word (`/bin/df` → `df`).
fn base_name(program: &str) -> &str {
    program.rsplit('/').next().unwrap_or(program)
}

/// Does this option word change `df`'s column set (inodes, types, custom
/// field lists)?
///
/// Long options are matched by prefix: GNU getopt accepts any unambiguous
/// abbreviation, so `--ino` means `--inodes` and `--out=…` means
/// `--output`. An exact-name check would let an inode or custom-field table
/// through under the six-column labels — the percentage and mount fields
/// line up the same. Every non-empty prefix of `inodes`/`print-type` is
/// refused outright: a prefix they answer to is theirs (getopt requires
/// uniqueness to accept it), and the only name they share with a harmless
/// option (`--p`: portability vs print-type) makes `df` itself refuse the
/// input, so refusing costs nothing valid. A prefix of `output` is refused
/// too, *unless* its value is exactly [`DF_CANONICAL_OUTPUT`] — that one
/// field list keeps the six-column shape this preprocessor parses, so it is
/// let through instead of declined.
fn option_changes_format(token: &str) -> bool {
    if let Some(long) = token.strip_prefix("--") {
        let mut parts = long.splitn(2, '=');
        let name = parts.next().unwrap_or("");
        let value = parts.next();
        if !name.is_empty() && "output".starts_with(name) {
            return value != Some(DF_CANONICAL_OUTPUT);
        }
        return !name.is_empty()
            && ["inodes", "print-type"]
                .iter()
                .any(|option| option.starts_with(name));
    }
    if token.starts_with('-') && token.len() > 1 {
        // Short options cluster: `-i`, `-hT`, ... The letters are distinct.
        return token[1..].chars().any(|c| c == 'i' || c == 'T');
    }
    false
}

/// Short single-line preview of a source line for error messages.
fn preview(line: &str) -> String {
    const MAX: usize = 60;
    let trimmed = line.trim();
    if trimmed.chars().count() <= MAX {
        return format!("`{trimmed}`");
    }
    let head: String = trimmed.chars().take(MAX).collect();
    format!("`{head}…`")
}

/// Columns produced by [`LsblkPreprocessor`].
const LSBLK_COLUMNS: [&str; 4] = ["name", "size", "type", "mountpoint"];

/// Reference preprocessor for `lsblk --json` (util-linux), issue #421.
///
/// Unlike `df`'s plain-text table, `--json` output is self-describing: each
/// object carries its own field names, so there is no column-order or
/// locale ambiguity to guard against. The two things that do vary across
/// util-linux releases are handled explicitly: older versions report a
/// single `mountpoint` string (or `null`), newer ones a `mountpoints` array
/// (`null` entries included, for a device with no mount) to support
/// multiple mount points on one device (e.g. bind mounts, btrfs
/// subvolumes); this preprocessor reads either key and joins multiple
/// mount points with `, `. Partitions nested under a whole-disk entry
/// (`children`) are flattened into their own rows, recursively, so a
/// `sda` + `sda1` + `sda2` layout becomes three table rows.
///
/// Claims `lsblk` only as a single simple command carrying `--json` or
/// `-J`; a pipeline, compound segment or redirect is not a pure JSON
/// document, and a custom `-o`/`--output` field list is not claimed either,
/// since it is not known which fields such a list would include. Busybox
/// `lsblk` (Alpine) does not understand `--json` at all: it is still
/// claimed syntactically, but its output is not valid JSON, so parsing
/// fails and the caller degrades to raw text rather than panicking.
pub struct LsblkPreprocessor;

impl OutputPreprocessor for LsblkPreprocessor {
    fn name(&self) -> &'static str {
        "lsblk"
    }

    fn matches(&self, command: &str) -> bool {
        if command.contains(SHELL_META) {
            return false;
        }
        let mut tokens = command.split_whitespace();
        let Some(program) = tokens.next() else {
            return false;
        };
        if base_name(program) != "lsblk" {
            return false;
        }
        let tokens: Vec<&str> = tokens.collect();
        let has_json = tokens
            .iter()
            .any(|token| *token == "--json" || *token == "-J");
        has_json && !tokens.iter().any(|token| lsblk_option_changes_format(token))
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        let trimmed = output.trim();
        if trimmed.is_empty() {
            return Err(PreprocessError::new("output is empty"));
        }
        let root: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|error| PreprocessError::new(format!("not valid JSON: {error}")))?;
        let devices = root
            .get("blockdevices")
            .and_then(|value| value.as_array())
            .ok_or_else(|| PreprocessError::new("missing a `blockdevices` array"))?;
        let mut rows = Vec::new();
        for device in devices {
            collect_lsblk_rows(device, &mut rows)?;
        }
        if rows.is_empty() {
            return Err(PreprocessError::new("lsblk reported no block devices"));
        }
        let columns = LSBLK_COLUMNS.iter().map(|column| column.to_string()).collect();
        PreprocessedOutput::new(columns, rows)
    }
}

/// Does this option word change `lsblk`'s field set (`-o`/`--output`, any
/// unambiguous abbreviation of the latter, attached or clustered)?
fn lsblk_option_changes_format(token: &str) -> bool {
    if let Some(long) = token.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or(long);
        return !name.is_empty() && "output".starts_with(name);
    }
    if let Some(short) = token.strip_prefix('-') {
        // `-o` takes a value, attached or not (`-oNAME`, `-o NAME`); `o`
        // never means anything else in a short cluster.
        return short.contains('o');
    }
    false
}

/// Read one `lsblk --json` block-device object into a row, recursing into
/// `children` (partitions under a whole disk) depth-first.
fn collect_lsblk_rows(
    device: &serde_json::Value,
    rows: &mut Vec<Vec<String>>,
) -> Result<(), PreprocessError> {
    let name = device
        .get("name")
        .and_then(|value| value.as_str())
        .ok_or_else(|| PreprocessError::new("a block device is missing `name`"))?;
    let size = lsblk_scalar(device.get("size")).ok_or_else(|| {
        PreprocessError::new(format!("block device `{name}` is missing `size`"))
    })?;
    let device_type = device
        .get("type")
        .and_then(|value| value.as_str())
        .ok_or_else(|| PreprocessError::new(format!("block device `{name}` is missing `type`")))?;
    let mountpoint = lsblk_mountpoint(device)
        .map_err(|error| PreprocessError::new(format!("block device `{name}`: {error}")))?;
    rows.push(vec![
        name.to_string(),
        size,
        device_type.to_string(),
        mountpoint,
    ]);
    if let Some(children_value) = device.get("children") {
        let Some(children) = children_value.as_array() else {
            return Err(PreprocessError::new(format!(
                "block device `{name}` has a `children` field that is not an array"
            )));
        };
        for child in children {
            collect_lsblk_rows(child, rows)?;
        }
    }
    Ok(())
}

/// One or more mount points for a device, joined with `, `; empty when the
/// device is not mounted or the field is absent. Reads the newer
/// `mountpoints` array first (falling back to the older singular
/// `mountpoint` string). A present field of the wrong shape — `mountpoints`
/// not an array, an entry that is neither a string nor `null`, or a
/// `mountpoint` that is neither a string nor `null` — is a parse error
/// rather than silently treated as absent: a table that quietly drops a
/// real mount point is worse than falling back to raw text.
fn lsblk_mountpoint(device: &serde_json::Value) -> Result<String, PreprocessError> {
    if let Some(value) = device.get("mountpoints") {
        let Some(list) = value.as_array() else {
            return Err(PreprocessError::new("`mountpoints` is not an array"));
        };
        let mut points = Vec::with_capacity(list.len());
        for entry in list {
            match entry {
                serde_json::Value::Null => {}
                serde_json::Value::String(text) => points.push(text.clone()),
                other => {
                    return Err(PreprocessError::new(format!(
                        "a `mountpoints` entry is neither a string nor null: {other}"
                    )));
                }
            }
        }
        return Ok(points.join(", "));
    }
    match device.get("mountpoint") {
        None | Some(serde_json::Value::Null) => Ok(String::new()),
        Some(serde_json::Value::String(text)) => Ok(text.clone()),
        Some(other) => Err(PreprocessError::new(format!(
            "`mountpoint` is neither a string nor null: {other}"
        ))),
    }
}

/// A JSON string or number rendered as a plain string (`size` is a string by
/// default, a number under `--bytes`); anything else (missing, `null`,
/// object, array) is not a scalar `lsblk` would have printed.
fn lsblk_scalar(value: Option<&serde_json::Value>) -> Option<String> {
    match value {
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(serde_json::Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

/// Columns produced by [`DpkgPreprocessor`] and [`RpmPreprocessor`] — the
/// installed-package inventory both package managers answer with.
const PACKAGE_COLUMNS: [&str; 2] = ["package", "version"];

/// The argument list [`DpkgPreprocessor`] claims, after the program word
/// (issue #422).
///
/// `dpkg-query`'s `-f` template language *is* the machine format here, so
/// this preprocessor pins one template instead of trying to read an
/// arbitrary one: `${Package}` and `${Version}`, tab-separated, one package
/// per line. The single quotes are part of the contract rather than
/// decoration — unquoted (or double-quoted) `${Package}` is expanded by the
/// shell before `dpkg-query` ever sees it and the template arrives empty.
/// `\t` and `\n` are `dpkg-query`'s own escapes, so in the command text they
/// travel as the two-character sequences they look like.
const DPKG_CANONICAL_ARGS: &str = r"-W -f='${Package}\t${Version}\n'";

/// The argument list [`RpmPreprocessor`] claims, after the program word
/// (issue #422).
///
/// Same reasoning as [`DPKG_CANONICAL_ARGS`]: one pinned `--qf` template,
/// tab-separated, one package per line.
///
/// The version field is the package's full identity, `EPOCH:VERSION-RELEASE`,
/// because every component of it distinguishes real builds. The release is
/// where the distribution's own patch level lives (`1.20.1-14.el9`), and the
/// epoch is the component RPM compares *first* — it exists precisely to
/// order releases whose version numbering changed, so `1:2.0-3` and
/// `2:2.0-3` are different packages that a bare `VERSION-RELEASE` would
/// report as equal across a fleet. `%{EPOCHNUM}` rather than `%{EPOCH}`
/// because it renders an unset epoch as `0` instead of the literal
/// `(none)`, which keeps every row comparable. (`dpkg`'s `${Version}`
/// already includes the epoch when a package has one, so only rpm needs
/// this spelled out.)
const RPM_CANONICAL_ARGS: &str = r"-qa --qf='%{NAME}\t%{EPOCHNUM}:%{VERSION}-%{RELEASE}\n'";

/// Reads `dpkg-query`'s pinned template into `package`/`version` rows.
///
/// Claims exactly one invocation — `dpkg-query` (optionally as a path) with
/// [`DPKG_CANONICAL_ARGS`] and nothing else. Exact matching replaces the
/// shell-metacharacter scan the other preprocessors run: the template itself
/// necessarily contains `$` and quotes, so scanning for those would reject
/// every valid invocation, while demanding equality with one known-safe
/// literal leaves a pipeline or a redirect nowhere to hide (an appended
/// `| head` is simply an extra word, and declined).
///
/// The parse is fail-closed like the rest of the module: every non-empty
/// line must be exactly two tab-separated fields, both non-empty. A line cut
/// mid-way by the fleet's output truncation therefore degrades to raw text
/// rather than entering the table as a package with no version.
pub struct DpkgPreprocessor;

impl OutputPreprocessor for DpkgPreprocessor {
    fn name(&self) -> &'static str {
        "dpkg-query"
    }

    fn matches(&self, command: &str) -> bool {
        is_canonical_invocation(command, "dpkg-query", DPKG_CANONICAL_ARGS)
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        parse_package_table(output)
    }
}

/// Reads `rpm -qa`'s pinned template into `package`/`version` rows.
///
/// The `dpkg-query` counterpart: claims exactly `rpm` (optionally as a path)
/// with [`RPM_CANONICAL_ARGS`], and shares the same fail-closed
/// two-field-per-line parse. See [`DpkgPreprocessor`] for why the match is
/// an exact one.
pub struct RpmPreprocessor;

impl OutputPreprocessor for RpmPreprocessor {
    fn name(&self) -> &'static str {
        "rpm"
    }

    fn matches(&self, command: &str) -> bool {
        is_canonical_invocation(command, "rpm", RPM_CANONICAL_ARGS)
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        parse_package_table(output)
    }
}

/// Does `command` invoke `program` with exactly `args`?
///
/// The program may be written as a path (`/usr/bin/rpm`), and runs of
/// whitespace between words are normalised on both sides; anything else —
/// an extra argument, a missing one, a trailing pipeline — is not this
/// invocation.
///
/// The program word is additionally held to the same no-shell-syntax rule
/// the other preprocessors apply to the whole command: `base_name` alone
/// would read `$(pwd)/dpkg-query` as `dpkg-query`, and a substitution in
/// the program position is neither the literal nor the path-qualified
/// invocation this matcher stands for — what it would actually run is
/// decided by the shell, not visible here. Only the pinned argument
/// template is exempt, since it necessarily contains `$` and quotes.
fn is_canonical_invocation(command: &str, program: &str, args: &str) -> bool {
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if first.contains(SHELL_META) || base_name(first) != program {
        return false;
    }
    words.eq(args.split_whitespace())
}

/// Parse tab-separated `name<TAB>version` lines into a package table.
///
/// Shared by [`DpkgPreprocessor`] and [`RpmPreprocessor`], whose pinned
/// templates produce the same two-field shape.
fn parse_package_table(output: &str) -> Result<PreprocessedOutput, PreprocessError> {
    let mut rows = Vec::new();
    for (index, line) in output
        .lines()
        .filter(|line| !line.trim().is_empty())
        .enumerate()
    {
        let mut fields = line.split('\t');
        let package = fields.next().unwrap_or("").trim();
        let version = fields.next().unwrap_or("").trim();
        if fields.next().is_some() {
            return Err(PreprocessError::new(format!(
                "line {} has more than the two tab-separated fields the template prints: {}",
                index + 1,
                preview(line)
            )));
        }
        if package.is_empty() || version.is_empty() {
            return Err(PreprocessError::new(format!(
                "line {} is not a `name<TAB>version` pair (truncated output?): {}",
                index + 1,
                preview(line)
            )));
        }
        rows.push(vec![package.to_string(), version.to_string()]);
    }
    if rows.is_empty() {
        return Err(PreprocessError::new(
            "no package lines in the output (truncated or empty?)",
        ));
    }
    let columns = PACKAGE_COLUMNS
        .iter()
        .map(|column| column.to_string())
        .collect();
    PreprocessedOutput::new(columns, rows)
}

/// Columns produced by [`VersionPreprocessor`].
///
/// `raw` always carries the line the version was read from, so a version
/// this preprocessor could not parse is visible rather than lost: such a row
/// has an empty `version` and the original text in `raw`.
const VERSION_COLUMNS: [&str; 3] = ["program", "version", "raw"];

/// Programs whose `-v` output [`VersionPreprocessor`] knows how to read.
const VERSION_PROGRAMS: [&str; 2] = ["nginx", "php"];

/// Reads an application's `-v` banner into a `program`/`version`/`raw` row.
///
/// Application version banners have no machine format, but these have been
/// stable for years, so reading them by pattern is the honest solution here
/// rather than a workaround (issue #422). Claims `nginx -v` and `php -v`
/// only — as a single simple command, with no other arguments, since any
/// further flag changes what is printed (`nginx -V` adds the whole configure
/// line).
///
/// **Unparsed versions are reported, not dropped.** Unlike the rest of the
/// module, an unrecognised banner does *not* degrade to raw text: the row is
/// emitted with an empty `version` and the original line in `raw`. That is
/// not a guess — the distinction fail-closed protects against is a wrong
/// value presented as right, and an explicitly empty version is the
/// opposite. It matters for the fleet: when eleven hosts parse and one does
/// not, the difference table should show eleven versions and one unparsed
/// host, not lose that host's row or fall back to twelve raw dumps. A host
/// without the program at all lands here too, its `raw` carrying the shell's
/// own `command not found` — which is exactly the difference worth seeing.
/// Only genuinely empty output has nothing to report and degrades to raw.
pub struct VersionPreprocessor;

impl OutputPreprocessor for VersionPreprocessor {
    fn name(&self) -> &'static str {
        "app-version"
    }

    fn matches(&self, command: &str) -> bool {
        if command.contains(SHELL_META) {
            return false;
        }
        let mut words = command.split_whitespace();
        let Some(program) = words.next() else {
            return false;
        };
        if !VERSION_PROGRAMS.contains(&base_name(program)) {
            return false;
        }
        words.eq(["-v"])
    }

    fn preprocess(
        &self,
        command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        let program = command
            .split_whitespace()
            .next()
            .map(base_name)
            .unwrap_or_default();
        let Some(line) = output.lines().find(|line| !line.trim().is_empty()) else {
            return Err(PreprocessError::new("output is empty"));
        };
        let line = line.trim();
        let version = parse_version_banner(program, line).unwrap_or_default();
        let columns = VERSION_COLUMNS
            .iter()
            .map(|column| column.to_string())
            .collect();
        PreprocessedOutput::new(
            columns,
            vec![vec![program.to_string(), version.to_string(), line.to_string()]],
        )
    }
}

/// The version in one application's `-v` banner, or `None` when the line is
/// not the banner this program prints.
/// Each arm requires the banner's **full** prefix, not just the marker
/// inside it: an error message that merely mentions a path like
/// `/etc/nginx/1.24.0.bak` is not a version report, and reading one out of
/// it would be exactly the wrong-value-presented-as-right this module
/// refuses. Anything that is not the real banner returns `None` and is
/// reported as unparsed instead.
fn parse_version_banner<'a>(program: &str, line: &'a str) -> Option<&'a str> {
    match program {
        // `nginx version: nginx/1.24.0`, sometimes with a `(Ubuntu)` suffix.
        "nginx" => leading_version(line.strip_prefix("nginx version: nginx/")?),
        // `PHP 8.2.7 (cli) (built: ...) (NTS)`.
        "php" => leading_version(line.strip_prefix("PHP ")?.trim_start()),
        _ => None,
    }
}

/// The leading version number of `text` — the run of digits and dots it
/// starts with, so `1.24.0` from `1.24.0 (Ubuntu)` and `8.2.7` from
/// `8.2.7-1ubuntu2`. `None` unless `text` starts with a digit.
fn leading_version(text: &str) -> Option<&str> {
    let end = text
        .find(|c: char| !c.is_ascii_digit() && c != '.')
        .unwrap_or(text.len());
    let candidate = text[..end].trim_end_matches('.');
    if !candidate.starts_with(|c: char| c.is_ascii_digit()) {
        return None;
    }
    Some(candidate)
}

/// Columns produced by [`SystemctlPreprocessor`].
const SYSTEMCTL_COLUMNS: [&str; 5] = ["unit", "load", "active", "sub", "description"];

/// The argument list [`SystemctlPreprocessor`] claims (issue #423).
const SYSTEMCTL_CANONICAL_ARGS: &str = "list-units --output=json";

/// Reads `systemctl list-units --output=json` into unit rows.
///
/// systemd's own JSON output is the machine format, so the shape is fixed:
/// a top-level array of unit objects. Every row needs `unit`, `load`,
/// `active` and `sub`; `description` is optional (absent becomes empty)
/// because it is free text rather than state.
///
/// **A host without systemd degrades to raw, not to an error** (issue #423's
/// DoD). On Alpine/OpenRC `systemctl` is usually absent entirely, and on a
/// container it exists but refuses: the real output is
/// `System has not been booted with systemd as init system (PID 1). Can't
/// operate.` Neither is JSON, so the parse fails and the caller falls back
/// to the raw text — which is exactly the message a reader needs.
pub struct SystemctlPreprocessor;

impl OutputPreprocessor for SystemctlPreprocessor {
    fn name(&self) -> &'static str {
        "systemctl"
    }

    fn matches(&self, command: &str) -> bool {
        is_canonical_invocation(command, "systemctl", SYSTEMCTL_CANONICAL_ARGS)
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        let units = parse_json_array(output, "systemctl")?;
        let mut rows = Vec::new();
        for unit in &units {
            let name = json_string(unit.get("unit"))
                .ok_or_else(|| PreprocessError::new("a unit entry is missing `unit`"))?;
            let mut row = vec![name.clone()];
            for field in ["load", "active", "sub"] {
                let value = json_string(unit.get(field)).ok_or_else(|| {
                    PreprocessError::new(format!("unit `{name}` is missing `{field}`"))
                })?;
                row.push(value);
            }
            // Free text, not state: absent or null is an empty cell, but a
            // non-string value means this is not systemd's own output.
            let description = match unit.get("description") {
                None | Some(serde_json::Value::Null) => String::new(),
                Some(serde_json::Value::String(text)) => text.clone(),
                Some(other) => {
                    return Err(PreprocessError::new(format!(
                        "unit `{name}` has a non-string `description`: {other}"
                    )))
                }
            };
            row.push(description);
            rows.push(row);
        }
        table(&SYSTEMCTL_COLUMNS, rows, "systemctl listed no units")
    }
}

/// Columns produced by [`PsPreprocessor`].
const PS_COLUMNS: [&str; 6] = ["pid", "ppid", "user", "rss", "pcpu", "comm"];

/// The argument list [`PsPreprocessor`] claims (issue #423).
///
/// `ps` has no machine format, but `-o` with an explicit field list is the
/// next best thing: it pins both which columns appear and their order, so
/// the positional parse below is reading a layout this preprocessor chose
/// rather than guessing at a default that varies between builds and
/// `$COLUMNS` widths.
const PS_CANONICAL_ARGS: &str = "-eo pid,ppid,user,rss,pcpu,comm";

/// Reads `ps -eo pid,ppid,user,rss,pcpu,comm` into process rows.
///
/// The parse is fail-closed: the header must start with `PID`, every data
/// line needs at least six whitespace-separated fields, and `pid`, `ppid`
/// and `rss` must be all digits with `pcpu` a decimal number — a line the
/// fleet's truncation cut mid-way fails those checks instead of entering the
/// table with shifted columns. `comm` is the last field group
/// (`fields[5..]` joined), so a command name containing spaces survives.
pub struct PsPreprocessor;

impl OutputPreprocessor for PsPreprocessor {
    fn name(&self) -> &'static str {
        "ps"
    }

    fn matches(&self, command: &str) -> bool {
        is_canonical_invocation(command, "ps", PS_CANONICAL_ARGS)
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        let mut lines = output.lines().filter(|line| !line.trim().is_empty());
        let Some(header) = lines.next() else {
            return Err(PreprocessError::new("output is empty"));
        };
        if !header.trim_start().starts_with("PID") {
            return Err(PreprocessError::new(format!(
                "first non-empty line is not a ps header: {}",
                preview(header)
            )));
        }
        let mut rows = Vec::new();
        for (index, line) in lines.enumerate() {
            let fields: Vec<&str> = line.split_whitespace().collect();
            if fields.len() < PS_COLUMNS.len() {
                return Err(PreprocessError::new(format!(
                    "data line {} has {} field(s), fewer than the {} a ps row has (truncated output?)",
                    index + 1,
                    fields.len(),
                    PS_COLUMNS.len()
                )));
            }
            for (position, name) in [(0, "pid"), (1, "ppid"), (3, "rss")] {
                if !is_all_digits(fields[position]) {
                    return Err(PreprocessError::new(format!(
                        "data line {} has a non-numeric `{name}`: {}",
                        index + 1,
                        preview(line)
                    )));
                }
            }
            if !is_decimal(fields[4]) {
                return Err(PreprocessError::new(format!(
                    "data line {} has a non-numeric `pcpu`: {}",
                    index + 1,
                    preview(line)
                )));
            }
            rows.push(vec![
                fields[0].to_string(),
                fields[1].to_string(),
                fields[2].to_string(),
                fields[3].to_string(),
                fields[4].to_string(),
                fields[5..].join(" "),
            ]);
        }
        table(&PS_COLUMNS, rows, "ps printed a header but no process rows")
    }
}

/// Columns produced by [`SsPreprocessor`].
const SS_COLUMNS: [&str; 6] = ["netid", "state", "recv_q", "send_q", "local", "peer"];

/// The flags [`SsPreprocessor`] accepts.
///
/// `-H` and `-n` are required: `-H` drops the header so every line is a
/// socket, and `-n` keeps addresses and ports numeric so no resolver runs
/// and no name lookup varies the output. `-l` and `-a` are optional and
/// select *which* sockets are listed without changing a line's shape, so
/// they are accepted rather than declined — `ss` with neither lists only
/// non-listening sockets, and a fleet check that wants listening ports
/// needs one of them.
const SS_REQUIRED_FLAGS: [&str; 2] = ["-H", "-n"];

/// Flags accepted alongside [`SS_REQUIRED_FLAGS`] (see its docs).
const SS_OPTIONAL_FLAGS: [&str; 2] = ["-l", "-a"];

/// Reads `ss -H -n` into socket rows.
///
/// **Which sockets appear is the caller's choice, not this parser's.** Bare
/// `ss` lists only *non-listening* sockets, so `ss -H -n` alone answers
/// "what is connected", not "what is listening" — for the latter the caller
/// adds `-l` (listening only) or `-a` (both), which this preprocessor
/// therefore accepts: they change the selection, never a line's shape.
///
/// Field counts differ by address family, which real output confirms: an
/// internet socket prints six fields, because its address and port are
/// joined (`tcp ESTAB 0 0 192.0.2.2:44438 198.51.100.7:443`), while a unix
/// socket prints eight, address and "port" (its inode) being separate
/// (`u_str ESTAB 0 0 * 896 * 0`). Both are read; for the eight-field shape
/// the pairs are rejoined with `:` so the `local` and `peer` columns mean
/// the same thing in every row.
///
/// Anything else fails closed — and that is not hypothetical: real `ss`
/// output can carry a diagnostic line of its own (`RTNETLINK answers:
/// Invalid argument`) mixed in with the sockets. A line that is not a socket
/// row means the output is not purely socket rows, so the raw text is the
/// honest answer rather than a table quietly missing entries.
pub struct SsPreprocessor;

impl OutputPreprocessor for SsPreprocessor {
    fn name(&self) -> &'static str {
        "ss"
    }

    fn matches(&self, command: &str) -> bool {
        if command.contains(SHELL_META) {
            return false;
        }
        let mut words = command.split_whitespace();
        let Some(program) = words.next() else {
            return false;
        };
        if base_name(program) != "ss" {
            return false;
        }
        let flags: Vec<&str> = words.collect();
        if !flags
            .iter()
            .all(|flag| SS_REQUIRED_FLAGS.contains(flag) || SS_OPTIONAL_FLAGS.contains(flag))
        {
            return false;
        }
        SS_REQUIRED_FLAGS
            .iter()
            .all(|required| flags.contains(required))
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        let mut rows = Vec::new();
        for (index, line) in output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
        {
            let fields: Vec<&str> = line.split_whitespace().collect();
            let (local, peer) = match fields.len() {
                6 => (fields[4].to_string(), fields[5].to_string()),
                8 => (
                    format!("{}:{}", fields[4], fields[5]),
                    format!("{}:{}", fields[6], fields[7]),
                ),
                other => {
                    return Err(PreprocessError::new(format!(
                        "line {} has {other} field(s); a socket row has 6 (internet) or 8 (unix): {}",
                        index + 1,
                        preview(line)
                    )))
                }
            };
            if !is_all_digits(fields[2]) || !is_all_digits(fields[3]) {
                return Err(PreprocessError::new(format!(
                    "line {} has a non-numeric queue length: {}",
                    index + 1,
                    preview(line)
                )));
            }
            rows.push(vec![
                fields[0].to_string(),
                fields[1].to_string(),
                fields[2].to_string(),
                fields[3].to_string(),
                local,
                peer,
            ]);
        }
        table(&SS_COLUMNS, rows, "no socket rows in the output")
    }
}

/// Columns produced by [`JournalctlPreprocessor`].
const JOURNALCTL_COLUMNS: [&str; 4] = ["timestamp", "unit", "priority", "message"];

/// Reads `journalctl --output=json` into log rows.
///
/// journald's JSON output is **JSON Lines** — one object per line, not an
/// array — so it is parsed line by line. Of the many fields an entry can
/// carry, four are read: `__REALTIME_TIMESTAMP`, `_SYSTEMD_UNIT` (absent for
/// entries that did not come from a unit, which becomes an empty cell),
/// `PRIORITY` and `MESSAGE`.
///
/// `MESSAGE` must be a string. journald represents a non-UTF-8 message as an
/// array of byte values instead, and there is no faithful way to put that in
/// a text cell, so such output fails closed to raw rather than inventing a
/// rendering for it.
///
/// Claimed with `--output=json` or `-o json`, optionally bounded by
/// `-n`/`--lines` and `--no-pager`, which change how much is printed but not
/// the shape of a line. Any other flag is not claimed: `--output=` in
/// another mode is a different format, and filters that take free-text
/// values (`--since="2 hours ago"`) cannot be recognised by word.
pub struct JournalctlPreprocessor;

impl OutputPreprocessor for JournalctlPreprocessor {
    fn name(&self) -> &'static str {
        "journalctl"
    }

    fn matches(&self, command: &str) -> bool {
        if command.contains(SHELL_META) {
            return false;
        }
        let mut words = command.split_whitespace();
        let Some(program) = words.next() else {
            return false;
        };
        if base_name(program) != "journalctl" {
            return false;
        }
        let args: Vec<&str> = words.collect();
        let mut json = false;
        let mut index = 0;
        while index < args.len() {
            match args[index] {
                "--output=json" => json = true,
                "-o" | "--output" => {
                    if args.get(index + 1) != Some(&"json") {
                        return false;
                    }
                    json = true;
                    index += 1;
                }
                "--no-pager" => {}
                "-n" | "--lines" => match args.get(index + 1) {
                    Some(value) if is_all_digits(value) => index += 1,
                    _ => return false,
                },
                other => {
                    let bounded = other
                        .strip_prefix("--lines=")
                        .or_else(|| other.strip_prefix("-n"))
                        .is_some_and(is_all_digits);
                    if !bounded {
                        return false;
                    }
                }
            }
            index += 1;
        }
        json
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        let mut rows = Vec::new();
        for (index, line) in output
            .lines()
            .filter(|line| !line.trim().is_empty())
            .enumerate()
        {
            let entry: serde_json::Value = serde_json::from_str(line.trim()).map_err(|error| {
                PreprocessError::new(format!("line {} is not valid JSON: {error}", index + 1))
            })?;
            let timestamp = json_string(entry.get("__REALTIME_TIMESTAMP")).ok_or_else(|| {
                PreprocessError::new(format!(
                    "entry {} is missing `__REALTIME_TIMESTAMP`",
                    index + 1
                ))
            })?;
            let unit = match entry.get("_SYSTEMD_UNIT") {
                None | Some(serde_json::Value::Null) => String::new(),
                Some(value) => json_string(Some(value)).ok_or_else(|| {
                    PreprocessError::new(format!(
                        "entry {} has a non-scalar `_SYSTEMD_UNIT`",
                        index + 1
                    ))
                })?,
            };
            let priority = json_string(entry.get("PRIORITY")).unwrap_or_default();
            let message = match entry.get("MESSAGE") {
                Some(serde_json::Value::String(text)) => text.clone(),
                // journald renders a non-UTF-8 message as an array of byte
                // values; there is no faithful text cell for that.
                other => {
                    return Err(PreprocessError::new(format!(
                        "entry {} has no string `MESSAGE` (binary payload?): {}",
                        index + 1,
                        other.map(|value| preview(&value.to_string())).unwrap_or_default()
                    )))
                }
            };
            rows.push(vec![timestamp, unit, priority, message]);
        }
        table(&JOURNALCTL_COLUMNS, rows, "no journal entries in the output")
    }
}

/// Columns produced by [`IpPreprocessor`].
const IP_COLUMNS: [&str; 5] = ["ifname", "ifindex", "operstate", "family", "address"];

/// The argument list [`IpPreprocessor`] claims (issue #423).
const IP_CANONICAL_ARGS: &str = "-j addr";

/// Reads `ip -j addr` into one row per address.
///
/// `ip`'s `-j` gives a top-level array of interface objects, each carrying
/// an `addr_info` array of the addresses configured on it. Rows are
/// flattened the way [`LsblkPreprocessor`] flattens partitions: one row per
/// address, with `local` and `prefixlen` rejoined into CIDR notation.
///
/// **An interface with no addresses still gets a row**, with empty `family`
/// and `address` — real output has them (a `DOWN` interface reports
/// `addr_info: []`), and dropping the row would hide the interface's
/// existence and its `operstate`, which is often the very difference worth
/// seeing across a fleet. That is `addr_info: []` specifically: an
/// interface with **no** `addr_info` key is not `ip addr` output (`ip -j
/// link` prints interfaces without one) and is refused, since reading it as
/// "no addresses" would render an address table that silently holds none.
///
/// Each address must carry a string `family`, a string `local` and a
/// numeric `prefixlen`, and each interface a string `operstate`. iproute2's
/// schema marks `family` and `operstate` mutually exclusive with
/// `family_index` / `operstate_index`, emitted when the value is unknown to
/// it; this table has no such column, so an absent one is refused rather
/// than defaulted to an empty cell that would read as "none".
pub struct IpPreprocessor;

impl OutputPreprocessor for IpPreprocessor {
    fn name(&self) -> &'static str {
        "ip"
    }

    fn matches(&self, command: &str) -> bool {
        is_canonical_invocation(command, "ip", IP_CANONICAL_ARGS)
    }

    fn preprocess(
        &self,
        _command: &str,
        output: &str,
    ) -> Result<PreprocessedOutput, PreprocessError> {
        let interfaces = parse_json_array(output, "ip")?;
        let mut rows = Vec::new();
        for interface in &interfaces {
            let ifname = json_string(interface.get("ifname"))
                .ok_or_else(|| PreprocessError::new("an interface is missing `ifname`"))?;
            let ifindex = json_string(interface.get("ifindex")).ok_or_else(|| {
                PreprocessError::new(format!("interface `{ifname}` is missing `ifindex`"))
            })?;
            // `operstate` and `family` below are both fields iproute2's
            // schema marks mutually exclusive with an `*_index` variant it
            // emits when the value is unknown to it. This table has no such
            // column, so an absent one is refused rather than defaulted:
            // an empty cell would read as "no state" / "no family" instead
            // of "a value we cannot render".
            let operstate = interface
                .get("operstate")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    PreprocessError::new(format!(
                        "interface `{ifname}` is missing a string `operstate`"
                    ))
                })?
                .to_string();
            // Every `ip addr` interface carries `addr_info`, empty when the
            // interface has no addresses. Absent entirely means this is not
            // `ip addr` output at all — `ip -j link` prints interfaces
            // without it — and treating that as "no addresses" would render
            // a whole address table that silently contains none.
            let addresses = interface
                .get("addr_info")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    PreprocessError::new(format!(
                        "interface `{ifname}` has no `addr_info` array (not `ip addr` output?)"
                    ))
                })?;
            if addresses.is_empty() {
                rows.push(vec![
                    ifname,
                    ifindex,
                    operstate,
                    String::new(),
                    String::new(),
                ]);
                continue;
            }
            for address in addresses {
                let family = address
                    .get("family")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        PreprocessError::new(format!(
                            "an address of `{ifname}` is missing a string `family`"
                        ))
                    })?;
                let local = address
                    .get("local")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        PreprocessError::new(format!(
                            "an address of `{ifname}` is missing a string `local`"
                        ))
                    })?;
                // iproute2 prints `prefixlen` as a number. Requiring it
                // keeps the column one format: without it a row would carry
                // a bare address while its neighbours carry CIDR, and the
                // two would compare as different across a fleet.
                let prefix = address
                    .get("prefixlen")
                    .and_then(serde_json::Value::as_u64)
                    .ok_or_else(|| {
                        PreprocessError::new(format!(
                            "an address of `{ifname}` is missing a numeric `prefixlen`"
                        ))
                    })?;
                let cidr = format!("{local}/{prefix}");
                let family = family.to_string();
                rows.push(vec![
                    ifname.clone(),
                    ifindex.clone(),
                    operstate.clone(),
                    family,
                    cidr,
                ]);
            }
        }
        table(&IP_COLUMNS, rows, "no interfaces in the output")
    }
}

/// Parse `output` as a top-level JSON array, naming `tool` in the error.
///
/// Shared by the preprocessors whose command prints one (`systemctl`, `ip`).
/// A tool that is missing, or refuses to run, prints prose rather than JSON,
/// and that is the ordinary path to [`RawFallback::Unparseable`].
fn parse_json_array(output: &str, tool: &str) -> Result<Vec<serde_json::Value>, PreprocessError> {
    let trimmed = output.trim();
    if trimmed.is_empty() {
        return Err(PreprocessError::new("output is empty"));
    }
    let value: serde_json::Value = serde_json::from_str(trimmed)
        .map_err(|error| PreprocessError::new(format!("not valid JSON: {error}")))?;
    match value {
        serde_json::Value::Array(items) => Ok(items),
        other => Err(PreprocessError::new(format!(
            "{tool} output is not a JSON array but {}",
            match other {
                serde_json::Value::Object(_) => "an object",
                serde_json::Value::String(_) => "a string",
                serde_json::Value::Null => "null",
                _ => "a scalar",
            }
        ))),
    }
}

/// A JSON string or number as a plain string; `None` for anything else
/// (missing, `null`, bool, object, array).
fn json_string(value: Option<&serde_json::Value>) -> Option<String> {
    match value {
        Some(serde_json::Value::String(text)) => Some(text.clone()),
        Some(serde_json::Value::Number(number)) => Some(number.to_string()),
        _ => None,
    }
}

/// Is `text` a non-empty run of ASCII digits?
fn is_all_digits(text: &str) -> bool {
    !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit())
}

/// Is `text` a plain decimal number, the way `ps` prints a percentage
/// (`0.0`, `1.3`, `100`)?
///
/// Deliberately stricter than `f64::from_str`, which also accepts `NaN`,
/// `inf`, `-inf` and exponent forms: those parse successfully and would
/// have slipped through a check that only asked whether parsing worked,
/// putting a value `ps` never prints into a numeric column.
fn is_decimal(text: &str) -> bool {
    let mut digits = 0usize;
    let mut dots = 0usize;
    for byte in text.bytes() {
        match byte {
            b'0'..=b'9' => digits += 1,
            b'.' => dots += 1,
            _ => return false,
        }
    }
    digits > 0 && dots <= 1
}

/// Build a table from `columns` and `rows`, refusing an empty row set with
/// `empty_reason` — the shape every preprocessor here ends with.
fn table(
    columns: &[&str],
    rows: Vec<Vec<String>>,
    empty_reason: &str,
) -> Result<PreprocessedOutput, PreprocessError> {
    if rows.is_empty() {
        return Err(PreprocessError::new(empty_reason.to_string()));
    }
    PreprocessedOutput::new(columns.iter().map(|column| column.to_string()).collect(), rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A preprocessor with a fixed verdict, for registry-policy tests.
    struct Stub {
        name: &'static str,
        claims: bool,
        result: Result<PreprocessedOutput, PreprocessError>,
    }

    impl OutputPreprocessor for Stub {
        fn name(&self) -> &'static str {
            self.name
        }

        fn matches(&self, _command: &str) -> bool {
            self.claims
        }

        fn preprocess(
            &self,
            _command: &str,
            _output: &str,
        ) -> Result<PreprocessedOutput, PreprocessError> {
            self.result.clone()
        }
    }

    fn stub(
        name: &'static str,
        claims: bool,
        result: Result<PreprocessedOutput, PreprocessError>,
    ) -> Box<dyn OutputPreprocessor> {
        Box::new(Stub {
            name,
            claims,
            result,
        })
    }

    fn one_field_table(field: &str) -> PreprocessedOutput {
        PreprocessedOutput::new(
            vec!["value".to_string()],
            vec![vec![field.to_string()]],
        )
        .expect("valid stub table")
    }

    const DF_SAMPLE: &str = "\
Filesystem     1K-blocks    Used Available Use% Mounted on
/dev/sda1       40160304 6421852  31675276  17% /
tmpfs            3986548       0   3986548   0% /dev/shm
";

    #[test]
    fn empty_registry_reports_no_preprocessor() {
        let registry = PreprocessorRegistry::new();
        assert!(registry.is_empty());
        let outcome = registry.preprocess("df -h", DF_SAMPLE);
        assert_eq!(
            outcome,
            PreprocessOutcome::Raw {
                reason: RawFallback::NoPreprocessor
            }
        );
    }

    #[test]
    fn df_output_becomes_a_typed_table() {
        let registry = PreprocessorRegistry::default();
        assert_eq!(registry.len(), 10);
        let outcome = registry.preprocess("/bin/df -h", DF_SAMPLE);
        match outcome {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "df");
                assert_eq!(
                    table.columns(),
                    ["filesystem", "size", "used", "avail", "use_percent", "mount"]
                );
                assert_eq!(table.column_index("mount"), Some(5));
                assert_eq!(table.column_index("missing"), None);
                assert!(!table.is_empty());
                assert_eq!(table.row_count(), 2);
                assert_eq!(
                    table.rows()[0],
                    ["/dev/sda1", "40160304", "6421852", "31675276", "17", "/"]
                );
                assert_eq!(table.rows()[1][0], "tmpfs");
                assert_eq!(table.rows()[1][4], "0");
                assert_eq!(table.rows()[1][5], "/dev/shm");
            }
            other => panic!("expected a structured outcome, got {other:?}"),
        }
    }

    #[test]
    fn df_mount_point_with_spaces_is_kept_whole() {
        let output = "\
Filesystem 1K-blocks Used Available Use% Mounted on
//nas/share 104857600 20971520 83886080 20% /mnt/my files
";
        let outcome = PreprocessorRegistry::default().preprocess("df", output);
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0][5], "/mnt/my files");
                assert_eq!(table.rows()[0][0], "//nas/share");
            }
            other => panic!("expected a structured outcome, got {other:?}"),
        }
    }

    #[test]
    fn df_dash_use_percent_is_accepted() {
        // GNU df prints `-` instead of a percentage for filesystems that have
        // no size, e.g. `df /proc`.
        let output = "\
Filesystem 1K-blocks Used Available Use% Mounted on
proc 0 0 0 - /proc
";
        let outcome = PreprocessorRegistry::default().preprocess("df /proc", output);
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0][4], "-");
                assert_eq!(table.rows()[0][5], "/proc");
            }
            other => panic!("expected a structured outcome, got {other:?}"),
        }
    }

    #[test]
    fn truncated_df_output_degrades_to_raw() {
        // The fleet truncation cuts mid-line: the last row is incomplete.
        let output = "\
Filesystem     1K-blocks    Used Available Use% Mounted on
/dev/sda1       40160304 6421852  31675276  17% /
tmpfs            3986548       0   3986548
";
        let outcome = PreprocessorRegistry::default().preprocess("df -h", output);
        match outcome {
            PreprocessOutcome::Raw {
                reason: RawFallback::Unparseable(error),
            } => {
                assert!(error.reason().contains("truncated"), "{error}");
            }
            other => panic!("expected raw fallback, got {other:?}"),
        }
    }

    #[test]
    fn header_only_df_output_degrades_to_raw() {
        let output = "Filesystem     1K-blocks    Used Available Use% Mounted on\n";
        let outcome = PreprocessorRegistry::default().preprocess("df -h", output);
        match outcome {
            PreprocessOutcome::Raw {
                reason: RawFallback::Unparseable(error),
            } => {
                assert!(error.reason().contains("no data rows"), "{error}");
            }
            other => panic!("expected raw fallback, got {other:?}"),
        }
    }

    #[test]
    fn empty_and_garbage_output_degrade_to_raw() {
        let registry = PreprocessorRegistry::default();
        for output in ["", "\n\n  \n", "df: no file systems processed\n"] {
            let outcome = registry.preprocess("df", output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn df_inode_mode_is_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in ["df -i", "df --inodes", "df -hi"] {
            assert_eq!(
                registry.preprocess(command, DF_SAMPLE),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn df_format_changing_options_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in [
            "df -T",
            "df -hT",
            "df --print-type",
            "df --output=source,size",
            // GNU getopt accepts unambiguous long-option abbreviations.
            "df --ino /",
            "df --print-t /",
            "df --o",
            "df --out=source,itotal,iused,iavail,ipcent,target",
        ] {
            assert_eq!(
                registry.preprocess(command, DF_SAMPLE),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn df_harmless_long_options_are_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in [
            "df --no-sync",
            "df --block-size=1M",
            "df --all",
            "df --portability",
            "df --type=ext4",
            "df --si",
        ] {
            match registry.preprocess(command, DF_SAMPLE) {
                PreprocessOutcome::Structured { preprocessor, .. } => {
                    assert_eq!(preprocessor, "df", "{command}");
                }
                other => panic!("expected {command} to be claimed, got {other:?}"),
            }
        }
    }

    #[test]
    fn compound_and_redirected_dfs_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in ["df -h | sort", "df; uptime", "df > /tmp/df.txt", "df -h && echo ok"] {
            assert_eq!(
                registry.preprocess(command, DF_SAMPLE),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn unknown_commands_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in ["cat /etc/fstab", "lsblk", "dfx list"] {
            assert_eq!(
                registry.preprocess(command, DF_SAMPLE),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn custom_preprocessors_are_consulted() {
        let mut registry = PreprocessorRegistry::default();
        registry.register(stub(
            "uptime",
            true,
            Ok(one_field_table("through the stub")),
        ));
        match registry.preprocess("uptime", "some output") {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "uptime");
                assert_eq!(table.rows()[0][0], "through the stub");
            }
            other => panic!("expected the stub to answer, got {other:?}"),
        }
        // The built-in still wins for the command it owns.
        match registry.preprocess("df", DF_SAMPLE) {
            PreprocessOutcome::Structured { preprocessor, .. } => assert_eq!(preprocessor, "df"),
            other => panic!("expected the df built-in, got {other:?}"),
        }
    }

    #[test]
    fn a_failing_claimant_falls_through_to_the_next() {
        let mut registry = PreprocessorRegistry::new();
        registry
            .register(stub(
                "first",
                true,
                Err(PreprocessError::new("first cannot read this")),
            ))
            .register(stub("second", true, Ok(one_field_table("second answers"))));
        match registry.preprocess("anything", "output") {
            PreprocessOutcome::Structured { preprocessor, .. } => {
                assert_eq!(preprocessor, "second");
            }
            other => panic!("expected the second claimant, got {other:?}"),
        }
    }

    #[test]
    fn when_all_claimants_fail_the_last_error_comes_back() {
        let mut registry = PreprocessorRegistry::new();
        registry
            .register(stub(
                "first",
                true,
                Err(PreprocessError::new("first failed")),
            ))
            .register(stub(
                "second",
                true,
                Err(PreprocessError::new("second failed")),
            ));
        match registry.preprocess("anything", "output") {
            PreprocessOutcome::Raw {
                reason: RawFallback::Unparseable(error),
            } => assert_eq!(error.reason(), "second failed"),
            other => panic!("expected the last error to come back, got {other:?}"),
        }
    }

    #[test]
    fn ragged_tables_are_rejected() {
        let error = PreprocessedOutput::new(
            vec!["a".to_string(), "b".to_string()],
            vec![
                vec!["1".to_string(), "2".to_string()],
                vec!["3".to_string()],
            ],
        )
        .expect_err("a ragged table must be rejected");
        assert!(error.reason().contains("ragged"), "{error}");
    }

    #[test]
    fn tables_without_columns_are_rejected() {
        let error =
            PreprocessedOutput::new(Vec::new(), Vec::new()).expect_err("columns are required");
        assert!(error.reason().contains("column"), "{error}");
    }

    #[test]
    fn adversarial_outputs_do_not_panic() {
        let registry = PreprocessorRegistry::default();
        let tough: [&str; 9] = [
            "",
            "\n\n\n",
            "   ",
            "Filesystem\n",
            "Filesystem 1K-blocks Used Available Use% Mounted on\n",
            "Filesystem 1K-blocks Used Available Use% Mounted on\n/ / / / / / / / / /\n",
            "Filesystem\u{0}1K-blocks\n",
            "df: /proc: No such file or directory\n",
            "Filesystem 1K-blocks Used Available Use% Mounted on\nproc 0 0 0 - not/a/mount\n",
        ];
        for output in tough {
            // Any outcome is acceptable here — the point is no panic.
            let _ = registry.preprocess("df -h", output);
        }
        let long_row = format!(
            "Filesystem 1K-blocks Used Available Use% Mounted on\n{}\n",
            "x ".repeat(5_000)
        );
        let _ = registry.preprocess("df", &long_row);
    }

    // --- df --output=<canonical> (issue #421) ---

    #[test]
    fn df_canonical_output_flag_is_claimed_and_parsed() {
        let registry = PreprocessorRegistry::default();
        let outcome = registry.preprocess(
            "df --output=source,size,used,avail,pcent,target",
            DF_SAMPLE,
        );
        match outcome {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "df");
                assert_eq!(table.row_count(), 2);
                assert_eq!(table.rows()[0][0], "/dev/sda1");
            }
            other => panic!("expected the canonical --output form to be claimed, got {other:?}"),
        }
    }

    #[test]
    fn df_canonical_output_combines_with_harmless_flags() {
        let registry = PreprocessorRegistry::default();
        let outcome = registry.preprocess(
            "df -h --output=source,size,used,avail,pcent,target",
            DF_SAMPLE,
        );
        assert!(
            outcome.is_structured(),
            "canonical --output plus -h must still be claimed, got {outcome:?}"
        );
    }

    #[test]
    fn df_non_canonical_output_field_lists_stay_declined() {
        let registry = PreprocessorRegistry::default();
        for command in [
            // Same fields, different order: not the exact layout this
            // preprocessor's positional parse assumes.
            "df --output=target,source,size,used,avail,pcent",
            // A subset of the canonical fields.
            "df --output=source,size",
            // The canonical fields plus one more.
            "df --output=source,fstype,size,used,avail,pcent,target",
        ] {
            assert_eq!(
                registry.preprocess(command, DF_SAMPLE),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn busybox_df_rejecting_output_flag_degrades_to_raw_not_panic() {
        // Alpine's busybox df does not understand --output: it errors out
        // instead of printing a table. The command still looks like our
        // canonical invocation, so it is claimed, but the output isn't a df
        // table, so parsing must fail closed rather than panic.
        let registry = PreprocessorRegistry::default();
        let busybox_error = "df: unrecognized option '--output'\n\
             BusyBox v1.36.1 (2024-03-05 09:00:00 UTC) multi-call binary.\n";
        let outcome = registry.preprocess(
            "df --output=source,size,used,avail,pcent,target",
            busybox_error,
        );
        assert!(
            matches!(
                outcome,
                PreprocessOutcome::Raw {
                    reason: RawFallback::Unparseable(_)
                }
            ),
            "expected a graceful raw fallback, got {outcome:?}"
        );
    }

    // --- Real-world df samples across distros (issue #421 DoD) ---

    #[test]
    fn debian_df_output_parses() {
        let output = "\
Filesystem     1K-blocks    Used Available Use% Mounted on
udev             4030716       0   4030716   0% /dev
tmpfs             811772    1512    810260   1% /run
/dev/sda1       20509268 6421104  12994700  34% /
tmpfs            4058848       0   4058848   0% /dev/shm
overlay          20509268 6421104  12994700  34% /var/lib/docker/overlay2/abc123/merged
";
        let outcome = PreprocessorRegistry::default().preprocess("df", output);
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.row_count(), 5);
                assert_eq!(table.rows()[2][0], "/dev/sda1");
                assert_eq!(table.rows()[4][5], "/var/lib/docker/overlay2/abc123/merged");
            }
            other => panic!("expected a structured Debian table, got {other:?}"),
        }
    }

    #[test]
    fn rhel_family_df_output_parses() {
        let output = "\
Filesystem                  1K-blocks    Used Available Use% Mounted on
devtmpfs                       4030716       0   4030716   0% /dev
/dev/mapper/rhel-root          52403200 8912340  43490860  17% /
/dev/sda1                       1038336  345678    692658  34% /boot
tmpfs                            811772       0    811772   0% /dev/shm
";
        let outcome = PreprocessorRegistry::default().preprocess("df", output);
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.row_count(), 4);
                assert_eq!(table.rows()[1][0], "/dev/mapper/rhel-root");
                assert_eq!(table.rows()[2][5], "/boot");
            }
            other => panic!("expected a structured RHEL-family table, got {other:?}"),
        }
    }

    #[test]
    fn alpine_busybox_plain_df_output_parses() {
        // Busybox df's default (no-flags) output matches the same six-column
        // GNU shape; only its flag support (no --output) differs.
        let output = "\
Filesystem           1K-blocks      Used Available Use% Mounted on
overlay               10188088   1234560   8425000  13% /
tmpfs                    65536         0     65536   0% /dev
/dev/sda1              1998672    89012   1786000   5% /etc/resolv.conf
";
        let outcome = PreprocessorRegistry::default().preprocess("df", output);
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.row_count(), 3);
                assert_eq!(table.rows()[0][0], "overlay");
            }
            other => panic!("expected a structured Alpine/busybox table, got {other:?}"),
        }
    }

    // --- lsblk --json (issue #421) ---

    const LSBLK_SAMPLE_SINGULAR: &str = r#"{
   "blockdevices": [
      {"name": "sda", "size": "20G", "type": "disk", "mountpoint": null,
       "children": [
          {"name": "sda1", "size": "1G", "type": "part", "mountpoint": "/boot"},
          {"name": "sda2", "size": "19G", "type": "part", "mountpoint": "/"}
       ]
      },
      {"name": "sr0", "size": "1024M", "type": "rom", "mountpoint": null}
   ]
}"#;

    const LSBLK_SAMPLE_PLURAL: &str = r#"{
   "blockdevices": [
      {"name": "vda", "size": "40G", "type": "disk", "mountpoints": [null],
       "children": [
          {"name": "vda1", "size": "40G", "type": "part", "mountpoints": ["/"]}
       ]
      }
   ]
}"#;

    #[test]
    fn lsblk_json_flattens_children_into_rows() {
        let registry = PreprocessorRegistry::default();
        let outcome = registry.preprocess("lsblk --json", LSBLK_SAMPLE_SINGULAR);
        match outcome {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "lsblk");
                assert_eq!(table.columns(), ["name", "size", "type", "mountpoint"]);
                assert_eq!(table.row_count(), 4);
                assert_eq!(table.rows()[0], ["sda", "20G", "disk", ""]);
                assert_eq!(table.rows()[1], ["sda1", "1G", "part", "/boot"]);
                assert_eq!(table.rows()[2], ["sda2", "19G", "part", "/"]);
                assert_eq!(table.rows()[3], ["sr0", "1024M", "rom", ""]);
            }
            other => panic!("expected a structured lsblk table, got {other:?}"),
        }
    }

    #[test]
    fn lsblk_json_short_flag_is_claimed() {
        let outcome = PreprocessorRegistry::default().preprocess("lsblk -J", LSBLK_SAMPLE_SINGULAR);
        assert!(outcome.is_structured(), "{outcome:?}");
    }

    #[test]
    fn lsblk_plural_mountpoints_array_is_read() {
        // Newer util-linux reports `mountpoints` (an array, `null` entries
        // for an unmounted device) instead of the older singular field.
        let outcome =
            PreprocessorRegistry::default().preprocess("lsblk --json", LSBLK_SAMPLE_PLURAL);
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0], ["vda", "40G", "disk", ""]);
                assert_eq!(table.rows()[1], ["vda1", "40G", "part", "/"]);
            }
            other => panic!("expected a structured table, got {other:?}"),
        }
    }

    #[test]
    fn lsblk_multiple_mountpoints_are_joined() {
        let output = r#"{"blockdevices": [
            {"name": "sdb1", "size": "5G", "type": "part",
             "mountpoints": ["/mnt/a", "/mnt/b"]}
        ]}"#;
        let outcome = PreprocessorRegistry::default().preprocess("lsblk --json", output);
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0][3], "/mnt/a, /mnt/b");
            }
            other => panic!("expected a structured table, got {other:?}"),
        }
    }

    #[test]
    fn lsblk_numeric_bytes_size_is_stringified() {
        // `--bytes` turns `size` into a JSON number instead of a string.
        let output = r#"{"blockdevices": [
            {"name": "sda", "size": 21474836480, "type": "disk", "mountpoint": null}
        ]}"#;
        let outcome = PreprocessorRegistry::default().preprocess("lsblk --json -b", output);
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0][1], "21474836480");
            }
            other => panic!("expected a structured table, got {other:?}"),
        }
    }

    #[test]
    fn lsblk_without_json_flag_is_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in ["lsblk", "lsblk -a", "lsblk --all"] {
            assert_eq!(
                registry.preprocess(command, LSBLK_SAMPLE_SINGULAR),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn lsblk_custom_output_flag_is_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in [
            "lsblk --json --output NAME,SIZE",
            "lsblk --json -oNAME,SIZE",
            "lsblk -J -o NAME",
        ] {
            assert_eq!(
                registry.preprocess(command, LSBLK_SAMPLE_SINGULAR),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn compound_and_redirected_lsblks_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in [
            "lsblk --json | jq .",
            "lsblk --json; uptime",
            "lsblk --json > /tmp/lsblk.json",
        ] {
            assert_eq!(
                registry.preprocess(command, LSBLK_SAMPLE_SINGULAR),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn old_lsblk_without_json_support_degrades_to_raw_not_panic() {
        // Busybox (and some very old util-linux builds) don't support
        // --json at all: the flag is rejected and plain text (or an error)
        // comes back instead of JSON. The command is still claimed
        // syntactically, but parsing must fail closed, not panic.
        let registry = PreprocessorRegistry::default();
        let not_json_outputs = [
            "lsblk: unrecognized option '--json'\n",
            "NAME   MAJ:MIN RM  SIZE RO TYPE MOUNTPOINT\nsda      8:0    0   20G  0 disk \n",
            "",
            "   \n",
            "{not valid json",
            "null",
            "[]",
            "{\"blockdevices\": \"not an array\"}",
            "{\"blockdevices\": []}",
            "{\"blockdevices\": [{\"name\": \"sda\"}]}",
        ];
        for output in not_json_outputs {
            let outcome = registry.preprocess("lsblk --json", output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a graceful raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn lsblk_malformed_children_is_rejected_not_silently_dropped() {
        // `children` present but not an array must fail closed rather than
        // silently behave as "no children" and omit real partitions.
        let registry = PreprocessorRegistry::default();
        for output in [
            r#"{"blockdevices": [{"name": "sda", "size": "1G", "type": "disk", "children": "sda1"}]}"#,
            r#"{"blockdevices": [{"name": "sda", "size": "1G", "type": "disk", "children": 1}]}"#,
            r#"{"blockdevices": [{"name": "sda", "size": "1G", "type": "disk", "children": null}]}"#,
            r#"{"blockdevices": [{"name": "sda", "size": "1G", "type": "disk", "children": {}}]}"#,
        ] {
            assert!(
                matches!(
                    registry.preprocess("lsblk --json", output),
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a malformed `children` to be rejected for {output:?}"
            );
        }
    }

    #[test]
    fn lsblk_malformed_mountpoints_is_rejected_not_silently_dropped() {
        // A `mountpoints` array entry that is neither a string nor null, or
        // a `mountpoints`/`mountpoint` field of the wrong shape entirely,
        // must fail closed rather than be silently filtered out of the
        // joined mount-point string.
        let registry = PreprocessorRegistry::default();
        for output in [
            // Entry is a number, not a string or null.
            r#"{"blockdevices": [{"name": "sda1", "size": "1G", "type": "part", "mountpoints": [1]}]}"#,
            // Entry is an object.
            r#"{"blockdevices": [{"name": "sda1", "size": "1G", "type": "part", "mountpoints": [{}]}]}"#,
            // `mountpoints` itself is not an array.
            r#"{"blockdevices": [{"name": "sda1", "size": "1G", "type": "part", "mountpoints": "/"}]}"#,
            r#"{"blockdevices": [{"name": "sda1", "size": "1G", "type": "part", "mountpoints": null}]}"#,
            // Singular `mountpoint` is not a string or null.
            r#"{"blockdevices": [{"name": "sda1", "size": "1G", "type": "part", "mountpoint": 1}]}"#,
            r#"{"blockdevices": [{"name": "sda1", "size": "1G", "type": "part", "mountpoint": {}}]}"#,
        ] {
            assert!(
                matches!(
                    registry.preprocess("lsblk --json", output),
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a malformed mountpoint field to be rejected for {output:?}"
            );
        }
    }

    #[test]
    fn lsblk_adversarial_outputs_do_not_panic() {
        let registry = PreprocessorRegistry::default();
        let tough = [
            "",
            "\n\n\n",
            "{",
            "{}",
            "{\"blockdevices\": null}",
            "{\"blockdevices\": [null]}",
            "{\"blockdevices\": [{\"name\": null, \"size\": \"1G\", \"type\": \"disk\"}]}",
            "{\"blockdevices\": [{\"name\": \"a\", \"size\": {}, \"type\": \"disk\"}]}",
            "\u{0}\u{0}\u{0}",
        ];
        for output in tough {
            let _ = registry.preprocess("lsblk --json", output);
        }
        // Deeply nested children must not blow the stack in a normal test run.
        let mut nested = String::from(r#"{"blockdevices": [{"name": "d0", "size": "1G", "type": "disk""#);
        for i in 1..200 {
            nested.push_str(&format!(
                r#", "children": [{{"name": "d{i}", "size": "1G", "type": "part""#
            ));
        }
        for _ in 1..200 {
            nested.push_str("}]");
        }
        nested.push('}');
        nested.push_str("]}");
        let _ = registry.preprocess("lsblk --json", &nested);
    }

    // --- dpkg-query / rpm package inventories (issue #422) ---

    const DPKG_COMMAND: &str = r"dpkg-query -W -f='${Package}\t${Version}\n'";
    const RPM_COMMAND: &str = r"rpm -qa --qf='%{NAME}\t%{EPOCHNUM}:%{VERSION}-%{RELEASE}\n'";

    /// Real Debian 12 output: an epoch in one version, a `+deb12u1` distro
    /// suffix, a `~` pre-release separator, and a package whose name carries
    /// a `-dev` suffix.
    const DPKG_SAMPLE: &str = "\
base-files\t12.4+deb12u5
libc6\t2.36-9+deb12u7
libssl3\t3.0.11-1~deb12u2
nginx\t1.22.1-9
util-linux\t2.38.1-5+deb12u1
zlib1g-dev\t1:1.2.13.dfsg-1
";

    /// Real RHEL 9 output: `%{EPOCHNUM}:%{VERSION}-%{RELEASE}`, with `.el9`
    /// releases, a module-build release, an unset epoch normalised to `0`
    /// and the non-zero epochs `nginx` and `grub2-tools` really carry there.
    const RPM_SAMPLE: &str = "\
bash\t0:5.1.8-9.el9
glibc\t0:2.34-100.el9_4.2
grub2-tools\t1:2.06-80.el9
nginx\t1:1.20.1-20.el9_4.1
openssl-libs\t0:3.0.7-27.el9
util-linux\t0:2.37.4-18.el9
";

    #[test]
    fn dpkg_output_becomes_a_package_table() {
        let registry = PreprocessorRegistry::default();
        match registry.preprocess(DPKG_COMMAND, DPKG_SAMPLE) {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "dpkg-query");
                assert_eq!(table.columns(), ["package", "version"]);
                assert_eq!(table.row_count(), 6);
                assert_eq!(table.rows()[0], ["base-files", "12.4+deb12u5"]);
                assert_eq!(table.rows()[2], ["libssl3", "3.0.11-1~deb12u2"]);
                // An epoch is part of the version string, kept verbatim.
                assert_eq!(table.rows()[5], ["zlib1g-dev", "1:1.2.13.dfsg-1"]);
            }
            other => panic!("expected a structured dpkg table, got {other:?}"),
        }
    }

    #[test]
    fn rpm_output_becomes_a_package_table() {
        let registry = PreprocessorRegistry::default();
        match registry.preprocess(RPM_COMMAND, RPM_SAMPLE) {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "rpm");
                assert_eq!(table.columns(), ["package", "version"]);
                assert_eq!(table.row_count(), 6);
                assert_eq!(table.rows()[1], ["glibc", "0:2.34-100.el9_4.2"]);
                assert_eq!(table.rows()[2], ["grub2-tools", "1:2.06-80.el9"]);
                assert_eq!(table.rows()[3], ["nginx", "1:1.20.1-20.el9_4.1"]);
            }
            other => panic!("expected a structured rpm table, got {other:?}"),
        }
    }

    #[test]
    fn rpm_versions_differing_only_by_epoch_stay_distinct() {
        // RPM compares the epoch first, so these are different packages —
        // a template without it would report both as `2.0-3`.
        let output = "app\t1:2.0-3\nother\t2:2.0-3\n";
        match PreprocessorRegistry::default().preprocess(RPM_COMMAND, output) {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0][1], "1:2.0-3");
                assert_eq!(table.rows()[1][1], "2:2.0-3");
                assert_ne!(table.rows()[0][1], table.rows()[1][1]);
            }
            other => panic!("expected a structured rpm table, got {other:?}"),
        }
    }

    #[test]
    fn package_commands_are_claimed_as_a_path_and_with_extra_spacing() {
        let registry = PreprocessorRegistry::default();
        let dpkg_by_path = r"/usr/bin/dpkg-query -W  -f='${Package}\t${Version}\n'";
        assert!(
            registry.preprocess(dpkg_by_path, DPKG_SAMPLE).is_structured(),
            "a path-qualified dpkg-query with extra spacing must still be claimed"
        );
        let rpm_by_path = r"/usr/bin/rpm -qa --qf='%{NAME}\t%{EPOCHNUM}:%{VERSION}-%{RELEASE}\n'";
        assert!(
            registry.preprocess(rpm_by_path, RPM_SAMPLE).is_structured(),
            "a path-qualified rpm must still be claimed"
        );
    }

    #[test]
    fn non_canonical_package_commands_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        let cases = [
            // No template at all: the default output is not two fields.
            ("dpkg-query -W", DPKG_SAMPLE),
            ("dpkg -l", DPKG_SAMPLE),
            ("rpm -qa", RPM_SAMPLE),
            // A different template, whose columns we do not know.
            (r"dpkg-query -W -f='${Package}\n'", DPKG_SAMPLE),
            (
                r"dpkg-query -W -f='${Package}\t${Version}\t${Status}\n'",
                DPKG_SAMPLE,
            ),
            (r"rpm -qa --qf='%{NAME}\n'", RPM_SAMPLE),
            // The pre-epoch template is not kept as an alternative: it
            // cannot tell `1:2.0-3` from `2:2.0-3`.
            (r"rpm -qa --qf='%{NAME}\t%{VERSION}-%{RELEASE}\n'", RPM_SAMPLE),
            // `%{EPOCH}` renders an unset epoch as the literal `(none)`,
            // which is not comparable across hosts.
            (
                r"rpm -qa --qf='%{NAME}\t%{EPOCH}:%{VERSION}-%{RELEASE}\n'",
                RPM_SAMPLE,
            ),
            // An appended pipeline is an extra word, so the match fails.
            (
                r"dpkg-query -W -f='${Package}\t${Version}\n' | head",
                DPKG_SAMPLE,
            ),
            (
                r"rpm -qa --qf='%{NAME}\t%{VERSION}-%{RELEASE}\n' > /tmp/pkgs",
                RPM_SAMPLE,
            ),
        ];
        for (command, output) in cases {
            assert_eq!(
                registry.preprocess(command, output),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn shell_syntax_in_the_program_word_is_not_claimed() {
        // `base_name` alone would read these as `dpkg-query` / `rpm`, but
        // what a substitution actually runs is the shell's decision, not
        // something this matcher can stand behind.
        let registry = PreprocessorRegistry::default();
        let cases = [
            (
                r"$(pwd)/dpkg-query -W -f='${Package}\t${Version}\n'",
                DPKG_SAMPLE,
            ),
            (
                r"`which dpkg-query` -W -f='${Package}\t${Version}\n'",
                DPKG_SAMPLE,
            ),
            (
                r"$HOME/bin/rpm -qa --qf='%{NAME}\t%{EPOCHNUM}:%{VERSION}-%{RELEASE}\n'",
                RPM_SAMPLE,
            ),
        ];
        for (command, output) in cases {
            assert_eq!(
                registry.preprocess(command, output),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn truncated_package_output_degrades_to_raw() {
        let registry = PreprocessorRegistry::default();
        let cases = [
            // The fleet truncation cut the last line before its tab.
            "base-files\t12.4+deb12u5\nlibc6\n",
            // ... or right after it, leaving an empty version.
            "base-files\t12.4+deb12u5\nlibc6\t\n",
            // A third field means this is not the pinned template's output.
            "base-files\t12.4+deb12u5\tinstalled\n",
            // Nothing usable at all.
            "",
            "\n\n  \n",
            "dpkg-query: no packages found matching nosuchpkg\n",
        ];
        for output in cases {
            let outcome = registry.preprocess(DPKG_COMMAND, output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    // --- application version banners (issue #422) ---

    #[test]
    fn nginx_version_banner_is_parsed() {
        let registry = PreprocessorRegistry::default();
        match registry.preprocess("nginx -v", "nginx version: nginx/1.24.0\n") {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "app-version");
                assert_eq!(table.columns(), ["program", "version", "raw"]);
                assert_eq!(
                    table.rows()[0],
                    ["nginx", "1.24.0", "nginx version: nginx/1.24.0"]
                );
            }
            other => panic!("expected a structured version row, got {other:?}"),
        }
    }

    #[test]
    fn nginx_version_with_distro_suffix_is_parsed() {
        let outcome = PreprocessorRegistry::default()
            .preprocess("/usr/sbin/nginx -v", "nginx version: nginx/1.18.0 (Ubuntu)\n");
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0][0], "nginx");
                assert_eq!(table.rows()[0][1], "1.18.0");
            }
            other => panic!("expected a structured version row, got {other:?}"),
        }
    }

    #[test]
    fn php_version_banner_is_parsed_from_the_first_line() {
        // `php -v` prints four lines; the version is on the first.
        let output = "\
PHP 8.2.7 (cli) (built: Jun  9 2023 06:52:52) (NTS)
Copyright (c) The PHP Group
Zend Engine v4.2.7, Copyright (c) Zend Technologies
    with Zend OPcache v8.2.7, Copyright (c) Zend Technologies
";
        match PreprocessorRegistry::default().preprocess("php -v", output) {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.row_count(), 1);
                assert_eq!(table.rows()[0][0], "php");
                assert_eq!(table.rows()[0][1], "8.2.7");
                assert!(table.rows()[0][2].starts_with("PHP 8.2.7 (cli)"));
            }
            other => panic!("expected a structured version row, got {other:?}"),
        }
    }

    #[test]
    fn php_version_with_packaging_suffix_keeps_the_upstream_version() {
        let outcome = PreprocessorRegistry::default()
            .preprocess("php -v", "PHP 8.2.7-1ubuntu2 (cli) (built: ...)\n");
        match outcome {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0][1], "8.2.7");
            }
            other => panic!("expected a structured version row, got {other:?}"),
        }
    }

    #[test]
    fn unparsed_version_is_reported_not_dropped() {
        // The DoD of #422: an unknown version format must not be lost
        // silently — it lands in the result marked as unparsed (empty
        // `version`) with the original line preserved in `raw`.
        let registry = PreprocessorRegistry::default();
        let cases = [
            ("nginx -v", "nginx: command not found"),
            ("nginx -v", "nginx version: rolling"),
            ("php -v", "php: error while loading shared libraries: libx.so"),
            ("php -v", "PHP built from git"),
            ("nginx -v", "Segmentation fault"),
            // A line that merely mentions a version-shaped path is not a
            // version report: the banner's full prefix is required, so this
            // must not be read as `1.24.0`.
            (
                "nginx -v",
                "nginx: [emerg] cannot load /etc/nginx/1.24.0.bak",
            ),
            ("nginx -v", "error loading nginx/1.24.0"),
            // ... and the real banner must be at the start of the line.
            ("nginx -v", "note: nginx version: nginx/1.24.0"),
        ];
        for (command, output) in cases {
            match registry.preprocess(command, output) {
                PreprocessOutcome::Structured { preprocessor, table } => {
                    assert_eq!(preprocessor, "app-version", "{output:?}");
                    assert_eq!(table.row_count(), 1, "{output:?}");
                    assert_eq!(
                        table.rows()[0][1],
                        "",
                        "an unparsed version must be empty, not guessed: {output:?}"
                    );
                    assert_eq!(
                        table.rows()[0][2], output,
                        "the raw line must be preserved: {output:?}"
                    );
                }
                other => panic!("an unparsed banner must still be reported, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_version_output_degrades_to_raw() {
        // Nothing was printed at all, so there is no row to report.
        let registry = PreprocessorRegistry::default();
        for output in ["", "\n\n", "   \n  "] {
            let outcome = registry.preprocess("nginx -v", output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn non_version_commands_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in [
            // `-V` prints the whole configure line, a different shape.
            "nginx -V",
            "nginx",
            "nginx -t",
            "php --version",
            "php -v -a",
            "nginx -v | tail -1",
            "nginx -v; php -v",
            "nginx -v > /tmp/v",
            "httpd -v",
        ] {
            assert_eq!(
                registry.preprocess(command, "nginx version: nginx/1.24.0\n"),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn package_and_version_adversarial_outputs_do_not_panic() {
        let registry = PreprocessorRegistry::default();
        let tough = [
            "",
            "\n\n\n",
            "\t",
            "\t\t\t",
            "\u{0}\t\u{0}",
            "a\tb\tc\td",
            "пакет\tверсия",
            "nginx version: nginx/",
            "nginx version: nginx/...",
            "PHP ",
            "PHP .",
        ];
        for output in tough {
            // Any outcome is acceptable here — the point is no panic.
            let _ = registry.preprocess(DPKG_COMMAND, output);
            let _ = registry.preprocess(RPM_COMMAND, output);
            let _ = registry.preprocess("nginx -v", output);
            let _ = registry.preprocess("php -v", output);
        }
        let huge = format!("{}\t1.0\n", "p".repeat(20_000));
        let _ = registry.preprocess(DPKG_COMMAND, &huge);
        let _ = registry.preprocess("php -v", &huge);
    }

    // --- services, processes, sockets, journal, network (issue #423) ---

    const SYSTEMCTL_COMMAND: &str = "systemctl list-units --output=json";
    const PS_COMMAND: &str = "ps -eo pid,ppid,user,rss,pcpu,comm";
    const SS_COMMAND: &str = "ss -H -n";
    const JOURNALCTL_COMMAND: &str = "journalctl --output=json";
    const IP_COMMAND: &str = "ip -j addr";

    /// Real `systemctl list-units --output=json` shape (Debian 12, systemd
    /// 252): a top-level array, one object per unit.
    const SYSTEMCTL_SAMPLE: &str = r#"[
  {"unit":"dbus.service","load":"loaded","active":"active","sub":"running","description":"D-Bus System Message Bus"},
  {"unit":"nginx.service","load":"loaded","active":"active","sub":"running","description":"A high performance web server"},
  {"unit":"ssh.service","load":"loaded","active":"active","sub":"running","description":"OpenBSD Secure Shell server"},
  {"unit":"unattended-upgrades.service","load":"loaded","active":"inactive","sub":"dead","description":"Unattended Upgrades Shutdown"}
]"#;

    /// Real `ps -eo pid,ppid,user,rss,pcpu,comm` output, captured on the
    /// agent's own host — note the right-aligned numeric columns and the
    /// kernel-thread names carrying `/` and `-`.
    const PS_SAMPLE: &str = "\
  PID  PPID USER       RSS %CPU COMMAND
    1     0 root      4760  1.3 process_api
    2     0 root         0  0.0 kthreadd
    4     2 root         0  0.0 kworker/R-rcu_gp
  914   870 www-data  8321  0.4 nginx
";

    /// Real `ss -H -n` shape: internet sockets print six fields, unix
    /// sockets eight. Addresses are from the documentation ranges.
    const SS_SAMPLE: &str = "\
tcp   ESTAB 0      0      192.0.2.2:44438 198.51.100.7:443
tcp   LISTEN 0     128    0.0.0.0:22      0.0.0.0:*
tcp   ESTAB 0      0      [2001:db8::1]:8080 [2001:db8::2]:51234
u_str ESTAB 0      0              * 896               * 0
u_str ESTAB 0      0      /run/systemd/journal/stdout 21456 * 21455
";

    /// Real `journalctl --output=json` shape: JSON Lines, one object per
    /// entry, not an array.
    const JOURNALCTL_SAMPLE: &str = concat!(
        r#"{"__REALTIME_TIMESTAMP":"1789837304171000","PRIORITY":"6","_SYSTEMD_UNIT":"ssh.service","MESSAGE":"Server listening on 0.0.0.0 port 22."}"#,
        "\n",
        r#"{"__REALTIME_TIMESTAMP":"1789837305002000","PRIORITY":"3","_SYSTEMD_UNIT":"nginx.service","MESSAGE":"bind() to 0.0.0.0:80 failed (98: Address already in use)"}"#,
        "\n",
        // A kernel entry carries no _SYSTEMD_UNIT.
        r#"{"__REALTIME_TIMESTAMP":"1789837306500000","PRIORITY":"4","MESSAGE":"TCP: request_sock_TCP: Possible SYN flooding"}"#,
        "\n"
    );

    /// Real `ip -j addr` shape, captured on the agent's own host: `ifindex`
    /// is a JSON number, and a `DOWN` interface reports `addr_info: []`.
    const IP_SAMPLE: &str = r#"[
  {"ifindex":1,"ifname":"lo","operstate":"UNKNOWN","mtu":65536,
   "addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8},
                {"family":"inet6","local":"::1","prefixlen":128}]},
  {"ifindex":2,"ifname":"ifb0","operstate":"DOWN","mtu":1500,"addr_info":[]},
  {"ifindex":3,"ifname":"eth0","operstate":"UP","mtu":1500,
   "addr_info":[{"family":"inet","local":"192.0.2.2","prefixlen":24}]}
]"#;

    #[test]
    fn the_builtin_registry_holds_every_family() {
        assert_eq!(PreprocessorRegistry::default().len(), 10);
    }

    #[test]
    fn systemctl_json_becomes_unit_rows() {
        match PreprocessorRegistry::default().preprocess(SYSTEMCTL_COMMAND, SYSTEMCTL_SAMPLE) {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "systemctl");
                assert_eq!(
                    table.columns(),
                    ["unit", "load", "active", "sub", "description"]
                );
                assert_eq!(table.row_count(), 4);
                assert_eq!(
                    table.rows()[1],
                    [
                        "nginx.service",
                        "loaded",
                        "active",
                        "running",
                        "A high performance web server"
                    ]
                );
                assert_eq!(table.rows()[3][2], "inactive");
            }
            other => panic!("expected a structured systemctl table, got {other:?}"),
        }
    }

    #[test]
    fn a_host_without_systemd_degrades_to_raw() {
        // The issue's DoD. Two real shapes: the container message (systemctl
        // present but systemd is not PID 1) and Alpine/OpenRC, where the
        // binary is absent and the shell answers instead.
        let registry = PreprocessorRegistry::default();
        let cases = [
            "System has not been booted with systemd as init system (PID 1). Can't operate.\n\
             Failed to connect to bus: Host is down\n",
            "/bin/sh: systemctl: not found\n",
            "-ash: systemctl: not found\n",
        ];
        for output in cases {
            let outcome = registry.preprocess(SYSTEMCTL_COMMAND, output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn systemctl_missing_state_fields_degrade_to_raw() {
        let registry = PreprocessorRegistry::default();
        for output in [
            r#"[{"unit":"a.service","load":"loaded","active":"active"}]"#,
            r#"[{"load":"loaded","active":"active","sub":"running"}]"#,
            r#"[{"unit":"a.service","load":"loaded","active":"active","sub":"running","description":{}}]"#,
            "[]",
            r#"{"units":[]}"#,
        ] {
            let outcome = registry.preprocess(SYSTEMCTL_COMMAND, output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn ps_output_becomes_process_rows() {
        match PreprocessorRegistry::default().preprocess(PS_COMMAND, PS_SAMPLE) {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "ps");
                assert_eq!(table.columns(), ["pid", "ppid", "user", "rss", "pcpu", "comm"]);
                assert_eq!(table.row_count(), 4);
                assert_eq!(table.rows()[0], ["1", "0", "root", "4760", "1.3", "process_api"]);
                assert_eq!(table.rows()[2][5], "kworker/R-rcu_gp");
                assert_eq!(table.rows()[3], ["914", "870", "www-data", "8321", "0.4", "nginx"]);
            }
            other => panic!("expected a structured ps table, got {other:?}"),
        }
    }

    #[test]
    fn ps_command_name_with_spaces_is_kept_whole() {
        let output = "  PID  PPID USER       RSS %CPU COMMAND\n  \
                      42     1 root      1024  0.1 my helper\n";
        match PreprocessorRegistry::default().preprocess(PS_COMMAND, output) {
            PreprocessOutcome::Structured { table, .. } => {
                assert_eq!(table.rows()[0][5], "my helper");
            }
            other => panic!("expected a structured ps table, got {other:?}"),
        }
    }

    #[test]
    fn malformed_ps_output_degrades_to_raw() {
        let registry = PreprocessorRegistry::default();
        let cases = [
            // No header.
            "    1     0 root      4760  1.3 init\n",
            // Truncated mid-line: too few fields.
            "  PID  PPID USER       RSS %CPU COMMAND\n    1     0 root\n",
            // A non-numeric pid means the columns are not what we think.
            "  PID  PPID USER       RSS %CPU COMMAND\n  one     0 root  4760  1.3 init\n",
            // A non-numeric pcpu likewise.
            "  PID  PPID USER       RSS %CPU COMMAND\n    1     0 root  4760  n/a init\n",
            // Header only.
            "  PID  PPID USER       RSS %CPU COMMAND\n",
            "",
            "ps: unrecognized option '-eo'\n",
        ];
        for output in cases {
            let outcome = registry.preprocess(PS_COMMAND, output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn ss_reads_both_internet_and_unix_socket_shapes() {
        match PreprocessorRegistry::default().preprocess(SS_COMMAND, SS_SAMPLE) {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "ss");
                assert_eq!(
                    table.columns(),
                    ["netid", "state", "recv_q", "send_q", "local", "peer"]
                );
                assert_eq!(table.row_count(), 5);
                // Six-field internet row: address and port already joined.
                assert_eq!(
                    table.rows()[0],
                    ["tcp", "ESTAB", "0", "0", "192.0.2.2:44438", "198.51.100.7:443"]
                );
                assert_eq!(table.rows()[1][4], "0.0.0.0:22");
                assert_eq!(table.rows()[2][4], "[2001:db8::1]:8080");
                // Eight-field unix rows: the pairs are rejoined with `:`.
                assert_eq!(table.rows()[3][4], "*:896");
                assert_eq!(table.rows()[3][5], "*:0");
                assert_eq!(table.rows()[4][4], "/run/systemd/journal/stdout:21456");
            }
            other => panic!("expected a structured ss table, got {other:?}"),
        }
    }

    #[test]
    fn ss_listening_selections_are_claimed() {
        // Bare `ss` lists only non-listening sockets, so a check that wants
        // listening ports adds `-l` or `-a`. Both select which sockets are
        // listed without changing a line's shape — verified against real
        // output, where every netid still prints 6 fields (8 for unix).
        let registry = PreprocessorRegistry::default();
        for command in [
            "ss -H -n",
            "ss -H -n -l",
            "ss -H -l -n",
            "ss -H -n -a",
            "ss -a -H -n",
            "/usr/bin/ss -H -n -l",
        ] {
            assert!(
                registry.preprocess(command, SS_SAMPLE).is_structured(),
                "{command} must be claimed"
            );
        }
    }

    #[test]
    fn ss_without_the_required_flags_is_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in [
            // Without `-H` the header line would be misread as a socket.
            "ss -n",
            "ss -n -l",
            // Without `-n` names and ports resolve, so the output varies.
            "ss -H",
            "ss -H -l",
            // Flags outside the accepted set change what the columns mean.
            "ss -H -n -p",
            "ss -H -n -t",
            "ss -Hln",
        ] {
            assert_eq!(
                registry.preprocess(command, SS_SAMPLE),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn ps_rejects_special_float_values_in_pcpu() {
        // `"NaN".parse::<f64>()` succeeds, as do `inf` and `-inf`, so a
        // check that only asked whether parsing worked would have let a
        // value `ps` never prints into the numeric column.
        let registry = PreprocessorRegistry::default();
        for pcpu in ["NaN", "nan", "inf", "-inf", "infinity", "1e5", "-1.0", "+1.0", "1.2.3"] {
            let output = format!(
                "  PID  PPID USER       RSS %CPU COMMAND\n    1     0 root      4760  {pcpu} init\n"
            );
            let outcome = registry.preprocess(PS_COMMAND, &output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for pcpu {pcpu:?}, got {outcome:?}"
            );
        }
        // ... while the shapes `ps` does print stay accepted.
        for pcpu in ["0.0", "1.3", "100", "99.9"] {
            let output = format!(
                "  PID  PPID USER       RSS %CPU COMMAND\n    1     0 root      4760  {pcpu} init\n"
            );
            assert!(
                registry.preprocess(PS_COMMAND, &output).is_structured(),
                "pcpu {pcpu:?} must be accepted"
            );
        }
    }

    #[test]
    fn ss_diagnostic_line_degrades_to_raw() {
        // Real `ss -H -n` output can carry its own error line among the
        // sockets; a line that is not a socket row means the output is not
        // purely socket rows, so raw is the honest answer.
        let output = "tcp   ESTAB 0      0      192.0.2.2:44438 198.51.100.7:443\n\
                      RTNETLINK answers: Invalid argument\n";
        let outcome = PreprocessorRegistry::default().preprocess(SS_COMMAND, output);
        assert!(
            matches!(
                outcome,
                PreprocessOutcome::Raw {
                    reason: RawFallback::Unparseable(_)
                }
            ),
            "expected a raw fallback, got {outcome:?}"
        );
    }

    #[test]
    fn malformed_ss_output_degrades_to_raw() {
        let registry = PreprocessorRegistry::default();
        for output in [
            // Seven fields: neither the internet nor the unix shape.
            "tcp ESTAB 0 0 a b c\ntcp ESTAB 0 0 a b c d\n",
            // Non-numeric queue length.
            "tcp ESTAB x 0 192.0.2.2:22 192.0.2.3:1234\n",
            "",
            "ss: unrecognized option\n",
        ] {
            let outcome = registry.preprocess(SS_COMMAND, output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn journalctl_json_lines_become_log_rows() {
        match PreprocessorRegistry::default().preprocess(JOURNALCTL_COMMAND, JOURNALCTL_SAMPLE) {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "journalctl");
                assert_eq!(table.columns(), ["timestamp", "unit", "priority", "message"]);
                assert_eq!(table.row_count(), 3);
                assert_eq!(table.rows()[0][1], "ssh.service");
                assert_eq!(table.rows()[1][2], "3");
                assert_eq!(
                    table.rows()[1][3],
                    "bind() to 0.0.0.0:80 failed (98: Address already in use)"
                );
                // A kernel entry has no unit: an empty cell, not a dropped row.
                assert_eq!(table.rows()[2][1], "");
                assert_eq!(table.rows()[2][0], "1789837306500000");
            }
            other => panic!("expected a structured journalctl table, got {other:?}"),
        }
    }

    #[test]
    fn journalctl_bounded_invocations_are_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in [
            "journalctl --output=json",
            "journalctl -o json",
            "journalctl -n 100 --output=json",
            "journalctl --lines=100 --output=json",
            "journalctl --output=json --no-pager",
            "/usr/bin/journalctl -n50 -o json",
        ] {
            assert!(
                registry
                    .preprocess(command, JOURNALCTL_SAMPLE)
                    .is_structured(),
                "{command} must be claimed"
            );
        }
    }

    #[test]
    fn journalctl_other_invocations_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        for command in [
            // Another output mode is a different format entirely.
            "journalctl --output=short",
            "journalctl -o cat",
            "journalctl",
            // A free-text filter cannot be recognised word by word.
            "journalctl --output=json --since=yesterday",
            "journalctl --output=json -u ssh",
            // `-n` without a count is not the bounded form we accept.
            "journalctl -n --output=json",
            "journalctl --output=json | head",
        ] {
            assert_eq!(
                registry.preprocess(command, JOURNALCTL_SAMPLE),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn journalctl_binary_message_degrades_to_raw() {
        // journald renders a non-UTF-8 message as an array of byte values;
        // there is no faithful text cell for that, so it fails closed.
        let output = r#"{"__REALTIME_TIMESTAMP":"1789837304171000","PRIORITY":"6","MESSAGE":[104,105,0,255]}"#;
        let outcome = PreprocessorRegistry::default().preprocess(JOURNALCTL_COMMAND, output);
        assert!(
            matches!(
                outcome,
                PreprocessOutcome::Raw {
                    reason: RawFallback::Unparseable(_)
                }
            ),
            "expected a raw fallback, got {outcome:?}"
        );
    }

    #[test]
    fn malformed_journalctl_output_degrades_to_raw() {
        let registry = PreprocessorRegistry::default();
        for output in [
            // Truncation cuts the last line mid-object.
            "{\"__REALTIME_TIMESTAMP\":\"1\",\"MESSAGE\":\"a\"}\n{\"__REALTIME\n",
            // An entry with no timestamp.
            r#"{"MESSAGE":"a"}"#,
            // An array instead of JSON Lines: journalctl does not print one.
            r#"[{"__REALTIME_TIMESTAMP":"1","MESSAGE":"a"}]"#,
            // Empty, as on a host with no journal at all.
            "",
            "-- No entries --\n",
        ] {
            let outcome = registry.preprocess(JOURNALCTL_COMMAND, output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn ip_json_flattens_addresses_into_rows() {
        match PreprocessorRegistry::default().preprocess(IP_COMMAND, IP_SAMPLE) {
            PreprocessOutcome::Structured { preprocessor, table } => {
                assert_eq!(preprocessor, "ip");
                assert_eq!(
                    table.columns(),
                    ["ifname", "ifindex", "operstate", "family", "address"]
                );
                // Two addresses on lo, one row for the address-less ifb0, one
                // for eth0.
                assert_eq!(table.row_count(), 4);
                assert_eq!(
                    table.rows()[0],
                    ["lo", "1", "UNKNOWN", "inet", "127.0.0.1/8"]
                );
                assert_eq!(table.rows()[1], ["lo", "1", "UNKNOWN", "inet6", "::1/128"]);
                // An interface with no addresses keeps its row, and its state.
                assert_eq!(table.rows()[2], ["ifb0", "2", "DOWN", "", ""]);
                assert_eq!(table.rows()[3][4], "192.0.2.2/24");
            }
            other => panic!("expected a structured ip table, got {other:?}"),
        }
    }

    #[test]
    fn ip_incomplete_records_are_rejected() {
        // Every field this table renders must be present and of the shape
        // iproute2 documents; an absent one is refused rather than filled
        // with an empty cell that would read as "none".
        let registry = PreprocessorRegistry::default();
        for output in [
            // `addr_info` absent entirely: this is `ip -j link` output, not
            // `ip addr`, and must not become an address-less address table.
            r#"[{"ifname":"lo","ifindex":1,"operstate":"UP"}]"#,
            r#"[{"ifname":"lo","ifindex":1,"operstate":"UP","addr_info":null}]"#,
            // `operstate` absent (iproute2 would send `operstate_index`).
            r#"[{"ifname":"lo","ifindex":1,"addr_info":[]}]"#,
            // `family` absent (iproute2 would send `family_index`).
            r#"[{"ifname":"lo","ifindex":1,"operstate":"UP","addr_info":[{"local":"127.0.0.1","prefixlen":8}]}]"#,
            // `prefixlen` absent, or a string rather than a number: either
            // would leave this row's address in a different format from its
            // neighbours'.
            r#"[{"ifname":"lo","ifindex":1,"operstate":"UP","addr_info":[{"family":"inet","local":"127.0.0.1"}]}]"#,
            r#"[{"ifname":"lo","ifindex":1,"operstate":"UP","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":"8"}]}]"#,
            // `local` as a number is not an address iproute2 would print.
            r#"[{"ifname":"lo","ifindex":1,"operstate":"UP","addr_info":[{"family":"inet","local":1,"prefixlen":8}]}]"#,
        ] {
            let outcome = registry.preprocess(IP_COMMAND, output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn malformed_ip_output_degrades_to_raw() {
        let registry = PreprocessorRegistry::default();
        for output in [
            r#"[{"ifindex":1,"operstate":"UP","addr_info":[]}]"#,
            r#"[{"ifname":"lo","operstate":"UP","addr_info":[]}]"#,
            r#"[{"ifname":"lo","ifindex":1,"operstate":"UP","addr_info":"none"}]"#,
            r#"[{"ifname":"lo","ifindex":1,"operstate":"UP","addr_info":[{"family":"inet","prefixlen":8}]}]"#,
            "[]",
            "",
            "/bin/sh: ip: not found\n",
            "Object \"addr\" is unknown, try \"ip help\".\n",
        ] {
            let outcome = registry.preprocess(IP_COMMAND, output);
            assert!(
                matches!(
                    outcome,
                    PreprocessOutcome::Raw {
                        reason: RawFallback::Unparseable(_)
                    }
                ),
                "expected a raw fallback for {output:?}, got {outcome:?}"
            );
        }
    }

    #[test]
    fn non_canonical_service_family_commands_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        let cases = [
            // Other output modes / no explicit format.
            ("systemctl list-units", SYSTEMCTL_SAMPLE),
            ("systemctl list-units --output=short", SYSTEMCTL_SAMPLE),
            ("systemctl status nginx", SYSTEMCTL_SAMPLE),
            // A different ps field list is a different column layout.
            ("ps aux", PS_SAMPLE),
            ("ps -eo pid,comm", PS_SAMPLE),
            ("ps -eo pid,ppid,user,rss,pcpu,args", PS_SAMPLE),
            // `ss` without `-H` prints a header this parse would misread.
            ("ss -n", SS_SAMPLE),
            ("ss -tuln", SS_SAMPLE),
            // `ip` in another object or without `-j`.
            ("ip addr", IP_SAMPLE),
            ("ip -j link", IP_SAMPLE),
            // Shell syntax in the program word, and compound forms.
            ("$(which ps) -eo pid,ppid,user,rss,pcpu,comm", PS_SAMPLE),
            ("ss -H -n | wc -l", SS_SAMPLE),
            ("systemctl list-units --output=json > /tmp/u", SYSTEMCTL_SAMPLE),
        ];
        for (command, output) in cases {
            assert_eq!(
                registry.preprocess(command, output),
                PreprocessOutcome::Raw {
                    reason: RawFallback::NoPreprocessor
                },
                "{command} must not be claimed"
            );
        }
    }

    #[test]
    fn service_family_adversarial_outputs_do_not_panic() {
        let registry = PreprocessorRegistry::default();
        let commands = [
            SYSTEMCTL_COMMAND,
            PS_COMMAND,
            SS_COMMAND,
            JOURNALCTL_COMMAND,
            IP_COMMAND,
        ];
        let tough = [
            "",
            "\n\n\n",
            "   ",
            "{",
            "[",
            "[]",
            "null",
            "[null]",
            "[[]]",
            "{\"a\":1}",
            "\u{0}\u{0}",
            "PID\n",
            "tcp\n",
            "пример вывода\n",
            "[{\"ifname\":null,\"ifindex\":null}]",
        ];
        for command in commands {
            for output in tough {
                // Any outcome is acceptable here — the point is no panic.
                let _ = registry.preprocess(command, output);
            }
        }
        let huge_line = format!("tcp ESTAB 0 0 {} b\n", "x".repeat(20_000));
        let _ = registry.preprocess(SS_COMMAND, &huge_line);
        let deep = format!("[{}]", "[".repeat(200) + &"]".repeat(200));
        let _ = registry.preprocess(IP_COMMAND, &deep);
    }
}
