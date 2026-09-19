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
//! fallback and one built-in reference preprocessor (`df`) proving the path
//! end to end. Command families (disk, packages, services) and the
//! agent-loop / fleet integration come in later issues.

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

/// Reference preprocessor for GNU coreutils `df`.
///
/// Claims `df` only as a single simple command — a pipeline, a compound
/// segment or a redirect has output that is not a pure `df` table. Options
/// that change the column set are not claimed either: inode mode
/// (`-i`/`--inodes`, and `i` in a short cluster), type mode
/// (`-T`/`--print-type`) and custom field lists (`--output`). Everything else
/// — filters (`-x`, `-t`, `-l`, `-a`), display sizes (`-h`, `-H`, `-k`,
/// `-B`), `-P` — keeps the six-column shape and is accepted.
///
/// The parse is strict by design: the header must start with `Filesystem`,
/// every data line must have at least six whitespace-separated fields with
/// the use-percent field ending in `%` (GNU prints `-` for filesystems that
/// have no size) and the mount field starting with `/`. The mount point is
/// the last field group ([`fields[5..]`] joined), so mount points with spaces
/// survive. Any mismatch — truncation cuts mid-line, another locale or
/// `df` flavour — returns an error and the caller degrades to raw text.
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
fn option_changes_format(token: &str) -> bool {
    if let Some(long) = token.strip_prefix("--") {
        let name = long.split('=').next().unwrap_or(long);
        return matches!(name, "inodes" | "print-type" | "output");
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
        assert_eq!(registry.len(), 1);
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
        for command in ["df -T", "df -hT", "df --print-type", "df --output=source,size"] {
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
}
