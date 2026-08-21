# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The engine crates (`filar-core`, `filar-transport`, `filar-agent`) are consumed
by external projects via git tags; `engine-v0.3.0` is the first stable
dependency point for embedders (see `docs/ENGINE_API.md`).

## [Unreleased]

### Changed

- Password/TTY failure guidance shortened to a compact Ctrl+P /
  `$FILAR_SECRET_N` / `sudo -S` hint in command output
  ([#331](https://github.com/devlawey/filar/issues/331)).

## [1.0.2] - 2026-08-21

### Added

- Local / air-gapped OpenAI-compatible models: empty `key_env` means keyless
  (no `Authorization` header; redirects disabled); GUI hints for ollama URL and
  keyless status; clearer tool-calling and LLM timeout errors; README section
  ([#320](https://github.com/devlawey/filar/issues/320)).
- Agent rejects long wall-clock waits (`sleep` / `Start-Sleep` ≥ 30s) before
  execute and steers toward background + short poll (or Ctrl+T); timeout errors
  include the same guidance
  ([#323](https://github.com/devlawey/filar/issues/323)).

### Fixed

- Confirm modal clamps/truncates oversized commands so long heredocs no longer
  panic the TUI buffer; `PanicHookGuard` skips `take_hook` while unwinding
  ([#324](https://github.com/devlawey/filar/issues/324)).
- Chat scroll fully repaints the viewport (cell reset + width padding) so
  shorter lines no longer leave glyph artifacts from previous frames
  ([#325](https://github.com/devlawey/filar/issues/325)).
- Allowlist never auto-approves `sudo`/`su`/`doas` or write-form `sysctl`;
  Unix local agent children `setsid` so password prompts cannot overwrite the
  TUI; agent steers to Ctrl+P / `$FILAR_SECRET_N` + `sudo -S`
  ([#329](https://github.com/devlawey/filar/issues/329)).

## [1.0.1] - 2026-08-19

### Changed

- SSH status bar shows **alias** (or host-only when there is no alias),
  **host**, and last-known **pwd**. Local tabs still show `local` plus the
  process cwd. Interactive OSC 7 updates pwd; agent↔PTY sync is #313
  ([#309](https://github.com/devlawey/filar/issues/309)).
- Default command timeout is now 5 minutes (`[timeouts].command_secs = 300`).
  The value is applied to SSH marker wait and local subprocess execution, so
  long jobs such as `du`/`find` are no longer killed at 120s (SSH) or 60s
  (local) under default settings
  ([#308](https://github.com/devlawey/filar/issues/308)).

### Fixed

- GUI launcher: paste into API key / SSH password no longer keeps a trailing
  newline (or the space egui 0.29 substituted for it). Show-password /
  show-API-key toggles were added
  ([#312](https://github.com/devlawey/filar/issues/312)).
- Interactive terminal (Ctrl+T): drag-select copies to the clipboard like
  agent mode when the PTY has not requested mouse tracking. Apps that enable
  SGR/legacy mouse still receive events; Ctrl+C is unchanged
  ([#311](https://github.com/devlawey/filar/issues/311)).
- F1 help overlay no longer uses `⌘` on Windows (console fonts render it as
  `?`); macOS keeps Fn+F1 / Ctrl vs ⌘ wording
  ([#310](https://github.com/devlawey/filar/issues/310)).
- Interactive `cd` (Ctrl+T) is applied to the agent executor on return, and
  entering interactive starts the PTY in the tab cwd (local spawn cwd; SSH
  sends `cd`). Status-bar pwd tracks OSC 7, a POSIX leave-probe, and SSH
  `$PWD` in the command marker
  ([#313](https://github.com/devlawey/filar/issues/313)).

## [1.0.0] - 2026-08-17

### Added

- macOS 1.0.0 packaging decision: **binary-only** (not `.app` / not notarized);
  unsigned OSS policy aligned with Windows SmartScreen (#80); README and
  PLATFORM_NOTES quarantine / download notes
  ([#297](https://github.com/devlawey/filar/issues/297)).
- Dual-platform docs for 1.0.0: README badge and Getting Started cover
  Windows + macOS; USER_GUIDE Credential Manager / Keychain; SMOKE and
  PLATFORM_NOTES index findings from #289–#293
  ([#295](https://github.com/devlawey/filar/issues/295)).
- macOS keyboard notes: Ctrl (not ⌘), Fn+F1 for help, smoke checks, and
  help-overlay hints
  ([#292](https://github.com/devlawey/filar/issues/292)).

### Changed

- `prepare-release` skill: preflight accepts Windows + macOS from `release.yml`;
  `all` expects both jobs/assets; release notes list both binary names
  ([#296](https://github.com/devlawey/filar/issues/296)).
- Local interactive PTY (Ctrl+T) on Unix/macOS uses `$SHELL` when set to an
  existing file, otherwise `sh`; Windows remains `cmd.exe`. Agent
  `LocalExecutor` is unchanged (`sh -c` / PowerShell)
  ([#293](https://github.com/devlawey/filar/issues/293)).
- Desktop data directory now uses `dirs::data_dir()`:
  Windows `%APPDATA%\filar\`, macOS `~/Library/Application Support/filar/`,
  Linux `~/.local/share/filar/` (or `$XDG_DATA_HOME`). Legacy Unix
  `$HOME/filar` is migrated once on startup when the new path is empty
  ([#291](https://github.com/devlawey/filar/issues/291)).
- Release CI now builds and attaches both Windows (`*-windows-x86_64.exe`)
  and macOS (`*-macos-aarch64`) binaries to the same GitHub Release
  ([#289](https://github.com/devlawey/filar/issues/289)).

### Fixed

- GUI→TUI SSH password handoff now reads the OS keyring under
  `ssh_target:{alias|SSHn}` (same key the launcher writes), instead of the
  legacy `ssh{slot}` name that never matched
  ([#290](https://github.com/devlawey/filar/issues/290)).
- Docs: log file path documented as `{app data}/filar/logs/filar.log`
  (the `logs/` segment was missing in README)
  ([#291](https://github.com/devlawey/filar/issues/291)).

## [0.9.0] - 2026-08-15

### Added

- Ctrl+S session export: saves the current chat session as a Markdown file
  in the working directory. Russian layout (Ctrl+ы) also supported.
  A progress overlay shows during the save, and a toast confirms completion.
  (`save_in_flight` guard prevents concurrent saves.)
   ([#232](https://github.com/devlawey/filar/issues/232),
    [#233](https://github.com/devlawey/filar/issues/233),
    [#234](https://github.com/devlawey/filar/issues/234),
    [#235](https://github.com/devlawey/filar/issues/235)).
- Smooth progress animation during session save: the progress bar now
  shows intermediate states (0% → 50% → 100%) with brief pauses,
  instead of jumping instantly to completion.
  ([#240](https://github.com/devlawey/filar/issues/240)).
- Configurable save directory for session exports: `save_dir` in
  `config.toml` sets where Ctrl+S writes `.md` files (default: working
  directory). The GUI launcher gained a "Save directory" field with a
  Browse folder picker.
  ([#247](https://github.com/devlawey/filar/issues/247)).
- Explain (safe mode) confirm mode: `CommandConfirmMode::Explain` requires
  every command to have a mandatory `explanation` from the model. In this
  mode, `explanation` is added to the `required` array in all tool schemas,
  a SAFE MODE block is appended to the system prompt, and all commands
  (including read-only) require confirmation. Missing explanations are
  rejected with a retry limit of 2.
  ([#262](https://github.com/devlawey/filar/issues/262)).
- F2 key toggles Explain (safe mode) on/off at runtime, per-tab. If a
  confirmation is pending when F2 is pressed, it is aborted (denied).
  The status bar highlights Explain mode with an accent color.
  ([#263](https://github.com/devlawey/filar/issues/263)).
- Automatic Markdown session transcript in Explain mode: all commands,
  explanations, outputs, and denials are written to a single `.md` file
  in the configured save directory. The file is overwritten on each save.
  Errors are shown once per session without blocking the agent.
  ([#264](https://github.com/devlawey/filar/issues/264)).
- F2 visible in the help overlay (`F1`) and bottom help bar. README documents
  all four confirmation modes (`always`, `allowlist`, `never`, `explain`) and
  F2 in the keyboard shortcuts table. `docs/SMOKE.md` includes a safe-mode
  checklist.
  ([#265](https://github.com/devlawey/filar/issues/265)).
- Session launch context: saved sessions now record `ssh_info`, `model`,
  `api_base_url`, and `confirm_mode`. On restore (`--session`), the saved
  SSH host is surfaced in the tab label and the saved confirm mode is
  re-applied.
  ([#271](https://github.com/devlawey/filar/issues/271)).
- Periodic session auto-save: the active session is persisted every 30
  seconds (only when it changed) and once more from the panic hook, so
  closing the window or killing the process loses at most ~30 seconds of
  history. Each run reuses a single session id, and pruning keeps at most 10
  session files. Session writes are now atomic (temp file + rename), so a
  crash mid-write cannot corrupt a saved session.
  ([#272](https://github.com/devlawey/filar/issues/272)).
- F3 session selection overlay: restore a saved session from within the TUI.
  The overlay lists saved sessions (date, host, profile, preview); Enter
  restores messages/history/profile/tokens and re-initiates the SSH
  connection (password prompt via Ctrl+P), Esc cancels. F3 is shown in the
  bottom help bar and the F1 help overlay.
  ([#273](https://github.com/devlawey/filar/issues/273)).
- GUI launcher: clicking a saved session now auto-selects the matching SSH
  target (by host:port) and LLM profile (by name), and fills the Model / API
  base URL fields from the session's launch context. The session list shows
  the SSH host and model.
  ([#274](https://github.com/devlawey/filar/issues/274)).

### Fixed

- Explain mode transcript: each F2 toggle cycle now creates a new `.md`
  file. Previously the same file was reused across toggle cycles because
  `transcript_path` was never cleared on exit.
  ([#275](https://github.com/devlawey/filar/issues/275)).
- Transcript date now uses local time with timezone offset (via `chrono`).
  The initial "Connected to" message no longer includes the mode.
  ([#277](https://github.com/devlawey/filar/issues/277)).
- Safe mode (Explain) activation/deactivation messages added to the chat feed
  and transcript: "Safe mode (Explain) activated. Transcript: {path}" on F2
  entry, "Safe mode (Explain) deactivated" on exit.
  ([#277](https://github.com/devlawey/filar/issues/277)).

### Changed

- Session-save directory, LLM profiles and SSH targets are now passed from
  the GUI launcher to the TUI via `pending_launch.json`. The GUI no longer
  writes these launch-specific sections to `config.toml`; they remain only
  as a fallback for direct (non-GUI) TUI launches.
  ([#255](https://github.com/devlawey/filar/issues/255)).
- `tool_definitions()` now takes a `CommandConfirmMode` argument to produce
  mode-aware tool schemas. External engine consumers must update calls.
  ([#262](https://github.com/devlawey/filar/issues/262)).

### Fixed

- PowerShell error output (stderr) on Windows now displays correctly in
  UTF-8 — redirected to stdout via `2>&1` (local Windows executor only)
  ([#243](https://github.com/devlawey/filar/issues/243)).
- Fixed visual artifact in Thinking mode where the mode badge yellow
  background would bleed into adjacent cells during spinner animation
  ([#245](https://github.com/devlawey/filar/issues/245)).
- PowerShell output (including Russian error messages) on Windows now
  encodes as UTF-8 via `[Console]::OutputEncoding`, replacing the
  ineffective `chcp 65001` (which .NET ignores for piped output)
  ([#253](https://github.com/devlawey/filar/issues/253)).
- Fixed panic when agent output was truncated in the middle of a multibyte
  (e.g. Cyrillic) character — truncation is now by characters, not bytes
  ([#260](https://github.com/devlawey/filar/issues/260)).
- F3 session restore: selecting a saved SSH session no longer shows the
  remote host in the status bar before the connection is actually
  established. When the saved host matches a configured SSH target, the tab
  reconnects using that target's stored credentials (OS keyring /
  `SSH_PASSWORD` environment), falling back to a password prompt if none are
  available; otherwise it opens the password prompt and stays on its previous
  connection until the reconnect completes
  ([#287](https://github.com/devlawey/filar/issues/287)).

## [0.8.6] - 2026-08-02

### Fixed

- Russian characters in local command output on Windows are no longer
  displayed as question marks — `chcp 65001` (UTF-8) is now prepended to
  every PowerShell command
  ([#229](https://github.com/devlawey/filar/issues/229)).
- Visual artifacts when expanding/collapsing command blocks in chat are
  fixed — the chat area is now cleared before re-rendering
  ([#228](https://github.com/devlawey/filar/issues/228)).

## [0.8.5] - 2026-08-02

### Fixed

- Launcher-generated `[[ssh_targets]]` now replace ALL previous targets on every
  save, eliminating stale entries like `prod-web` with old addresses
  ([#222](https://github.com/devlawey/filar/issues/222)).
- Ctrl+O targets now use the correct authentication type: password-based
  authentication when `save_password` is set in the launcher, key-based
  otherwise ([#220](https://github.com/devlawey/filar/issues/220)).
- The status bar no longer shows "Connected to: local" after a failed
  `Ctrl+O` connection — error is in the chat, not the transport label
  ([#221](https://github.com/devlawey/filar/issues/221)).
- SSH password keyring key now matches between the launcher and the TUI
  runner, so saved passwords are found on `Ctrl+O` without re-prompting
  ([#226](https://github.com/devlawey/filar/issues/226)).

## [0.8.4] - 2026-08-02

### Fixed

- `merge_ssh_targets` now properly removes stale launcher targets by host match
  and no longer duplicates slot names when an alias is set, eliminating leftover
  targets like `prod-web` and `SSH1` from the `Ctrl+O` overlay
  ([#214](https://github.com/devlawey/filar/issues/214)).

## [0.8.2] - 2026-08-02

### Fixed

- SSH profiles configured in the GUI launcher are now synced to
  `[[ssh_targets]]` in `config.toml`, so `Ctrl+O` sees them
  ([#210](https://github.com/devlawey/filar/issues/210)).
- README and SMOKE.md now document the launcher → `config.toml` synchronisation
  flow for SSH targets ([#211](https://github.com/devlawey/filar/issues/211)).

## [0.8.1] - 2026-08-01

### Changed

- `Ctrl+O` now opens a visual host-selection overlay instead of cycling targets
  instantly. The overlay lists `local` plus all `[[ssh_targets]]` with navigation
  via arrow keys, `Enter` to select, and `Esc` to cancel
  ([#206](https://github.com/devlawey/filar/issues/206)).

### Changed

- `^O` hint bar and F1 reference now describe the host-selection overlay; README
  and SMOKE.md updated to match
  ([#207](https://github.com/devlawey/filar/issues/207)).

## [0.8.0] - 2026-08-01

### Added

- `Ctrl+O` cycles through SSH targets defined in `config.toml` (plus `local`),
  showing the target alias in the status bar and reconnecting only the active tab
  ([#200](https://github.com/devlawey/filar/issues/200)).
- SSH targets using password authentication are now supported by `Ctrl+O`, with
  the password resolved from the OS credential store, environment, or an
  interactive prompt ([#201](https://github.com/devlawey/filar/issues/201)).

### Changed

- `^O` now appears in the hint bar and F1 reference, and the README covers
  `[[ssh_targets]]`, host cycling, and password storage
  ([#202](https://github.com/devlawey/filar/issues/202)).

## [0.7.4] - 2026-07-31

### Fixed

- Token usage and the served model slug were attributed to the startup profile
  instead of the active one, because ordinary message sends did not record the
  pending profile ([#198](https://github.com/devlawey/filar/issues/198)).

## [0.7.3] - 2026-07-30

### Fixed

- The profile selected in the launcher is now actually used: sessions no longer always
  start on the first profile in `config.toml`, and the first `Ctrl+L` press no longer
  goes to waste ([#194](https://github.com/devlawey/filar/issues/194)).
- The status bar now follows the active LLM profile: the model slug and token usage
  update immediately on `Ctrl+L` instead of lagging until the next response
  ([#195](https://github.com/devlawey/filar/issues/195)).

## [0.7.2] - 2026-07-29

### Fixed

- The bottom hint bar now shows `F1`, so the full command reference is discoverable
  without reading the README ([#191](https://github.com/devlawey/filar/issues/191)).

### Added

- Session cost is now taken from OpenRouter's per-request `usage.cost`, tokens are tracked
  per LLM profile, and the actually served model slug is shown in the status bar
  ([#190](https://github.com/devlawey/filar/issues/190)).

## [0.7.1] - 2026-07-28

### Fixed

- LLM client construction passed the resolved API key where a secret *name* was
  expected, so every agent request failed and the key value was shown in the error
  message ([#183](https://github.com/devlawey/filar/issues/183)).
- Secret lookup failures no longer embed the looked-up name in the error message,
  closing a path that leaked an API key into the UI
  ([#184](https://github.com/devlawey/filar/issues/184)).

### Changed

- Engine API consistency: `SessionMeta.llm_profile` now matches `Session.llm_profile`
  (`Option<String>`), and `KeyringSecretProvider` is re-exported alongside the other
  secret providers ([#181](https://github.com/devlawey/filar/issues/181)).
- Contributor docs now require a manual smoke run of the built binary before closing
  user-facing issues, with a checklist in `docs/SMOKE.md`
  ([#185](https://github.com/devlawey/filar/issues/185)).

## [0.7.0] - 2026-07-27

### Added

- GUI launcher now has a Models tab with add/delete profile management; each profile
  stores its API key in the OS credential store independently
  ([#162](https://github.com/devlawey/filar/issues/162)).
- Ctrl+L now cycles through LLM profiles per tab; each session can use a different
  model without affecting other tabs
  ([#163](https://github.com/devlawey/filar/issues/163)).
- Clipboard paste now works via Ctrl+V and bracketed paste in agent input, interactive
  terminal, and password prompt
  ([#153](https://github.com/devlawey/filar/issues/153)).
- Token usage counter shown in the status bar (v1: character-length estimation) per
  session ([#164](https://github.com/devlawey/filar/issues/164)).
- `config.toml` is now searched in `%APPDATA%\filar\` first (unified config location),
  then CWD, then next to the executable
  ([#161](https://github.com/devlawey/filar/issues/161)).

### Changed

- `config.toml` search order: CWD before app-data (local `./config.toml` now
  overrides `%APPDATA%\filar\`)
  ([#175](https://github.com/devlawey/filar/issues/175)).
- README updated with all 0.7.0 features: Models tab, Ctrl+L, token counter,
  config priority, key storage
  ([#175](https://github.com/devlawey/filar/issues/175)).
- Engine API: `SessionMeta.llm_profile` now matches `Session.llm_profile`
  (`Option<String>`), and `KeyringSecretProvider` is re-exported alongside the
  other secret providers ([#181](https://github.com/devlawey/filar/issues/181)).

### Fixed

- API keys and SSH passwords are no longer written in plain text to
  `pending_launch.json`; secrets are read from the OS credential store instead
  ([#159](https://github.com/devlawey/filar/issues/159)).
- `Settings::save` and `save_pending_launch` now create their parent directory before
  writing, preventing silent data loss when `%APPDATA%\filar` doesn't exist
  ([#160](https://github.com/devlawey/filar/issues/160)).
- Switching LLM profiles with Ctrl+L now resolves the profile's API key from the OS
  credential store, not just the in-memory key of the launched profile
  ([#171](https://github.com/devlawey/filar/issues/171)).
- Profile names and credential entries are now unique: deleting a profile no longer
  leaves its API key in the OS credential store, and adding a profile after a deletion
  no longer collides with an existing one
  ([#172](https://github.com/devlawey/filar/issues/172)).
- Token counter now uses real API usage data instead of character-length estimation,
  and shows `—` when the provider doesn't report usage
  ([#173](https://github.com/devlawey/filar/issues/173)).
- Session persistence now saves and restores the selected LLM profile and accumulated
  token counters ([#174](https://github.com/devlawey/filar/issues/174)).

## [0.6.2] - 2026-07-25

### Added

- Clipboard paste now works via Ctrl+V and bracketed paste in agent input, interactive
  terminal, and password prompt
  ([#153](https://github.com/devlawey/filar/issues/153)).

### Fixed

- The F1 help overlay is now scrollable (PgUp/PgDn/arrows) instead of clipping entries
  that don't fit a small terminal window
  ([#151](https://github.com/devlawey/filar/issues/151)).
- The close-tab shortcut (^W) is now shown in the bottom hint bar, not only in the F1
  overlay ([#152](https://github.com/devlawey/filar/issues/152)).
- Toggling the interactive view with Ctrl+T now redraws immediately instead of leaving a
  stale frame until the next event
  ([#154](https://github.com/devlawey/filar/issues/154)).

## [0.6.1] - 2026-07-25

### Added

- Help overlay listing every shortcut and command, opened with `F1` (Ctrl+H tested
  and found indistinguishable from Backspace on Windows Terminal)
  ([#142](https://github.com/devlawey/filar/issues/142)).
- Agent-mode input history is now saved with the session and restored on reopen, so
  Up/Down recalls previous prompts
  ([#143](https://github.com/devlawey/filar/issues/143)).

### Changed

- README updated for v0.6.1: per-tab connections, `!ssh` scope, help overlay, persisted
  agent input history ([#144](https://github.com/devlawey/filar/issues/144)).

### Fixed

- Command executors are now per session: a new tab always starts local, and `!ssh`
  reconnects only its own tab instead of swapping the connection for every tab; the
  interactive terminal now opens on the tab's current host
  ([#140](https://github.com/devlawey/filar/issues/140)).
- Session tab labels now show the tab's actual target (`user@host`) instead of always
  reading `local-N`
  ([#141](https://github.com/devlawey/filar/issues/141)).

## [0.6.0] - 2026-07-23

### Changed

- Interactive terminal backends are now stored per `SessionId` in the runner
  (internal refactor, no behavior change) preparing per-tab persistent terminals
  ([#113](https://github.com/devlawey/filar/issues/113)).
- Interactive terminals are now persistent per tab: switching tabs no longer
  closes the terminal, and Ctrl+T toggles the view without killing the PTY
  (supersedes the 0.5.1 exit-on-switch behavior)
  ([#115](https://github.com/devlawey/filar/issues/115)).

### Added

- Background terminal output marks its tab with a new-output indicator that
  clears on switch; docs updated for persistent per-tab terminals
  ([#118](https://github.com/devlawey/filar/issues/118)).

- Per-terminal reader tasks feed a tagged channel so every interactive backend
  (including background tabs) is drained and routed to its own session model
  ([#114](https://github.com/devlawey/filar/issues/114)).

### Fixed

- Per-tab interactive terminals are torn down on tab close and app exit, and
  background EOF/errors retire a tab's terminal without disturbing the active
  tab ([#116](https://github.com/devlawey/filar/issues/116)).
- Window resize now propagates to every live per-tab terminal (model and backend),
  not just the active one, so background terminals stay correctly sized
  ([#117](https://github.com/devlawey/filar/issues/117)).
- Interactive scrollbar now responds to mouse drag; it was previously only
  controllable via PgUp/PgDn keys
  ([#119](https://github.com/devlawey/filar/issues/119)).
- Fixed visual artifacts (stale text, status-bar fragments) when switching between
  session tabs, especially from interactive to agent views and after Ctrl+Z
  ([#120](https://github.com/devlawey/filar/issues/120)).
- `Ctrl+N` (new tab) and `Ctrl+W` (close tab) now work from interactive terminal
  mode; previously they were forwarded to the PTY and ignored
  ([#121](https://github.com/devlawey/filar/issues/121)).

## [0.5.1] - 2026-07-22

### Fixed

- Interactive terminal PTY/grid was sized 2 rows too tall (chrome = 4 lines, not 2),
  hiding the shell prompt below the viewport until the window was maximized
  ([#107](https://github.com/devlawey/filar/issues/107)).
- Interactive scrollback did not render: the grid was drawn from the live screen
  ignoring `display_offset`, so wheel/PgUp scrolling had no visible effect
  ([#108](https://github.com/devlawey/filar/issues/108)).
- Tab navigation was dead in interactive terminal mode; `Ctrl+Tab`/`Ctrl+Shift+Tab`/
  `BackTab`/`Ctrl+PageUp`/`Ctrl+PageDown` now switch tabs (leaving the terminal
  first) when more than one tab is open
  ([#109](https://github.com/devlawey/filar/issues/109)).

## [0.5.0] - 2026-07-21

Milestone v0.5.0 — hotfix интерактивного режима (select! starvation, скроллбар,
scrollback) и доработки UX (вкладки сессий, алиасы SSH-таргетов, тёмная тема
лаунчера).

### Added

- Session tabs: `Ctrl+N` — новая вкладка (local), `Ctrl+W` — закрыть,
  `Ctrl+Tab`/`Ctrl+1..9` — переключение. Tab bar над status bar. Session struct
  с Deref-паттерном для обратной совместимости (#96).
- SessionId и per-session диспетчеризация событий агента. Activity-индикаторы
  на ярлыках вкладок (`●` — агент работает, `?` — ожидание подтверждения,
  `○` — новые сообщения) (#103).
- Interactive scrollback: PgUp/PgDn и колесо мыши листают историю терминала.
  Скроллбар с корректной математикой (#93, #95).
- Scrollbar position fix: content_length = total − viewport, ползунок доходит
  до низа (#94).
- Launcher: поле alias для SSH-таргетов, сохранение в settings.json (#97).
- Launcher: тёмная тема (accent #3db3b3) и фиксированные кнопки Launch/Cancel
  (TopBottomPanel::bottom + ScrollArea) (#98).

### Fixed

- Interactive режим не перерисовывался: read_output голодил рендер в select!.
  Добавлен принудительный кадр после итерации цикла (#93).

## [0.4.0] - 2026-07-16

Milestone v0.4.0 — flexibility of LLM choice and measurability of its quality on
filar's own tasks.

### Changed

- Renamed the LLM client `GlmClient` → `OpenAiCompatClient` (module `glm` →
  `openai_compat`); filar works with any OpenAI-compatible endpoint, not just
  GLM. `GlmClient` stays as a deprecated re-export alias for back-compat (#71).
- Agent system prompt: rules are now separated by newlines for readability
  (previously concatenated without spacing) (#72).

### Added

- Configurable LLM request parameters — `temperature`, `top_p`, and `extra_body`
  on `LlmConfig`/`LlmProfile` (with validation and GUI launcher fields) (#70).
- README "Choosing an LLM" section with a verified-providers table and
  OpenAI-compatibility notes; `docs/ENGINE_API.md` local-model example and
  `key_env` override note (#71).
- `eval/` harness (promptfoo config, synced agent system prompt, tool-call
  asserts) for comparing LLMs on filar tasks (#72).
- Starter eval dataset — 30 anonymised cases (operations / safety / language)
  with a three-model comparison report (#73).
- `eval-smoke` CI regression workflow — a 10-case subset, ≥90% threshold with one
  retry, triggered on prompt/agent/dataset changes (#74).

## [0.3.1] - 2026-07-14

### Added

- SSH keepalive plus one silent reconnect-and-retry when an idle session is
  dropped before a command is dispatched (#58).
- "copied" toast in the status bar after a mouse-selection copy (#59).

### Changed

- TUI logs are written to a rotating file and WARN/ERROR events are mirrored
  into the chat, instead of being printed to the terminal and corrupting the UI
  (#57).
- Global hotkeys reworked: quit is `^Q`, cancel the agent's work is `^Z`, and
  `^C` is now a no-op to avoid accidental exits (both with ЙЦУКЕН equivalents)
  (#60).
- SSH password for password auth is resolved through the `SecretProvider`
  (`SSH_PASSWORD`) instead of a direct environment read, so engine embedders can
  inject it; TUI/desktop keep the `SSH_PASSWORD` env behaviour via the default
  `EnvSecretProvider` (#61).

## [0.3.0] - 2026-07-09

First release with a public engine API. The `engine-v0.3.0` tag is the intended
dependency point for external consumers (bots, mobile, FFI).

### Added

- Public engine API (Phase 0) exposing the `filar-core`, `filar-transport` and
  `filar-agent` crates for external consumers, documented in
  `docs/ENGINE_API.md` (#47).
- `AgentEvent` + `EventSink` to observe an agent turn, and a `ChatResponse`
  struct return type (#43).
- Streaming responses through the `LlmClient` trait (#44).
- `CancellationToken` in `Agent::run`, plus configurable confirm and command
  timeouts (#45).
- `SecretProvider` trait and `SecretSubstitutingExecutor` for injectable secrets
  and `$FILAR_SECRET_N` substitution (#46).
- `local` cargo feature for `filar-transport` and a cross-compilation CI matrix
  (#47).

### Fixed

- A panic hook restores the terminal, so a panic no longer leaves it in a broken
  raw/alternate-screen state (#40).
- Hovering a confirm-dialog button no longer changes the Enter action; the safety
  default (Deny) is kept until an explicit selection (#41).
- The SSE stream tail is flushed on end so the final response delta is not lost
  (#42).

## [0.2.0] - 2026-07-07

TUI modernization: the mouse becomes a first-class input alongside the keyboard.

### Added

- Mouse support in the chat: wheel scroll, click, and drag to select (#15).
- Scrollbar with click hit-testing (#16).
- Click-to-confirm command dialog with clickable buttons (#17).
- Collapsible command blocks, toggled by click (#18).
- Streaming LLM responses with a spinner in Thinking mode (#19).
- Text selection and clipboard copy (#21).
- Mouse support in the interactive terminal mode (#22).

### Changed

- Visual redesign: borderless layout, markdown-lite rendering, and a clickable
  help-bar (#20).
- Theme module extraction and render refactor (#13); chat layout is cached and
  rebuilt only on invalidation (#14).
- Enter in the confirm dialog activates the selected button (default Deny)
  instead of an unconditional approve.

### Fixed

- Layout stability: no flicker or artifacts on mode change, and graceful
  degradation when mouse capture is unavailable (#23).

[Unreleased]: https://github.com/devlawey/filar/compare/v1.0.2...HEAD
[1.0.2]: https://github.com/devlawey/filar/compare/v1.0.1...v1.0.2
[1.0.1]: https://github.com/devlawey/filar/compare/v1.0.0...v1.0.1
[1.0.0]: https://github.com/devlawey/filar/compare/v0.9.0...v1.0.0
[0.9.0]: https://github.com/devlawey/filar/compare/v0.8.6...v0.9.0
[0.8.6]: https://github.com/devlawey/filar/compare/v0.8.5...v0.8.6
[0.8.5]: https://github.com/devlawey/filar/compare/v0.8.4...v0.8.5
[0.8.4]: https://github.com/devlawey/filar/compare/v0.8.2...v0.8.4
[0.8.2]: https://github.com/devlawey/filar/compare/v0.8.1...v0.8.2
[0.8.1]: https://github.com/devlawey/filar/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/devlawey/filar/compare/v0.7.4...v0.8.0
[0.7.4]: https://github.com/devlawey/filar/compare/v0.7.3...v0.7.4
[0.7.3]: https://github.com/devlawey/filar/compare/v0.7.2...v0.7.3
[0.7.2]: https://github.com/devlawey/filar/compare/v0.7.1...v0.7.2
[0.7.1]: https://github.com/devlawey/filar/compare/v0.7.0...v0.7.1
[0.7.0]: https://github.com/devlawey/filar/compare/v0.6.2...v0.7.0
[0.6.2]: https://github.com/devlawey/filar/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/devlawey/filar/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/devlawey/filar/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/devlawey/filar/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/devlawey/filar/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/devlawey/filar/compare/v0.3.0...v0.4.0
[0.4.0]: https://github.com/devlawey/filar/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/devlawey/filar/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/devlawey/filar/releases/tag/v0.3.0
