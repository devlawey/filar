# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The engine crates (`filar-core`, `filar-transport`, `filar-agent`) are consumed
by external projects via git tags; `engine-v0.3.0` is the first stable
dependency point for embedders (see `docs/ENGINE_API.md`).

## [Unreleased]

### Fixed

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

### Changed

- `config.toml` search order: CWD before app-data (local `./config.toml` now
  overrides `%APPDATA%\filar\`)
  ([#175](https://github.com/devlawey/filar/issues/175)).
- README updated with all 0.7.0 features: Models tab, Ctrl+L, token counter,
  config priority, key storage
  ([#175](https://github.com/devlawey/filar/issues/175)).

### Added

- GUI launcher now has a Models tab with add/delete profile management; each profile
  stores its API key in the OS credential store independently
  ([#162](https://github.com/devlawey/filar/issues/162)).
- Ctrl+L now cycles through LLM profiles per tab; each session can use a different
  model without affecting other tabs
  ([#163](https://github.com/devlawey/filar/issues/163)).
- Token usage counter now uses real API response data (`usage.prompt_tokens` /
  `completion_tokens`) instead of character-length estimation
  ([#173](https://github.com/devlawey/filar/issues/173)).

### Fixed

- API keys and SSH passwords are no longer written in plain text to
  `pending_launch.json`; secrets are read from the OS credential store instead
  ([#159](https://github.com/devlawey/filar/issues/159)).
- `Settings::save` and `save_pending_launch` now create their parent directory before
  writing, preventing silent data loss when `%APPDATA%\filar` doesn't exist
  ([#160](https://github.com/devlawey/filar/issues/160)).
- `config.toml` is now searched in `%APPDATA%\filar\` first (unified config location),
  then CWD, then next to the executable
  ([#161](https://github.com/devlawey/filar/issues/161)).

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

### Fixed

- Per-tab interactive terminals are torn down on tab close and app exit, and
  background EOF/errors retire a tab's terminal without disturbing the active
  tab ([#116](https://github.com/devlawey/filar/issues/116)).
- Window resize now propagates to every live per-tab terminal (model and backend),
  not just the active one, so background terminals stay correctly sized
  ([#117](https://github.com/devlawey/filar/issues/117)).

### Added

- Background terminal output marks its tab with a new-output indicator that
  clears on switch; docs updated for persistent per-tab terminals
  ([#118](https://github.com/devlawey/filar/issues/118)).

- Per-terminal reader tasks feed a tagged channel so every interactive backend
  (including background tabs) is drained and routed to its own session model
  ([#114](https://github.com/devlawey/filar/issues/114)).

### Fixed

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

[Unreleased]: https://github.com/devlawey/filar/compare/v0.6.2...HEAD
[0.6.2]: https://github.com/devlawey/filar/compare/v0.6.1...v0.6.2
[0.6.1]: https://github.com/devlawey/filar/compare/v0.6.0...v0.6.1
[0.6.0]: https://github.com/devlawey/filar/compare/v0.5.1...v0.6.0
[0.5.1]: https://github.com/devlawey/filar/compare/v0.5.0...v0.5.1
[0.5.0]: https://github.com/devlawey/filar/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/devlawey/filar/compare/v0.3.0...v0.4.0
[0.4.0]: https://github.com/devlawey/filar/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/devlawey/filar/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/devlawey/filar/releases/tag/v0.3.0
