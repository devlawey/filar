# Platform Notes

Known platform-specific behaviours that affect development or runtime.
Add findings here whenever a platform difference is discovered.

**1.0.0 dual-platform summary** (Windows + macOS; Linux not a release target):

| Topic | Section | Issues |
|--------|---------|--------|
| App data dirs | [Application data directory](#application-data-directory) | #291 |
| macOS Ctrl vs ⌘, Fn+F1 | [macOS shortcuts](#macos-shortcuts) | #292 |
| Windows console missing `⌘` glyph | [TUI help overlay glyphs](#tui-help-overlay-glyphs) | #310 |
| GUI launcher paste / show password | [GUI launcher secrets](#gui-launcher-secrets-macos) | #312 |
| Interactive PTY shell | [Local interactive shell](#local-interactive-shell-ctrlt) | #293, #313, #338 |
| Release assets / packaging | [Release binaries (CI)](#release-binaries-ci) | #289, #297, #80 |
| Chat wrap display columns | [Chat display width vs char count](#chat-display-width-vs-char-count-333) | #333 |

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
>
> GUI launcher secret fields (API key / SSH password) do **not** take the raw
> egui paste: see [GUI launcher secrets](#gui-launcher-secrets-macos).

## GUI launcher secrets (macOS)

On macOS, copying an API token or SSH password from Keychain Access or a
browser puts **UTF-8 text plus a trailing newline** (sometimes a BOM) on
`NSPasteboard`. egui 0.29 `TextEdit::singleline` then replaces `\n`/`\r` with
a space, so the field becomes `"token "` and LLM auth / SSH login fail.
Typing the same value by hand works because it has no newline (#312).

The launcher therefore:

1. Intercepts `Event::Paste` on focused secret fields **before** TextEdit, and
   stores `sanitize_secret_clipboard` (first line, drop BOM / ZWSP / controls;
   surrounding ASCII spaces are kept so SSH passwords are not altered).
2. Sanitizes again on Launch and when reading/writing the OS keyring (newlines /
   BOM only — surrounding spaces are kept).
3. Offers **Show password** / **Show** (API key) checkboxes (not persisted) so
   the pasted value can be verified. Secrets are still never written to
   `settings.json`, `pending_launch.json`, or `config.toml`.

Windows is not the original report, but the same sanitizer runs there (browser
copy can also include a trailing newline).

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

## TUI help overlay glyphs

Windows console fonts (conhost / many raster and TrueType faces) do not include
`⌘`. Ratatui then shows `?` in the F1 overlay (#310).

The help registry therefore uses `overlay_desc_macos` in
`crates/tui/src/ui/help.rs`: macOS keeps `⌘` / Fn+F1 wording; Windows and
Linux use ASCII (`Cmd`). Markdown docs (`USER_GUIDE`, this file) may still
use `⌘` because they are not rendered in the Windows console.

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
>
> Cwd sync (#313, #338): on Unix/macOS and on SSH (POSIX remote), **hiding**
> interactive (`Ctrl+T`) or fully leaving it probes OSC 7 via `printf`/`pwd`
> and applies `CommandExecutor::set_cwd`, so the status bar and agent/`!`
> commands share the PTY directory. A stale enter-time cwd does not skip the
> probe. Windows **local** interactive is `cmd.exe`, which typically does not
> emit OSC 7; sync then keeps the last known tab cwd. Entering interactive
> still spawns `cmd.exe` with `CommandBuilder::cwd`.

## Agent local commands and controlling TTY (#329)

| Surface | Controlling TTY | Password prompts |
|---------|-----------------|------------------|
| Local agent `LocalExecutor` (Unix/macOS) | Child runs after `setsid` — **no** controlling TTY | `sudo`/`passwd` cannot paint `Password:` over the filar TUI; they fail fast (e.g. “terminal is required”) → agent guidance: Ctrl+P / `$FILAR_SECRET_N` + `sudo -S` |
| Local agent `LocalExecutor` (Windows) | No POSIX `setsid`; PowerShell `-NonInteractive` | Interactive secure-string prompts are uncommon; same Ctrl+P / secret substitution contract |
| Interactive Ctrl+T PTY | Full PTY — prompts allowed | Shell/`sudo` password UI is expected here |
| SSH agent channel | Remote shell PTY (persistent channel) | Bare `sudo` can still block until timeout; Allowlist never auto-approves `sudo`; prompt + `sudo -S` + secrets |

> Before #329, Unix local agent children inherited the TUI’s controlling
> terminal, so macOS `sudo` wrote `Password:` on top of ratatui while the
> session stayed in Thinking.

## Chat display width vs char count (#333)

Chat wrap/pad (#325 follow-up) measure lines in **unicode display columns**
(same `unicode-width` 0.1 as ratatui), not `chars().count()`. Tabs in
command output are expanded to spaces (tab stop 8) before wrap so
columnar tools (`ps`, aligned tables) stay consistent.

| Concern | Note |
|---------|------|
| CJK / wide glyphs | Two columns each; char-count wrap overflowed the viewport and left ghosts after scroll |
| Ambiguous width (e.g. some box-drawing) | Follow unicode-width/ratatui; Terminal.app vs Windows Terminal can still differ for East-Asian Ambiguous characters — report if still visible after #333 |
| Interactive PTY scrollback | Separate `TerminalModel` path; not changed by #333 |

## TUI path picker (#351, #359)

Agent input path picker (`/`, `Ctrl+Shift+F`, `Ctrl+Shift+D`) is an in-TUI
overlay listing the **active tab's target** filesystem:

| Tab | Listing source | Path algebra |
|-----|----------------|--------------|
| Local | Client `read_dir` | Host-native (`std::path`) |
| SSH | Readonly remote `ls -1Ap` via session executor (zero-install) | **POSIX string join/parent** (not Windows `Path`) |

Native OS file dialogs (`rfd`) are no longer used in the TUI; the GUI launcher
Browse button still uses `rfd` locally.

Remote listing requires a live SSH executor on the tab; large directories are
capped at 500 entries with a truncation warning.

Selection cursor is ASCII `>` — Unicode ▶ rendered as `?` on many Windows
consoles (#359, same class as #310 help glyphs).

## Разрешение системных часов в тестах: macOS грубее Linux

**Где:** `crates/core/src/config.rs` (тесты), обнаружено в CI по #424.

Два теста строили имя временного файла из `std::process::id()` и
`SystemTime::now().as_nanos()`. На Linux это годами не сталкивалось, на
macOS-раннере (`macos-aarch64`) два параллельных теста получили **одинаковое**
значение `as_nanos()` и записали в один путь.

`SystemTime::now()` не обязан иметь наносекундное разрешение — он лишь
выражен в наносекундах. На macOS фактическая гранулярность заметно грубее,
поэтому два вызова из разных потоков в пределах одного тика возвращают
одно и то же число.

Последствие было тихим, а не громким: `std::fs::write` сначала усекает файл,
и проигравший гонку тест читал нулевой размер. Пустой `config.toml` —
**валидная** конфигурация (все поля `#[serde(default)]`), проходит
`validate()`, поэтому тест, ожидавший `is_err()`, получал `Ok` и падал с
сообщением про temperature.

**Правило:** уникальность имени временного файла или каталога в тестах
нельзя закладывать на часы. Используйте process-wide счётчик
(`AtomicU64`) плюс метку теста — счётчик столкнуться не может, а
таймстамп может. Очистку вешайте на `Drop`, а не на последнюю строку
теста: при панике assert'а она не выполнится.

## busybox `df` (Alpine) не принимает `--output`

**Где:** `crates/core/src/builtin_fleet_checks.toml`, `crates/agent/src/preprocess.rs`.

GNU coreutils `df` принимает `--output=source,size,used,avail,pcent,target`,
что фиксирует набор колонок независимо от того, какие колонки конкретная
сборка печатает по умолчанию. busybox `df` на Alpine такой флаг отвергает
целиком.

Поэтому встроенная проверка `disk-usage` — первая и пока единственная, у
которой есть вариант команды под ОС (#425): `default` использует
`--output=`, `alpine` — голый `df`. Обе формы печатают одни и те же шесть
колонок, и `DfPreprocessor` читает обе, так что таблица для сравнения между
хостами получается одинаковая, а не вторая по форме.

**Правило:** прежде чем закладывать GNU-флаг в команду флот-проверки,
проверьте busybox-эквивалент. Если он отличается — это вариант под
`alpine`, а не повод отказаться от флага везде.

## Определение семейства ОС: `ID` и `ID_LIKE`

**Где:** `crates/core/src/os_family.rs`.

Семейство определяется по `/etc/os-release`: сначала `ID`, затем `ID_LIKE`.
Второе поле существует ровно для производных дистрибутивов — Alma, Rocky и
Oracle объявляют `ID_LIKE="rhel centos fedora"`, поэтому попадают в
RHEL-семейство без перечисления каждой пересборки в коде.

Значения бывают и в кавычках (`ID="alpine"`), и без (`ID=alpine`); обе формы
встречаются в реальных файлах, обе принимаются. Регистр игнорируется —
правило спецификации про нижний регистр реальные файлы нарушают.

Хост, который не удалось отнести ни к одному семейству, получает
`OsFamily::Unknown`. Это значение, а не ошибка: проверка с `default`-командой
на нём выполняется, проверка без — помечается неприменимой. Угадывать
семейство по частичному совпадению нельзя: неверный вариант — это неверная
команда на живом хосте.

## `Ctrl+J` — это перевод строки (#431)

**Где:** `crates/tui/src/app.rs::handle_side_panel_key`.

`Ctrl+J` посылает байт `0x0A` (LF). Unix-терминал в raw-режиме отдаёт его
как есть, и crossterm 0.28 сообщает `Char('j')` + `CONTROL`: ветка
`b'\n' => Enter` в его парсере срабатывает только **вне** raw-режима. TUI
работает в raw-режиме, поэтому `Ctrl+J` отличим от `Enter` (`0x0D`). На
Windows консоль отдаёт событие клавиши с виртуальным кодом `J` и флагом Ctrl.
На случай консоли, которая передаст управляющий символ без маппинга,
обработчик принимает и `Char('\n')` с `CONTROL`. Русская раскладка —
`Ctrl+о`, как у остальных хоткеев.

