# Platform Notes

Known platform-specific behaviours that affect development or runtime.
Add findings here whenever a platform difference is discovered.

**1.0.0 dual-platform summary** (Windows + macOS; Linux not a release target):

| Topic | Section | Issues |
|--------|---------|--------|
| App data dirs | [Application data directory](#application-data-directory) | #291 |
| macOS Ctrl vs ⌘, Fn+F1 | [macOS shortcuts](#macos-shortcuts) | #292 |
| Interactive PTY shell | [Local interactive shell](#local-interactive-shell-ctrlt) | #293 |
| Release assets / packaging | [Release binaries (CI)](#release-binaries-ci) | #289, #297, #80 |

## Clipboard

| Platform | `arboard::get_text()` | Bracketed paste |
|----------|----------------------|-----------------|
| Windows  | Instant (Win32 `GetClipboardData`) | Yes (Windows Terminal 1.18+) |
| Linux    | May block (X11 clipboard protocol) | Terminal-dependent |
| macOS    | Instant (`NSPasteboard`) | Terminal-dependent |

> On Windows, `arboard::Clipboard::get_text()` is synchronous but effectively
> instant — it reaches the local Win32 clipboard API. On Linux/X11, the
> clipboard owner can be a remote process and the call may block; when a Linux
> build becomes supported, the clipboard read in `handle_key` (#153) should be
> wrapped in `tokio::task::spawn_blocking`.

## Terminal features

| Feature          | Windows Terminal | Linux (foot/alacritty) | macOS (iTerm2/Terminal) |
|------------------|------------------|------------------------|-------------------------|
| Mouse capture    | Yes              | Yes                    | Yes                     |
| Bracketed paste  | Yes              | Yes                    | Yes                     |
| Ctrl+H vs BS     | Same (BS)        | Same (BS)              | Same (BS)               |
| F1 in raw mode   | Yes              | Yes                    | Fn+F1 may differ        |

> `Ctrl+H` is indistinguishable from `Backspace` on all tested terminals
> (both arrive as `KeyCode::Backspace` or `0x08`). For this reason the help
> overlay is bound only to `F1` (#142), and `Ctrl+H` is not reserved.
> On macOS see [macOS shortcuts](#macos-shortcuts) for Fn+F1 and Ctrl vs ⌘.

## Key mapping (Russian ЙЦУКЕН)

| Latin key | Russian char | C shortcut |
|-----------|-------------|------------|
| T         | е           | ^T         |
| C         | с           | Not used   |
| W         | ц           | ^W         |
| N         | т           | ^N         |
| P         | з           | ^P         |
| Q         | й           | ^Q         |
| Z         | я           | ^Z         |
| V         | м           | ^V (paste) |

Mapped via `ctrl_key(en, ru)` helper in `crates/tui/src/app.rs`.

## macOS shortcuts

TUI bindings use **Control**, not Command (⌘). This matches Windows/Linux and
is intentional for 1.0.0 (#292); remapping to ⌘ is out of scope.

| Topic | Behaviour on macOS |
|-------|---------------------|
| Modifier | `KeyModifiers::CONTROL` only. ⌘+letter is **not** a filar shortcut (often claimed by the terminal or the OS). |
| Help (`F1`) | Apple keyboards often need **Fn+F1** (Touch Bar / “Use F1, F2, etc. as standard function keys” off). If F1 does nothing, try Fn+F1 or enable standard F-keys in System Settings → Keyboard. |
| Paste (`Ctrl+V`) | Uses `arboard` (`NSPasteboard`) — works in Terminal.app / iTerm2 when the terminal forwards Ctrl+V. Bracketed paste also works when the terminal enables it. |
| Quit / terminal / tabs | `Ctrl+Q`, `Ctrl+T`, `Ctrl+N`/`W`/`Tab` — same as other platforms; use the **Control** key, not ⌘. |
| Russian ЙЦУКЕН | Same `ctrl_key(en, ru)` mapping as elsewhere; verify in the terminal you use (Terminal.app / iTerm2). |

**Known limitation:** if the terminal swallows Ctrl+letter or F-keys before
crossterm sees them, filar cannot receive the shortcut — that is a terminal
configuration issue, not a silent failure inside filar.

## Async blockers

- `arboard::Clipboard::get_text()` — synchronous on Windows/macOS, may require
  `spawn_blocking` on Linux/X11 (discussion in PR review of #153).

## Command output encoding (Windows)

| Platform | Default console code page | filar behaviour |
|----------|--------------------------|-----------------|
| Windows  | OEM console CP (e.g. CP866, CP437/CP850) or ANSI CP (CP1251/CP1252), host-config-dependent | `[Console]::OutputEncoding = UTF8` prepended + `2>&1` stderr redirect |
| Unix     | UTF-8                    | No conversion needed |

> PowerShell on Windows encodes its own console output using the active code
> page: OEM pages (CP866 for Russian locales, CP437/CP850 for Western) for
> legacy 8-bit console APIs, or ANSI pages (CP1251/CP1252) for the .NET text
> layer — which one applies depends on host configuration and locale.
> `chcp 65001` (via `SetConsoleOutputCP`) is **ineffective for piped output**:
> .NET caches the console encoding at startup and ignores later `chcp` calls.
> Instead, filar sets `[Console]::OutputEncoding = [System.Text.UTF8Encoding]::new()`,
> which directly changes the .NET property PowerShell uses to encode its own
> output (cmdlet output, error messages) to UTF-8. This does **not** touch the
> console active code page, so it avoids font-switch/resize events on the
> parent console (#246). `2>&1` redirects stderr through stdout so PowerShell
> error messages are also decoded correctly (#243). `String::from_utf8_lossy`
> then decodes the bytes (#228).
>
> **Note:** this covers PowerShell's *own* output. Native programs writing raw
> bytes to the pipe in the OEM code page may still be mis-decoded; that is a
> separate, harder problem and is not handled here.

## Release binaries (CI)

`.github/workflows/release.yml` builds and attaches assets on release publish:

| Platform | Runner | Asset name |
|----------|--------|------------|
| Windows  | `windows-latest` | `filar-{tag}-windows-x86_64.exe` |
| macOS    | `macos-14` (arm64) | `filar-{tag}-macos-aarch64` |

### Packaging decision for 1.0.0 (#297)

**Chosen: binary-only** (same shape as Windows `.exe` — one downloadable
file, no installer). macOS asset is the raw Mach-O executable
`filar-{tag}-macos-aarch64`, **not** a `.app` bundle and **not** notarized.

| Option | 1.0.0 |
|--------|--------|
| Raw binary | **Yes** (this release) |
| `.app` + zip | Follow-up after 1.0.0 |
| Notarization / staple | Follow-up after 1.0.0 (OSS cost/complexity) |
| Intel (`x86_64`) / universal | Out of scope (still Apple Silicon only, #289) |

### Unsigned OSS policy (macOS Gatekeeper + Windows SmartScreen)

Release builds are **unsigned** open-source binaries. Users may see OS
warnings; that is expected until code-signing / notarization land.

| Platform | Typical warning | User workaround |
|----------|----------------|-----------------|
| macOS | Gatekeeper quarantine / “cannot be opened” | `chmod +x` + `xattr -d com.apple.quarantine …` (below) |
| Windows | SmartScreen / “Windows protected your PC” | “More info” → Run anyway (tracked in [#80](https://github.com/devlawey/filar/issues/80)) |

Same policy on both platforms: document the bypass; do not pretend the
build is signed.

> **Arch (#289):** macOS ships **one** architecture — `aarch64` (Apple Silicon).
> The job pins `macos-14` and asserts `uname -m == arm64` before packaging.

After downloading the macOS asset from GitHub Releases:

```bash
chmod +x filar-*-macos-aarch64 && xattr -d com.apple.quarantine filar-*-macos-aarch64
```

### Release notes snippet (for `/prepare-release`)

Include under Downloads (adapt version):

```markdown
## Downloads
- Windows: `filar-vX.Y.Z-windows-x86_64.exe` (unsigned; SmartScreen may warn — see #80)
- macOS (Apple Silicon): `filar-vX.Y.Z-macos-aarch64` (raw binary, not a `.app`; not notarized)
  ```bash
  chmod +x filar-vX.Y.Z-macos-aarch64 && xattr -d com.apple.quarantine filar-vX.Y.Z-macos-aarch64
  ```
```

## Application data directory

Decision for [#291](https://github.com/devlawey/filar/issues/291): use the OS
user **data** directory via `dirs::data_dir()` as the **parent** returned by
`default_base_dir()`, then join `filar/` in callers (`SessionStore::new` does
the same). `settings.json`, `pending_launch.json`, `config.toml`, `sessions/`,
and `logs/` all share this single app root.

| Platform | Base (`default_base_dir()`) | App root |
|----------|------------------------------|----------|
| Windows  | `%APPDATA%` (Roaming) | `%APPDATA%\filar\` |
| macOS    | `~/Library/Application Support` | `~/Library/Application Support/filar/` |
| Linux    | `$XDG_DATA_HOME` or `~/.local/share` | `~/.local/share/filar/` |

> **Legacy Unix path:** before 1.0.0, non-Windows builds used `$HOME/filar/`.
> On first run, if `$HOME/filar` exists and the new app root does not, filar
> renames it into the new location (best-effort). Windows paths are unchanged.

## Local interactive shell (Ctrl+T)

| Platform | Default PTY shell |
|----------|-------------------|
| Unix / macOS | `$SHELL` if set to an existing file; otherwise `sh` |
| Windows | `cmd.exe` (PowerShell as default is out of scope) |

> Agent command execution (`LocalExecutor`) is separate: Unix still uses
> `sh -c`, Windows PowerShell. Only the interactive PTY follows `$SHELL`
> ([#293](https://github.com/devlawey/filar/issues/293)).

