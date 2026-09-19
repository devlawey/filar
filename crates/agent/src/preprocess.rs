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
//! devices (`df`, `lsblk`, issue #421) and packages and application versions
//! (`dpkg-query`, `rpm`, `nginx -v`, `php -v`, issue #422). Further families
//! and the agent-loop / fleet integration come in later issues.

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
/// tab-separated, one package per line. The version field carries
/// `%{RELEASE}` as well, because on RPM distributions the release is where
/// the distribution's own patch level lives (`1.20.1-14.el9`) — comparing
/// bare `%{VERSION}` across a fleet would call two different builds equal.
const RPM_CANONICAL_ARGS: &str = r"-qa --qf='%{NAME}\t%{VERSION}-%{RELEASE}\n'";

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
fn is_canonical_invocation(command: &str, program: &str, args: &str) -> bool {
    let mut words = command.split_whitespace();
    let Some(first) = words.next() else {
        return false;
    };
    if base_name(first) != program {
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
fn parse_version_banner<'a>(program: &str, line: &'a str) -> Option<&'a str> {
    match program {
        // `nginx version: nginx/1.24.0`, sometimes with a `(Ubuntu)` suffix.
        "nginx" => leading_version(line.split("nginx/").nth(1)?),
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
        assert_eq!(registry.len(), 5);
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
    const RPM_COMMAND: &str = r"rpm -qa --qf='%{NAME}\t%{VERSION}-%{RELEASE}\n'";

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

    /// Real RHEL 9 output: `%{VERSION}-%{RELEASE}` with `.el9` releases and
    /// a module-build release.
    const RPM_SAMPLE: &str = "\
bash\t5.1.8-9.el9
glibc\t2.34-100.el9_4.2
nginx\t1.20.1-20.el9_4.1
openssl-libs\t3.0.7-27.el9
util-linux\t2.37.4-18.el9
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
                assert_eq!(table.row_count(), 5);
                assert_eq!(table.rows()[1], ["glibc", "2.34-100.el9_4.2"]);
                assert_eq!(table.rows()[2], ["nginx", "1.20.1-20.el9_4.1"]);
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
        let rpm_by_path = r"/usr/bin/rpm -qa --qf='%{NAME}\t%{VERSION}-%{RELEASE}\n'";
        assert!(
            registry.preprocess(rpm_by_path, RPM_SAMPLE).is_structured(),
            "a path-qualified rpm must still be claimed"
        );
    }

    #[test]
    fn non_canonical_package_commands_are_not_claimed() {
        let registry = PreprocessorRegistry::default();
        let cases: [(&str, &str); 8] = [
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
}
