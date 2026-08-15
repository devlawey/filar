# Platform Notes

Known platform-specific behaviours that affect development or runtime.
Add findings here whenever a platform difference is discovered.

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

> **Arch decision (#289):** macOS ships **one** architecture — `aarch64`
> (Apple Silicon). The job pins `macos-14` (not floating `macos-latest`) and
> asserts `uname -m == arm64` before packaging so the asset name cannot drift.
> Intel (`x86_64`) and universal binaries are out of scope until packaging
> follow-up (#297). Unsigned OSS downloads on macOS may need
> `xattr -d com.apple.quarantine <path>` before first run.

