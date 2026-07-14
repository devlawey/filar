# Changelog

All notable changes to this project are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

The engine crates (`filar-core`, `filar-transport`, `filar-agent`) are consumed
by external projects via git tags; `engine-v0.3.0` is the first stable
dependency point for embedders (see `docs/ENGINE_API.md`).

## [Unreleased]

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

[Unreleased]: https://github.com/devlawey/filar/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/devlawey/filar/releases/tag/v0.3.0
