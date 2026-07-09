# PROGRESS.md — состояние проекта filar

> Этот файл содержит всё необходимое для продолжения работы в новом диалоге.
> Обновлять после каждого этапа.

---

## 1. О проекте

**Filar** — терминал с AI-агентом поверх SSH на Rust. Главная фича: агент
управляет удалённой машиной через SSH, **zero-install** на удалёнке (никаких
файлов на диске удалённой машины). План разработки — в `PLAN.md`.

---

## 2. Окружение (ВАЖНО для нового диалога)

- **ОС:** Windows 22H2, PowerShell (не использовать `&&`, использовать `;`)
- **Rust:** установлен через rustup, тулчейн `stable-x86_64-pc-windows-gnu`
  (НЕ msvc — нет Visual Studio Build Tools)
- **MinGW:** портативная сборка WinLibs в `C:\Users\AdminLocal\mingw\mingw64\bin`
  (линкер `x86_64-w64-mingw32-gcc.exe`, `windres.exe`)
- **PATH:** cargo и mingw добавлены в User PATH постоянно
- **Docker:** НЕ установлен (нельзя запустить тестовый sshd-контейнер)
- **Права:** без администратора

### Команды сборки:
```powershell
cd c:\dev\warper
cargo build
cargo test
```

### Конфиг линкера: `.cargo/config.toml`
Указывает абсолютные пути к линкеру и ar из WinLibs.

---

## 3. Структура воркспейса

```
c:\dev\warper\
├── Cargo.toml              # workspace (members: core, transport, agent, tui, gui, app)
├── Cargo.lock              # закоммитить (бинарный проект)
├── config.toml             # конфиг приложения
├── .cargo/config.toml      # конфиг линкера для GNU тулчейна
├── .gitignore
├── PLAN.md                 # полный план разработки (8 этапов)
├── PROGRESS.md             # этот файл
├── pics/                   # иконки приложения
│   ├── filar.ico           # мультирезолюшн .ico (6 размеров, 27KB) — для .exe
│   ├── filar_256.png       # PNG 256x256 — для иконки окна
│   ├── filar_128.png       # PNG 128x128
│   ├── filar_64.png        # PNG 64x64
│   ├── filar_512.png       # PNG 512x512
│   ├── filar_1024.png      # PNG 1024x1024
│   ├── filar_logo.svg      # SVG логотип
│   ├── filar_icon_*.svg    # SVG иконки (32, 64, 512)
│   └── icon.svg            # SVG исходник
├── crates/
│   ├── core/               # ошибки, конфиг, секреты, чат-блоки, сессии — ГОТОВ
│   │   ├── Cargo.toml
│   │   └── src/{lib,error,config,secrets,chat,session}.rs
│   ├── transport/          # CommandExecutor + InteractiveTerminal + Ssh/Local — ГОТОВ
│   │   ├── Cargo.toml
│   │   └── src/{lib,ssh,local,interactive}.rs
│   ├── agent/              # LlmClient + GlmClient + Agent + tools + security — ГОТОВ
│   │   ├── Cargo.toml
│   │   └── src/{lib,glm,agent,tools,security}.rs
│   ├── tui/                # ratatui + crossterm TUI + terminal emulator — ГОТОВ
│   │   ├── Cargo.toml
│   │   └── src/{lib,app,ui/mod,ui/theme,ui/text,ui/bars,ui/chat,ui/input,ui/layout_cache,event,confirmer,runner,terminal}.rs
│   ├── gui/                # GUI-лаунчер на eframe + keyring — ГОТОВ
│   │   ├── Cargo.toml
│   │   └── src/lib.rs
│   └── app/                # бинарник `filar` + build.rs (иконка) — ГОТОВ
│       ├── Cargo.toml
│       ├── build.rs        # встраивает .ico в .exe через windres
│       └── src/main.rs
└── docker/
    └── sshd/Dockerfile     # тестовый SSH-сервер (требует Docker)
```

---

## 4. Crate-имена и бинарник

| Crate | Имя | Зависимости |
|-------|-----|-------------|
| core | `filar-core` | serde, toml, thiserror, tracing |
| transport | `filar-transport` | filar-core, russh, ssh-key, portable-pty, tokio |
| agent | `filar-agent` | filar-core, filar-transport, reqwest |
| tui | `filar-tui` | filar-core, filar-agent, filar-transport, ratatui, crossterm, alacritty_terminal |
| gui | `filar-gui` | filar-core, eframe, keyring, image |
| app | `filar-app` | filar-core, filar-transport, filar-agent, filar-tui, filar-gui, winres (build-dep) |

Бинарник: `filar.exe` (binary name = `filar`)

Секретные переменные: `$FILAR_SECRET_N` (было `$WARP_SECRET_N`)
SSH-маркер: `__FILAR_req_XXXXXXXX` (было `__WARPLITE_...`)
Env для конфига: `FILAR_CONFIG` (было `WARP_CONFIG`)
Директория сессий: `%APPDATA%/filar/sessions/` (было `%APPDATA%/warp/sessions/`)
Cred store service: `"filar"` (было `"warp"`)

---

## 5. Что сделано (все этапы + дополнительные фичи)

### ✅ Этапы 1–8 — Базовая разработка (ЗАВЕРШЕНЫ)

См. `PLAN.md` для описания этапов. Все 8 этапов завершены.
Базовая функциональность: SSH-ядро, транспорт, LLM-клиент (GLM),
агент с инструментами, TUI на ratatui, интерактивный терминал,
GUI-лаунчер, сессии, мульти-LLM.

51 unit-тест проходят (33 agent, 16 tui, 2 transport).
1 pre-existing failure: `parse_minimal_config` (ожидает `Always`, дефолт `Allowlist`) — unrelated.

---

### ✅ Дополнительные фичи и багфиксы (ПОСЛЕ ЭТАПА 8)

#### 5.1 Shell escape (`!command`) — ВЫПОЛНЕНО

**Файлы:** `crates/tui/src/app.rs`, `crates/tui/src/runner.rs`

- В Normal режиме ввод `!ls` → команда выполняется напрямую через `TuiExecutor.run()`
- Без вызова агента — мгновенный результат
- `!ssh user@host` → специальная обработка (см. 5.5)
- Интерактивные команды (`vim`, `top`, `nano`, `less`, `man`, `mc`, `screen`, `tmux`,
  `passwd`, `mysql`, `python`, `bash`, `sudo` и др.) блокируются с сообщением
- Функция `is_interactive_command()` в `app.rs` — список из ~30 программ
- Help bar: `!=Shell`

#### 5.2 Исправление мерцания и наложения текста — ВЫПОЛНЕНО

**Файлы:** `crates/tui/src/runner.rs`

- Добавлен флаг `needs_clear` — устанавливается при получении agent events
- Перед `terminal.draw()` если `needs_clear = true` → `terminal.clear()`
- Предотвращает наложение старого текста на новый при быстром обновлении

#### 5.3 История ввода (Up/Down) — ВЫПОЛНЕНО

**Файлы:** `crates/tui/src/app.rs`

- `input_history: Vec<String>`, `history_pos: Option<usize>`, `saved_input: String`
- Up — browsing older, Down — newer
- Любой ввод символа отменяет browsing
- Down past end → restores saved input
- Дубликаты не сохраняются

#### 5.4 Динамический системный промпт — ВЫПОЛНЕНО

**Файлы:** `crates/agent/src/agent.rs`

- `build_system_prompt(is_local: bool, ssh_info: Option<&str>, is_windows: bool) -> String`
- **Windows local:** упоминает PowerShell, предлагает Windows-команды (Get-ComputerInfo, Get-ChildItem)
- **SSH:** упоминает удалённую машину, POSIX shell
- Явно говорит: "shell state does NOT persist between calls"
- Правила: не использовать интерактивные команды, секреты через `$FILAR_SECRET_N`
- `AgentBuilder.local_mode()` → `build_system_prompt(true, None, cfg!(windows))`
- `AgentBuilder.ssh_mode(ssh_info)` → `build_system_prompt(false, ssh_info, false)`
- `AgentEvent::TransportChanged { is_local, ssh_info }` — event для обновления промпта

#### 5.5 LocalExecutor — полная переработка — ВЫПОЛНЕНО

**Файлы:** `crates/transport/src/local.rs`

- **Было:** PTY через `portable-pty` (cmd.exe) — ломалось на POSIX-командах → "os error 232"
- **Стало:** `tokio::process::Command` (субпроцесс, без персистентного shell)
  - Windows: `powershell -NoProfile -NonInteractive -Command "..."`
  - Unix: `sh -c "..."`
- Timeout: 60 секунд (`DEFAULT_TIMEOUT`)
- Cancel: через `tokio::select!` + `cancel_notify: Arc<Notify>`
- `kill_on_drop(true)` — убивает процесс при drop future (cancel/timeout)

#### 5.6 Переключение SSH-транспорта из local mode — ВЫПОЛНЕНО

**Файлы:** `crates/tui/src/app.rs`, `crates/tui/src/runner.rs`, `crates/tui/src/event.rs`

- `!ssh user@host [-p port]` → парсинг `parse_ssh_command()` в `app.rs`
- Показ сообщения: "Connecting to user@host:port via SSH. Press Ctrl+P to enter the password."
- Ctrl+P → ввод пароля (маскированный)
- Enter → Thinking mode → `SshExecutor::connect()` → `swap_executor()` → `TransportChanged`
- `TuiExecutor.inner: Arc<RwLock<Arc<dyn CommandExecutor>>>` — swappable
- `swap_executor()` — замена исполнителя в runtime
- После успешного подключения: "Connected to user@host:port via SSH."
- Системный промпт автоматически обновляется на SSH-вариант

#### 5.7 Восстановление SSH-канала после таймаута — ВЫПОЛНЕНО

**Файлы:** `crates/transport/src/ssh.rs`

- При таймауте (120с): отправка Ctrl-C (`\x03`) в канал
- Resync: новый sync-маркер `__FILAR_sync_<uuid>__`
- Дренаж pending output до resync-маркера
- Канал возвращается в known state для следующих команд
- Логирование: `warn!("command timed out, sending Ctrl-C and resyncing")`

#### 5.8 Ctrl+C в Thinking mode — отмена вместо выхода — ВЫПОЛНЕНО

**Файлы:** `crates/tui/src/app.rs`

- **Было:** Ctrl+C в Thinking → `should_quit = true` (выход из приложения)
- **Стало:** Ctrl+C в Thinking → cancel: `agent_running = false`, `pending_* = None`,
  `mode = Normal`, сообщение "Cancelled."
- Normal mode Ctrl+C → выход (без изменений)

#### 5.9 GUI: 5 SSH-профилей — ВЫПОЛНЕНО

**Файлы:** `crates/gui/src/lib.rs`

- 6 radio-кнопок: Local, SSH1, SSH2, SSH3, SSH4, SSH5
- Каждый слот: Host, Port, User, Password, "Save password" checkbox
- В `settings.json` сохраняются: host, port, user, save_password (НЕ пароль)
- При следующем запуске: восстановление выбранного слота и его полей
- `Settings { model, api_base_url, ssh_profiles: Vec<SshProfile>, last_ssh }`
- `SshProfile { host, port, user, save_password }` — без пароля

#### 5.10 OS Credential Storage — ВЫПОЛНЕНО

**Файлы:** `crates/gui/src/lib.rs`, `Cargo.toml`, `crates/gui/Cargo.toml`

- Зависимость: `keyring = { version = "3", features = ["windows-native", "apple-native", "sync-secret-service"] }`
- CRED_SERVICE = `"filar"`
- **API key:** ВСЕГДА сохраняется в OS Credential Manager (Windows Credential Manager)
  - НЕ пишется в `settings.json`
  - При следующем запуске: автоматически подгружается
  - Подсказка поля: "saved in OS credential store"
- **SSH пароли:** по галочке "Save password"
  - С галочкой → сохраняется в Credential Manager по ключу `ssh0`, `ssh1`, ..., `ssh4`
  - Без галочки → удаляется из Credential Manager
  - При следующем запуске: если save_password=true, пароль подгружается автоматически

#### 5.11 Переименование warp → filar — ВЫПОЛНЕНО

**Все файлы** — полное переименование:

- Crate names: `warp-core` → `filar-core`, и т.д. для всех 6 крейтов
- Binary: `warp.exe` → `filar.exe`
- All `use warp_*::` → `use filar_*::` во всех `.rs` файлах
- `WARP_SECRET_N` → `FILAR_SECRET_N`
- `WARP_CONFIG` → `FILAR_CONFIG`
- `__WARPLITE_` → `__FILAR_` (SSH маркер)
- CRED_SERVICE: `"warp"` → `"filar"`
- Session dir: `%APPDATA%/warp/` → `%APPDATA%/filar/`
- GUI title: "Warp — Launcher" → "Filar — Launcher"
- Usage: `filar [--target ...]`
- config.toml: "# filar configuration"
- Docker: `warp-sshd` → `filar-sshd`

#### 5.12 Иконки приложения — ВЫПОЛНЕНО

**Файлы:** `crates/app/build.rs`, `crates/gui/src/lib.rs`, `crates/app/Cargo.toml`, `crates/gui/Cargo.toml`

Два типа иконок:

**.exe иконка (в Explorer):**
- `crates/app/build.rs` — кастомный build script (НЕ winres crate)
- Находит `pics/filar.ico` через `CARGO_MANIFEST_DIR`
- Пишет `.rc` файл (ICON + VERSIONINFO)
- Компилирует `windres` (из `C:\Users\AdminLocal\mingw\mingw64\bin\`) → `.o` (COFF)
- `cargo:rustc-link-arg=<path/to/filar_resource.o>` — ПРЯМАЯ передача в линкер
- **ВАЖНО:** `winres` crate НЕ работает на GNU — `ld` выбрасывает unreferenced объекты из static libraries. Только `cargo:rustc-link-arg` гарантирует включение ресурса.
- VersionInfo: FileDescription = "Filar - Terminal with AI Agent", ProductName = "Filar"

**Иконка окна (при запуске):**
- `crates/gui/src/lib.rs` — функция `load_icon()`
- `include_bytes!("../../../pics/filar_256.png")` — встраивание PNG в бинарник
- `image::load_from_memory()` → декодирование в RGBA
- `egui::IconData { rgba, width, height }`
- Зависимость: `image = { version = "0.25", default-features = false, features = ["png"] }`

**Папка `pics/`** содержит: filar.ico, filar_256.png, filar_128.png, filar_64.png,
filar_512.png, filar_1024.png, SVG-исходники.

---

## 6. Ключевые архитектурные решения

### Swappable TuiExecutor
```
TuiExecutor.inner: Arc<RwLock<Arc<dyn CommandExecutor>>>
```
Позволяет менять исполнителя (Local ↔ SSH) в runtime без перезапуска TUI.

### AgentEvent::TransportChanged
```rust
TransportChanged { is_local: bool, ssh_info: Option<String> }
```
Runner перехватывает это событие перед `handle_agent_event`, обновляет
`is_local` и `ssh_info` переменные, и обновляет `app.target_name`.
`handle_agent_event` обрабатывает как no-op.

### Динамический системный промпт
Системный промпт строится функцией `build_system_prompt()` при каждом
`spawn_agent()`. Параметры: `is_local`, `ssh_info`, `is_windows`.
При `TransportChanged` — следующий `spawn_agent()` получит новый промпт.

### Безопасность паролей
- Пароли НИКОГДА не пишутся в `settings.json`
- API key → Windows Credential Manager (через `keyring` crate)
- SSH пароли → по галочке в Credential Manager
- В TUI: пароли через `$FILAR_SECRET_N` переменные (маскированные)
- Ctrl+P → masked password input → secret variable

---

## 7. Зависимости воркспейса (из Cargo.toml)

```toml
tokio = { version = "1", features = ["full"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"
thiserror = "1"
anyhow = "1"
async-trait = "0.1"
russh = "0.61"
ssh-key = "0.7.0-rc.10"
uuid = { version = "1", features = ["v4"] }
portable-pty = "0.8"
bytes = "1"
futures = "0.3"
tokio-util = "0.7"
reqwest = { version = "0.12", default-features = false, features = ["json", "rustls-tls", "stream"] }
ratatui = { version = "0.28", features = ["all-widgets"] }
crossterm = { version = "0.28", features = ["event-stream"] }
alacritty_terminal = "0.26"
eframe = { version = "0.29", default-features = false, features = ["glow", "default_fonts"] }
keyring = { version = "3", default-features = false, features = ["apple-native", "windows-native", "sync-secret-service"] }
image = { version = "0.25", default-features = false, features = ["png"] }
tracing = "0.1"
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

---

## 8. Тесты

- **150 unit-тестов** проходят:
  - filar-agent: 42 теста (agent loop, tools, security, GLM client, SSE parser)
  - filar-transport: 2 теста (marker format, payload format) + 3 ignored (Docker)
  - filar-tui: 106 тестов (terminal model, key mapping, app state, layout, streaming)
- **1 pre-existing failure:** `parse_minimal_config` в filar-core (ожидает `Always`,
  дефолт `Allowlist`) — unrelated к текущим изменениям

```powershell
cd c:\dev\warper
cargo test -p filar-tui -p filar-agent -p filar-transport
```

---

## 9. Известные проблемы и ограничения

1. **Windows Explorer icon cache** — после пересборки .exe может показывать
   старую иконку. Решение: копирование .exe в новый файл, или перезагрузка,
   или `ie4uinit.exe -show`, или очистка `%LOCALAPPDATA%\IconCache.db`
2. **parse_minimal_config test** — pre-existing failure, unrelated
3. **Interactive terminal** — Ctrl+T режим работает, но требует доработки
   OSC-маркеров для блоков команд
4. **SSH agent auth** — TODO (возвращает ошибку), работает только password/key

---

## 10. Как продолжить

1. Прочитать этот файл и `PLAN.md`
2. Установить PATH (см. раздел 2)
3. `cargo build` — собрать проект
4. `cargo test` — запустить тесты
5. Для отладки иконок: `filar.exe` в Explorer + Свойства → Подробно
6. Для тестирования SSH: `!ssh user@host` в Normal режиме → Ctrl+P для пароля
7. Все изменения в коде — на русском языке в комментариях и сообщениях пользователю

---

## 11. Недавние изменения (R1 milestone)

### Issue #2: Системный промпт противоречит killer-фиче
- **Файл:** `crates/agent/src/agent.rs`, функция `build_system_prompt`
- **Проблема:** промпт говорил «shell state does NOT persist» для всех режимов,
  включая SSH, где состояние персистентного канала сохраняется между командами.
- **Фикс:** `shell_desc` теперь зависит от `is_local`:
  - Local (Windows/POSIX): «does NOT persist» (соответствует `LocalExecutor` —
    каждая команда в отдельном процессе).
  - SSH: «DOES persist ... carry over» (соответствует персистентному каналу `SshSession`).
- **Тесты:** добавлены `ssh_prompt_states_persistence` и `local_prompt_states_no_persistence`.
- **Публичные контракты:** без изменений — `build_system_prompt` сигнатура та же.

### Issue #3: cancel() не может прервать выполняющуюся команду
- **Файл:** `crates/transport/src/ssh.rs` — `SshSession`, `SshExecutor`
- **Проблема:** `run()` держал `self.inner.lock().await` всё время выполнения команды
  (до 120 c). `cancel()` лочил тот же мьютекс → Ctrl-C проходил только после
  завершения команды. Прерывание зависшей команды фактически не работало.
- **Фикс — reader-task архитектура:**
  - На `connect` заспавнен долгоживущий таск, который **единолично владеет**
    `Channel<Msg>`. Таск в цикле `tokio::select!` читает из канала
    (`channel.wait()`) и принимает команды из `mpsc` (`cmd_rx`).
  - `ChannelCmd::Write(Vec<u8>)` — запись в канал; `ChannelCmd::Interrupt` —
    отправка `\x03` (Ctrl-C).
  - `ChannelEvent::Data(String)`, `ChannelEvent::Stderr(String)`,
    `ChannelEvent::Closed` — события наружу через `mpsc::unbounded_channel`.
  - `SshSession` хранит: `cmd_tx` (отправка команд в таск), `run_lock: Mutex<()>`
    (сериализация команд), `event_rx: Mutex<UnboundedReceiver>` (чтение событий).
  - `run()` шлёт payload через `cmd_tx`, читает события из `event_rx` — **не
    держа** лока, который нужен `cancel()`.
  - `cancel()` просто шлёт `Interrupt` в `cmd_tx` — мгновенно, без contention.
- **Тесты:** добавлен интеграционный тест `ssh_cancel_interrupts_long_command`
  (`#[ignore]`): `sleep 30` прерывается через 1 c, общая длительность < 15 c,
  последующий `echo ok` возвращает `ok` с exit code 0.
- **Публичные контракты:** без изменений — `SshSession::run/cancel/close` и
  `SshExecutor::run/cancel` сигнатуры те же. `SshSessionInner` удалён (внутренняя
  деталь реализации).
- **Review fixes (CodeRabbit PR #9):**
  - Маркер команды дополнен UUID (`req_{id}_{uuid}`) для защиты от коллизий.
  - Drain stale events перенесён **до** отправки команды (был после — мог
    терять вывод быстрых команд).
  - `debug!` логирует только kind + length, не raw content (безопасность).
  - Тесты используют `env::var("SSH_PASSWORD").expect(...)` вместо хардкода.
  - Тест обёрнут `timeout(2s, cancel())` для проверки latency самого `cancel()`.

### Issue #4: host key не проверяется, MITM
- **Файлы:** `crates/core/src/config.rs`, `crates/core/src/lib.rs`,
  `crates/transport/src/ssh.rs`, `crates/transport/src/interactive.rs`,
  `crates/tui/src/runner.rs`, `crates/app/src/main.rs`
- **Проблема:** `check_server_key` всегда возвращал `Ok(true)` — любой MITM
  проходил незамеченным.
- **Фикс — TOFU (Trust On First Use):**
  - Добавлен `HostKeyPolicy` enum в `config.rs`: `Strict`, `Tofu` (default),
    `AcceptNew`. Сериализация `snake_case`.
  - Поле `host_key_policy: HostKeyPolicy` добавлено в `SshTarget` с
    `#[serde(default)]`.
  - `SshHandler` (ssh.rs) теперь содержит поля: `host`, `port`, `policy`,
    `known_hosts_path`.
  - `check_server_key` вычисляет SHA256 fingerprint, проверяет known_hosts.
    - `Match` → accept.
    - `Mismatch` → reject (`Ok(false)`).
    - `New` → зависит от policy: `Strict` reject, `Tofu` accept+save,
      `AcceptNew` accept без сохранения.
  - Known_hosts файл: `~/.config/filar/known_hosts`, формат `host:port SHA256:fp`.
  - Хелперы: `known_hosts_path()`, `parse_known_hosts_contents()`,
    `parse_known_hosts()`, `append_known_hosts_entry()`, `check_host_key()`.
  - `interactive.rs` и `runner.rs` обновлены для construction `SshHandler` с
    полями и `SshTarget` с `host_key_policy`.
- **Тесты (5 unit):** `known_hosts_parse_contents`, `known_hosts_append_and_read`,
  `host_key_check_match`, `host_key_check_mismatch`, `host_key_check_new`.
- **Публичные контракты:** `HostKeyPolicy` добавлен в re-exports `filar-core`.
  `SshTarget` получил новое поле (backward-incompatible для ручной инициализации,
  но serde-совместимо через `#[serde(default)]`).
- Total: 70 tests pass, 0 fail, 5 ignored (Docker).
- **Review fixes (CodeRabbit PR #10):**
  - `parse_known_hosts` возвращает `Result` вместо silent empty map. Только
    `NotFound` → пустая карта (first connection); остальные I/O ошибки →
    reject (fail closed).
  - TOFU-путь: если `append_known_hosts_entry` не удался → reject (`Ok(false)`)
    вместо accept с warning. Ключ должен быть закреплён, иначе подключение
    не должно проходить.

### Issue #5: Косметика и грубые эвристики
- **Файлы:** `crates/transport/src/ssh.rs`, `crates/agent/src/security.rs`
- **Часть A — лишняя пустая строка в выводе:**
  - **Проблема:** printf-маркер использует ведущий `\n` для надёжного детекта
    начала строки. Этот `\n` попадал в output как лишний хвостовой перевод строки.
  - **Фикс:** после извлечения `output` из буфера срезаем ровно один хвостовой
    `\n` через `strip_suffix('\n')` — синтетический от printf, не трогая вывод команды.
  - **Критерий:** `run("echo hi")` даёт `stdout == "hi\n"` без второго пустого ряда.
- **Часть B — грубый `writes_to_system_path`:**
  - **Проблема:** функция проверяла «где-то после `>`» встречается ли системный путь.
    Ложное срабатывание: `grep x > /tmp/a; cat /etc/passwd` → true (из-за `/etc/`
    в read-части, а не в redirect).
  - **Фикс:** переписана — теперь разделяет по `;&|&`, находит каждый `>` или `>>`,
    извлекает **непосредственно следующий токен** (цель редиректа) и проверяет
    только его. `/dev/null` исключён (null device, не системный путь).
  - **Критерий:** `writes_to_system_path("echo foo > /etc/passwd") == true`,
    `writes_to_system_path("grep x > /tmp/a; cat /etc/passwd") == false`.
- **Тесты:** `detect_system_redirect` обновлён — 7 кейсов (включая `/dev/null`
  исключение, `>>` append, system path в read-части, `/dev/sda` device).
- **Публичные контракты:** без изменений.
- Total: 70 tests pass, 0 fail, 5 ignored (Docker).
- **Review fixes (CodeRabbit PR #11):**
  - `writes_to_system_path` использует `char_indices()` вместо `chars()` для
    byte-safe offset'ов. Non-ASCII текст перед `>` больше не вызывает panic
    при слайсинге строки.
  - Quoted redirect targets: `trim_matches` снимает кавычки (`"`, `'`) с цели
    редиректа перед проверкой. `echo foo >"/etc/passwd"` теперь корректно
    распознаётся как запись в системный путь.
  - Тесты расширены: quoted paths (`"/etc/passwd"`, `'/etc/passwd'`) и
    non-ASCII перед `>` (`echo привет > /etc/passwd`).

### Issue #6: Отвечать на языке исходного запроса
- **Файл:** `crates/agent/src/agent.rs`, функция `build_system_prompt`
- **Проблема:** язык ответа жёстко зашит как русский в двух местах промпта:
  строка `Always respond in Russian` и правило №6 `final answer in Russian`.
- **Фикс:**
  - Удалены обе зашитые ссылки на русский.
  - Вместо `Always respond in Russian` — инструкция зеркалирования: определить
    язык **первого** запроса пользователя и писать все пояснения, сводки,
    вопросы и финальный ответ на том же языке. Сырой вывод команд
    (stdout/stderr) не переводится — только prose агента.
  - Правило №6: `final answer in the user's language` вместо `in Russian`.
- **Тест:** `prompt_mirrors_user_language` — проверяет отсутствие `Russian`
  в промпте, наличие `user's` + `same language`, и оговорку про неперевод
  вывода команд (`must NOT be translated`).
- **Публичные контракты:** без изменений — `build_system_prompt` сигнатура та же.
- Total: 71 tests pass, 0 fail, 5 ignored (Docker).

### Issue #13: TUI — модуль темы и рефакторинг рендера
- **Файлы:**
  - `crates/tui/src/ui.rs` → удалён, разбит на модуль `crates/tui/src/ui/`
  - `crates/tui/src/ui/mod.rs` — `pub fn render()` + layout + `render_interactive()`
  - `crates/tui/src/ui/theme.rs` — `Theme` struct, `default_dark()`, хелперы стилей
  - `crates/tui/src/ui/text.rs` — `strip_emoji`, `wrap_text` (перенесены без изменений)
  - `crates/tui/src/ui/bars.rs` — `render_status_bar`, `render_help_bar`
  - `crates/tui/src/ui/chat.rs` — `render_chat_history`
  - `crates/tui/src/ui/input.rs` — `render_input_area` (Normal, Thinking, Confirming, PasswordInput)
  - `crates/tui/src/app.rs` — добавлено поле `pub theme: Theme`
  - `crates/tui/src/lib.rs` — реэкспорт `Theme`
  - `crates/tui/src/runner.rs`, `crates/tui/src/terminal.rs` — pre-existing clippy фиксы
- **Что сделано:**
  - Создан `Theme` struct с 10 семантическими токенами (bg, fg, fg_dim, fg_muted,
    accent, success, warning, danger, surface, selection_bg).
  - `Theme::default_dark()` — единая точка цветов для всего UI.
  - Хелперы: `user_style()`, `agent_style()`, `error_style()`, `command_style()`,
    `muted()`, `dim()`, `fg_style()`, `surface_style()`, `help_bar_style()`,
    `target_badge_style()`, `mode_badge_style()`, `mode_color()`.
  - `ui.rs` (440 строк) разбит на 5 модулей по зоне ответственности.
  - Все `Color::*` литералы — только в `theme.rs` (DoD: ни одного вне).
  - Экземпляр темы хранится в `App.theme`, рендереры обращаются к `app.theme.*`.
- **Решение по Magenta:** Interactive и PasswordInput режимы раньше использовали
  `Color::Magenta`. По дизайн-философии (§2: «один акцентный цвет») они переведены
  на `accent` (Cyan). Это единственное видимое изменение — зафиксировано в доке
  `theme.rs` и в тесте `mode_color_mapping`.
- **Pre-existing clippy фиксы** (не часть issue, но нужны для DoD `cargo clippy -D warnings`):
  - `app.rs`: `manual_strip` → `strip_prefix`, `collapsible_match` → вложенный паттерн
  - `runner.rs`: `manual_strip` → `strip_prefix`, `too_many_arguments` → `#[allow]`
  - `terminal.rs`: `map_or(false,…)` → `is_some_and(…)`, `unnecessary_cast` → убраны
- **Тесты:** 3 новых в `theme.rs` (colors, mode_color, style_helpers), 5 в `text.rs`
  (strip_emoji, wrap_text). Total: 24 tui tests pass.
- **Публичные контракты:** `Theme` реэкспортирован из `filar-tui`. `App` получил
  новое поле `theme` (backward-incompatible для ручной инициализации, но `App::new()`
  и `App::with_history()` работают без изменений).
- **Review fix (CodeRabbit PR #24):** `ChatBlock::System` — добавлен `strip_emoji`
  для системных сообщений (могут содержать user-controlled текст: target_name,
  SSH user/host). Теперь все варианты `ChatBlock` проходят emoji-фильтрацию.

### Issue #14: TUI — кэширование layout чата (фундамент для мыши)
- **Файлы:**
  - `crates/tui/src/ui/layout_cache.rs` — новый модуль: `RenderedLine`, `LineRegion`, `ChatLayoutCache`
  - `crates/tui/src/ui/chat.rs` — переписан: использует кэш вместо per-frame rebuild
  - `crates/tui/src/ui/mod.rs` — `render()` и `render_interactive()` принимают `&mut App`
  - `crates/tui/src/ui/input.rs` — `render_input_area()` принимает `&mut App`, записывает `input_area`
  - `crates/tui/src/app.rs` — новые поля `layout_cache`, `message_rev`, `chat_area`, `input_area`,
    `confirm_button_areas`; метод `push_message()`; все `self.messages.push(...)` заменены
  - `crates/tui/src/runner.rs` — `ui::render(f, &mut app)`; bump `message_rev` в error path
- **Что сделано:**
  - `ChatLayoutCache` хранит pre-rendered `Vec<RenderedLine>` с метаданными
    (`block_index`, `LineRegion`) для будущего hit-testing.
  - Кэш инвалидируется при: изменении ширины, изменении `messages.len()`, изменении `message_rev`.
  - `rebuild()` переносит логику построения строк из `chat.rs` (wrapping, emoji strip,
    output truncation at 30 lines).
  - `MAX_CACHED_LINES` поднят с 500 до 2000 — кэш делает per-frame cost = slice.
  - `App::push_message()` — единая точка мутации `messages` + bump `message_rev`.
    Все `self.messages.push(...)` заменены на `self.push_message(...)`.
  - In-place update последнего Command блока (CommandExecuted event) — bump `message_rev`
    для инвалидации кэша.
  - `app.chat_area` и `app.input_area` заполняются при каждом рендере (для задачи 3).
- **Решения:**
  - `push_message()` оставлен приватным — runner bump’ит `message_rev` вручную для
    единственного `app.messages.push` вне `App` (error path в interactive terminal).
  - `collapsed: &HashSet<usize>` параметр в `rebuild()` зарезервирован для задачи 6
    (collapse/expand output) — пока всегда пустой.
  - `LineRegion::OutputToggle` помечает строку `... (N more lines)` — будущий target
    для клик-Expand (задача 6).
- **Тесты:** 6 новых в `layout_cache.rs` (invalidation on width/message/rev,
  no-rebuild on same params, region correctness, command output + toggle).
  Total: 30 tui tests pass.
- **Публичные контракты:** `App` получил 5 новых полей (backward-incompatible для
  ручной инициализации, но `App::new()` и `App::with_history()` работают без изменений).
  `ui::render()` сигнатура изменена: `&App` → `&mut App`.
- **Review fixes (CodeRabbit PR #25):**
  - Добавлен `pub fn push_error()` — единая точка для внешних (runner) мутаций
    `messages` с автоматическим bump `message_rev`. Runner больше не делает
    прямой `app.messages.push(...)` + ручной bump.
  - Добавлены 7 тестов в `app.rs` на `message_rev`-bumping paths: `push_error`,
    `enter_interactive`, `exit_interactive`, `AgentEvent::TextResponse`,
    `AgentEvent::Error`, `AgentEvent::CommandExecuted` (in-place update),
    `respond_to_confirmation` (via handle_key 'a' in Confirming mode).
  Total: 37 tui tests pass.

### Issue #15: TUI — захват мыши и скролл колесом
- **Файлы:**
  - `crates/tui/src/runner.rs` — `EnableMouseCapture`/`DisableMouseCapture` в init/teardown;
    обработка `Event::Mouse(m)` в event loop
  - `crates/tui/src/app.rs` — `handle_mouse()`, `clamp_scroll()`; `End` key при пустом
    вводе сбрасывает scroll; `End` добавлен в Thinking/Confirming; `clamp_scroll()` после PageUp
  - `crates/tui/src/ui/chat.rs` — definitive scroll clamp в render; индикатор `↓ N new`
    в правом нижнем углу chat area (тусклый цвет `theme.fg_muted`)
- **Что сделано:**
  - Mouse capture включается при старте и выключается при выходе (оба пути,
    включая ошибочный) — OS text selection работает после закрытия приложения.
  - `handle_mouse()`: `ScrollUp` → scroll += 3, `ScrollDown` → scroll -= 3;
    только внутри `chat_area`; игнорируется в Interactive/PasswordInput.
  - `clamp_scroll()`: clamp к `layout_cache.lines.len().saturating_sub(visible_height)` —
    нельзя укрутить в пустоту. Вызывается после mouse wheel и PageUp.
    Дублируется в `render_chat_history` для definitive clamp (точный visible_height).
  - `End` key: при пустом вводе → scroll = 0 (Normal/Thinking/Confirming);
    при непустом вводе в Normal → cursor в конец (как раньше).
  - Индикатор `↓ N new` где N = scroll (после clamp) — количество строк ниже вьюпорта.
- **Решения:**
  - `↓` (U+2193) — basic Unicode, рендерится в Windows Terminal и conhost.
    Glyphs-struct fallback (DESIGN_PHILOSOPHY §7) — отдельная задача, не эта.
  - `clamp_scroll` использует `chat_area` и `layout_cache.lines` из последнего рендера —
    best-effort; definitive clamp в render использует точные значения.
  - Mouse events за пределами `chat_area` игнорируются (не клики по input/help bar).
- **Тесты:** 11 новых в `app.rs`: scroll up/down, clamp to max/zero, ignored outside
  chat area, ignored in Interactive, End key (empty/nonempty input, Thinking,
  Confirming), PageUp clamp. Total: 48 tui tests pass.
- **Публичные контракты:** `App::handle_mouse()` — новый public метод (для runner).
  `clamp_scroll()` — приватный. End key в Normal изменил поведение: пустой ввод →
  scroll reset вместо cursor-to-end (backward-incompatible, но старый behavior
  остаётся при непустом вводе).
- **Review fixes (CodeRabbit PR #26):**
  - Fixed indicator width: `indicator.len()` (bytes) → `indicator.chars().count()`
    (display columns). `↓` (U+2193) is 3 bytes but 1 terminal column, so byte length
    overestimated width by 2 and mispositioned the indicator.

### Issue #16: TUI — скроллбар и hit-testing кликов
- **Файлы:**
  - `crates/tui/src/app.rs` — `HitZone` enum, `DragKind` enum, new fields (`mouse_drag`,
    `indicator_area`, `status_bar_area`, `help_bar_area`); `hit_test()`, `update_scrollbar_drag()`,
    `set_cursor_from_click()`; `handle_mouse()` полностью переписан для routing всех зон
  - `crates/tui/src/ui/chat.rs` — scrollbar rendering (`Scrollbar`, `ScrollbarState`);
    `indicator_area` stored in App for click detection
  - `crates/tui/src/ui/bars.rs` — `render_status_bar` / `render_help_bar` принимают `&mut App`,
    store `status_bar_area` / `help_bar_area`
- **Что сделано:**
  - **Scrollbar:** `Scrollbar::new(VerticalRight)` с `theme.dim()` thumb и `theme.muted()` track.
    Показывается только когда `total_lines > visible_height`. Position = `skip` (first visible line).
  - **Drag по скроллбару:** `Down(Left)` в колонке скроллбара → `mouse_drag = Some(Scrollbar)`,
    scroll пересчитывается пропорционально row. `Drag(Left)` продолжает обновлять.
    `Up(Left)` сбрасывает `mouse_drag = None`.
  - **`hit_test(col, row)`:** приватный метод, routing по зонам: `ScrollIndicator` (first,
    overlays chat), `Scrollbar`, `Chat { line_idx }`, `ChatEmpty`, `Input`, `StatusBar`,
    `HelpBar`, `ConfirmButton(bool)`, `Outside`. `line_idx` вычисляется из row, `chat_area`,
    `scroll` через `layout_cache`.
  - **Клик по `↓ N new`:** `Down(Left)` в `indicator_area` → `scroll = 0`.
  - **Клик в input:** `Down(Left)` в `input_area` (Normal mode only) → `cursor_pos` из
    row/col (reverse of `place_cursor` math: `pos = row * inner_width + col`, clamped).
- **Решения:**
  - `ConfirmButton(bool)` в HitZone enum включён для полноты, но `confirm_button_areas`
    пока не заполняется при рендере — это будущая задача.
  - Scrollbar рисуется на full `area` (поверх правой рамки) — стандартный паттерн ratatui.
  - `hit_test` — приватный (тесты в том же модуле имеют доступ).
  - `DragKind::Selection` зарезервирован для будущего text selection (не эта задача).
- **Тесты:** 17 новых в `app.rs`: hit_test по всем зонам (Chat, ChatEmpty, Scrollbar,
  Scrollbar-not-visible, Input, StatusBar, HelpBar, Outside, ScrollIndicator, line_idx
  with scroll), scrollbar drag (proportional, mouse_up clears), click indicator, click
  input (cursor set, second row, clamp to end, ignored in Thinking). Total: 65 tui tests.
- **Публичные контракты:** `HitZone`, `DragKind` — новые public enums. `App` получил 4 новых
  поля. `render_status_bar` / `render_help_bar` сигнатура: `&App` → `&mut App`.
- **Review fixes (CodeRabbit PR #27):**
  - Fixed `update_scrollbar_drag` divisor: `visible_height` → `visible_height - 1`
    (track span). Old formula prevented thumb from reaching `scroll = 0` at bottom.
    Updated test to assert `scroll == 0` at bottom.

## Issue #17: TUI: модальное подтверждение команд с кликабельными кнопками

PR: #28

**Задача:** Заменить текстовое подтверждение в нижней панели на центрированный
модальный диалог с кликабельными кнопками Approve / Deny.

**Файлы:**
- `crates/tui/src/ui/confirm.rs` — НОВЫЙ модуль: рендеринг модального диалога
  (Block с Rounded borders, Clear под ним, кнопки с hit-test areas)
- `crates/tui/src/app.rs` — новые поля `confirm_selected` (bool, default false=Deny)
  и `hovered_button` (Option<bool>); обновлён `handle_key` (Enter → активирует
  selected, Tab/←/→ → toggle); обновлён `handle_mouse` (click на кнопку →
  respond, Moved → hover tracking); `hit_test` — confirm buttons проверяются
  первыми (поверх всего); `confirm_selected` сбрасывается при новом
  ConfirmationRequest
- `crates/tui/src/ui/mod.rs` — `mod confirm;`, рендер модала после всех зон
  если mode == Confirming
- `crates/tui/src/ui/input.rs` — `render_confirm` показывает приглушённый
  `waiting for confirmation…` (layout не прыгает); убраны старые импорты
  `Line`/`Span`
- `crates/tui/src/ui/bars.rs` — help-bar для Confirming: `Tab=Switch | Enter=Confirm | a/y=Approve | d/n=Deny | Ctrl+C=Quit`

**Решения:**
- Enter теперь активирует выделенную кнопку (дефолт Deny) — согласованное
  изменение из DESIGN_PHILOSOPHY §6. Безопаснее, чем безусловный approve.
- Hover перемещает selection на кнопку под курсором — интуитивный UX.
- `confirm_button_areas` проверяются в `hit_test` первыми — модал поверх всего.
- Кнопки: `[ Approve (a) ]` / `[ Deny (d) ]`, inversion (fg↔bg) для выбранной,
  `theme.surface` bg для невыбранной. 3 пробела между кнопками.
- Рамка: `BorderType::Rounded`, `danger` для destructive, `warning` иначе.
- Title: ` Confirm command ` (ASCII-safe, без `⚠`).

**Тесты:** 16 новых в `app.rs`: confirm_selected defaults, Tab/Left/Right toggle,
  Enter activates selected (default deny, after tab approve), letter hotkeys
  (a/d, Russian ф/в), Ctrl+C denies+quits, confirm_selected resets on new
  request, mouse click Approve/Deny, mouse hover updates selected, hit_test
  confirm button overrides chat. Total: 81 tui tests.

**Публичные контракты:** `App` получил 2 новых поля: `confirm_selected: bool`,
  `hovered_button: Option<bool>`. Новый модуль `ui::confirm`.
  `render_confirm` в `input.rs` больше не рисует диалог — только muted placeholder.
  Help-bar текст для Confirming изменён.
- **Review fixes (CodeRabbit PR #28):**
  - Fixed stale hit-test state: `respond_to_confirmation` now clears
    `confirm_button_areas` and `hovered_button` so old button rects don't
    swallow clicks after modal closes. Added regression test.
  - Fixed modal sizing: `estimate_wrapped_rows` helper computes wrapped line
    count so `Constraint::Length` doesn't clip long text. Min width 30 → 32.
  - Fixed title color: hardcoded `danger` → `border_color` (warning for
    non-destructive commands).

### Issue #18: Сворачиваемые блоки команд по клику (task 6)

**Ветка:** `feat/18-collapsible-command-blocks`

**Что сделано:**
- Заменён жёсткий лимит 30 строк на collapse/expand: по умолчанию блоки с
  выводом > 6 строк свёрнуты до 5 строк + строка-переключатель
  `▸ … N more lines — click to expand`. Развёрнутые длинные блоки показывают
  `▾ collapse`. Страховочный потолок: 400 строк (`… truncated`).
- Компактный заголовок команды: `▸ $ command  ✓` (свёрнут) /
  `▾ $ command  ✓` (развёрнут). `✓` = success, `✗` = danger для denied.
  Если вывода нет (`output: None`) — стрелка не показывается.
  Команда перенесена из отдельной output-строки в заголовок.
- `collapsed_overrides: HashMap<usize, bool>` в `App` — пользовательские
  переопределения. Блоки не в map используют дефолт (> 6 строк → свёрнут).
- Клик по строке `OutputToggle` или по заголовку `Command` (с output)
  переключает collapse/expand. `message_rev` инкрементируется → кэш
  перестраивается.
- `collapsed_set()` в `App` вычисляет множество свёрнутых индексов из
  overrides + дефолтов, передаётся в `layout_cache.rebuild()`.
- `strip_emoji`: добавлен диапазон 0x2713–0x2717 (Dingbats: ✓ ✗).

**Файлы:**
- `crates/tui/src/app.rs` — `collapsed_overrides`, `collapsed_set()`,
  `toggle_collapse()`, handle_mouse Chat zone (OutputToggle + Header click)
- `crates/tui/src/ui/layout_cache.rs` — новый заголовок, collapse/expand логика,
  400-строк потолок
- `crates/tui/src/ui/chat.rs` — передаёт `app.collapsed_set()` вместо `HashSet::new()`
- `crates/tui/src/ui/text.rs` — whitelist 0x2713–0x2717

**Тесты:** 10 новых (92 tui total): collapsed_set defaults/overrides (4),
  toggle collapse (2), layout_cache collapsed shows 5 lines + expand toggle,
  expanded shows collapse toggle, short output no toggle, header arrow+status,
  no-output no arrow.

**Публичные контракты:** `App` получил `collapsed_overrides: HashMap<usize, bool>`.
  `ChatLayoutCache::rebuild()` теперь получает реальные collapsed-данные.
  Заголовок Command блока изменился: `> Command [ok]` → `▾ $ command  ✓`.
  `strip_emoji` whitelist расширен диапазоном 0x2713–0x2717.
- **Review fixes (CodeRabbit PR #29):**
  - Truncation marker `… truncated (N more lines)` changed from `OutputToggle`
    to `Output` region — it's informational, not clickable. Only `▾ collapse`
    remains as the toggle.
  - Stale doc comment on `rebuild()` updated: `collapsed` is now populated
    by `app.collapsed_set()`, not "reserved for task 6".
  - Extracted `default_collapsed_for()` helper to deduplicate the `> 6 lines`
    threshold between `collapsed_set()` and `toggle_collapse()`.
  - Added 4 mouse click routing tests: OutputToggle click toggles, Header click
    toggles (with output), Header click no-op (without output), Body click
    no-op. Total: 96 tui tests.

### Issue #19: Agent+TUI — стриминг ответа LLM и спиннер

**Ветка:** `feat/19-llm-streaming-spinner`

**Задача:** SSE-стриминг ответа LLM с поблочным выводом текста в TUI и спиннером
в Thinking-режиме.

**Что сделано:**
- **SSE-стриминг в GLM-клиенте** (`crates/agent/src/glm.rs`):
  - `chat_stream()` — отправляет `"stream": true`, читает `bytes_stream()`,
    парсит SSE через stateful `SseState` парсер.
  - `SseState` — аккумулирует `buffer`, `full_text`, `tool_calls` (BTreeMap
    по `index`), флаг `done`. `process_chunk()` возвращает `Vec<String>`
    текстовых дельт. `into_response()` собирает финальный `ChatResponse`.
  - `send_stream_request()` — retry loop для initial connection (5xx/429/network).
- **LlmClient trait** (`crates/agent/src/lib.rs`):
  - `chat_stream()` — default метод с fallback на `chat()`.
  - Callback: `Fn(String)` вместо `Fn(&str)` — обходит HRTB-проблему
    `async_trait` (десугарит `for<'a> Fn(&'a str)` в конкретный lifetime).
- **AgentEvent::TextDelta** (`crates/tui/src/event.rs`) — новый вариант.
- **Agent loop** (`crates/agent/src/agent.rs`):
  - `on_text_delta: Option<Arc<dyn Fn(String) + Send + Sync>>` — callback.
  - Если callback установлен — `chat_stream()`, иначе `chat()`.
- **Runner** (`crates/tui/src/runner.rs`):
  - Streaming callback: клонирует `tx`, отправляет `TextDelta`.
  - Spinner tick: `app.tick` инкрементируется каждый render frame в Thinking.
  - `needs_clear` подавлен для `TextDelta` (анти-мерцание).
- **App streaming state** (`crates/tui/src/app.rs`):
  - `streaming: bool`, `tick: u64`, `spinner_char()` (braille `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏`
    в WT_SESSION, ASCII `|/-\` fallback).
  - `TextDelta`: append к последнему Agent блоку если streaming, иначе новый;
    auto-scroll только если `scroll == 0`.
  - `Finished`: заменяет streaming-блок авторитетным текстом.
  - `Error`: добавляет `System("response interrupted")` если streaming.
- **Input panel** (`crates/tui/src/ui/input.rs`): disabled frame с muted
  стилем, спиннер + `writing…` / `thinking…` + `(Ctrl+C to cancel)`.
- **Status bar** (`crates/tui/src/ui/bars.rs`): `{spinner} thinking`.
- **strip_emoji** (`crates/tui/src/ui/text.rs`): whitelist 0x2800–0x28FF.

**Решения:**
- `Fn(String)` вместо `Fn(&str)` — `async_trait` десугарит HRTB в конкретный
  lifetime, привязанный к async future. Owned `String` решает проблему.
- `process_chunk` возвращает `Vec<String>` вместо callback — та же причина.
- Braille спиннер только в Windows Terminal (`WT_SESSION` env).
- `needs_clear` подавлен для `TextDelta` — prevents мерцание.
- Auto-scroll только если `scroll == 0` — `↓ N new` индикатор растёт.

**Тесты:**
- 6 SSE parser tests (chunked text, tool calls, multiple tool calls,
  text+tool calls, empty stream, stream serialization).
- 10 streaming tests (append, new block, auto-scroll, reset, spinner,
  Finished, Error, ConfirmationRequest, CommandExecuted).
- Total: 42 agent tests + 106 tui tests pass.

**Публичные контракты:**
- `LlmClient` trait: новый метод `chat_stream()` (default impl).
- `AgentEvent`: новый вариант `TextDelta(String)`.
- `Agent`/`AgentBuilder`: новое поле `on_text_delta` + builder method.
- `App`: новые поля `streaming: bool`, `tick: u64`; метод `spinner_char()`.
- Callback: `Fn(&str)` → `Fn(String)`.

**Что дальше:**
- Issue #21: Keyboard shortcuts in Thinking mode.

### Issue #20: TUI — визуальный редизайн, markdown-lite, help-bar клики

**Ветка:** `feat/20-visual-redesign-markdown-lite`

**Задача:** Убрать «коробочность», сделать воздух и акценты. Markdown-lite для
сообщений агента. Кликабельный help-бар. Многострочный ввод.

**Что сделано:**

#### 1. Главный layout — без рамок
- **Статус-бар** (`bars.rs`): без сплошной заливки `DarkGray`. Слева `filar ▸ {target}`
  (accent на имени таргета), режим по центру/справа (словом + цветом), `confirm_mode`
  тускло справа. Разделитель `─` цветом `fg_muted`.
- **Help-бар** (`bars.rs`): без заливки фоном. Клавиши — `fg_dim`, описания — `fg_muted`,
  разделение тремя пробелами.
- **Роль-заголовки** (`layout_cache.rs`): строчные `you` / `agent` (bold + цвет роли).
  Тело — `fg` с отступом 2 пробела.
- **Блок команды**: заголовок из задачи 6 (✓/✗, ▸/▾). Строки вывода — gutter `│`
  цветом `fg_muted`.
- **System**: `· text` (`fg_muted`). **Error**: `✗ text` (`danger`).
- **Поле ввода** (`input.rs`): без рамки. Промпт `❯` (accent; ASCII `>`).
  Плейсхолдер `enter your message...` (`fg_muted`). При вводе `!` — промпт `$` и
  цвет `warning`.

#### 2. Help-бар с кликабельными зонами
- **`HelpAction` enum** (`app.rs`): Send, Shell, Terminal, Password, Quit, Switch,
  Confirm, Approve, Deny, SendPassword, Cancel.
- **`helpbar_zones: Vec<(Rect, HelpAction)>`** в `App` — заполняется при рендере
  help-бара (`bars.rs`).
- **Обработка кликов** (`handle_mouse`): клик по help-бару работает во ВСЕХ режимах
  (включая Interactive/PasswordInput). Метод `execute_help_action()` выполняет
  действие, соответствующее клавиатурному эквиваленту.

#### 3. Markdown-lite для сообщений Agent
- **`render_markdown_line()`** (`text.rs`): inline-парсинг `` `code spans` ``,
  `**bold**`, `# headers`, `- list markers`. Fenced-блоки через `MarkdownState`.
- **Незакрытые маркеры** — рендерятся как обычный текст (проверка наличия
  закрывающего маркера перед переключением состояния).
- **Стили** (`theme.rs`): `code_span_style()` (fg на surface), `bold_style()`
  (fg + bold), `header_style()` (accent + bold).

#### 4. Glyphs и ASCII-фоллбэки
- **`Glyphs` struct** (`theme.rs`): prompt, gutter, separator, success, danger,
  middle_dot, collapse_arrow, expand_arrow, bullet, target_sep.
- Детект по `WT_SESSION` env → Unicode; иначе ASCII.

#### 5. Многострочный рост поля ввода
- **`input_height()`** (`mod.rs`): вычисляет высоту поля ввода от wrap текста,
  до 5 строк максимум.
- **`render_normal_input()`** (`input.rs`): wraps input, рендерит каждую строку.
  Промпт только на первой строке, последующие — с отступом. Внутренний скролл
  к курсору при превышении 5 строк.

#### 6. Предсуществующие clippy-фиксы
- `filar-gui/src/lib.rs`: `match → unwrap_or_default()`.
- `filar-transport/src/ssh.rs`: redundant guard → pattern match, `loop → while let`,
  `match → if let`, `get().is_none() → !contains_key()`.
- `filar-agent/src/security.rs`: collapsible `if`, identical branches merged.

**Файлы:**
- `crates/tui/src/ui/bars.rs` — полный редизайн status/help баров + helpbar_zones
- `crates/tui/src/ui/input.rs` — многострочный ввод
- `crates/tui/src/ui/mod.rs` — динамическая высота input area
- `crates/tui/src/ui/text.rs` — markdown-lite с незакрытыми маркерами
- `crates/tui/src/ui/theme.rs` — Glyphs struct (предсуществовал, дополнен)
- `crates/tui/src/ui/layout_cache.rs` — unused import fix
- `crates/tui/src/app.rs` — HelpAction enum, helpbar_zones, execute_help_action
- `crates/gui/src/lib.rs` — clippy fix
- `crates/transport/src/ssh.rs` — clippy fixes
- `crates/agent/src/security.rs` — clippy fixes

**Тесты:** 11 новых (124 tui total): helpbar_zones init, HelpAction quit/terminal/
  password/shell/approve/deny/cancel/switch. Markdown tests: code span, bold,
  mixed, unclosed marker, unclosed bold, header, list marker.

**Публичные контракты:**
- `HelpAction` enum — новый public type.
- `App`: новое поле `helpbar_zones: Vec<(Rect, HelpAction)>`.
- `App::execute_help_action()` — приватный метод.
- `Glyphs` struct — предсуществовал в theme.rs.
- `render_markdown_line` сигнатура: `(&str, &Theme, &mut MarkdownState) -> Vec<Span>`.

---

## Issue #21: Выделение текста мышью и копирование в буфер

**Задача:** Вернуть нативное выделение текста в mouse-capture TUI. Drag мышью
выделяет текст, отпускание копирует в системный буфер. Двойной клик — слово,
тройной — строка. Выделение переживает скролл, сбрасывается при новых сообщениях.

**Что сделано:**

### 1. Зависимость arboard
- Добавлена `arboard = "3"` в workspace dependencies и `crates/tui/Cargo.toml`.
- Кроссплатформенный clipboard (Windows поддерживается из коробки).

### 2. Selection struct
- `Selection { anchor_line, anchor_col, head_line, head_col }` — координаты
  в пространстве `layout_cache.lines` (не экрана), переживает скролл.
- `normalised()` → `((start_line, start_col), (end_line, end_col))` —
  отсортированные пары для рендера/копирования.
- `is_empty()` — true если anchor == head.
- `DragKind::Selection` — новый вариант для отслеживания состояния drag.

### 3. Mouse events
- **Down(Left)** в чате (не toggle/header): старт выделения. Отслеживание
  двойного/тройного клика (< 400 ms, та же позиция).
- **Drag(Left)**: обновление `head` + автоскролл у верхней/нижней кромки.
- **Up(Left)**: copy-on-select — вызов `arboard::Clipboard::set_text()`,
  тост `· copied` на 1.5 сек. Если выделение пусто — очищается.

### 4. Двойной / тройной клик
- Double click → `select_word()`: максимальный run непробельных символов.
- Triple click → `select_line()`: вся строка целиком.
- Счётчик кликов зацикливается: 1 → 2 → 3 → 1.

### 5. Рендер выделения
- `apply_selection()` в `chat.rs`: проходит по spans видимых линий, разбивает
  на «до / выделено / после», накладывает `theme.selection_bg` на выбранный
  диапазон колонок. Поддерживает multi-line selection.

### 6. Toast уведомление
- `toast: Option<(String, Instant)>` в `App`.
- `toast_text()` — возвращает текст, если тост ещё активен.
- Рендерится в status-bar (`bars.rs`) после confirm_mode: `· copied` цветом
  `success_fg`.

### 7. Сброс выделения
- `push_message()` очищает `selection` — новые сообщения инвалидируют индексы.

**Файлы:**
- `Cargo.toml` — arboard workspace dependency
- `crates/tui/Cargo.toml` — arboard dependency
- `crates/tui/src/app.rs` — Selection struct, DragKind::Selection, handle_mouse
  обновлён, screen_to_line_col, line_text, selected_text, copy_selection_to_clipboard,
  select_word, select_line, toast_text, 20 новых тестов
- `crates/tui/src/ui/chat.rs` — apply_selection для рендера selection_bg
- `crates/tui/src/ui/bars.rs` — toast в status-bar
- `crates/tui/src/ui/theme.rs` — комментарий selection_bg обновлён

**Тесты:** 20 новых (145 tui total): selection normalised (forward/backward),
  is_empty, selected_text (single/multi/empty), select_word (middle/start),
  select_line, screen_to_line_col (map/exclude scrollbar/outside),
  mouse_down_starts_selection, mouse_drag_updates_head, mouse_up_clears_drag,
  push_message_clears_selection, toast (none/active/expired).

**Публичные контракты:**
- `Selection` struct — новый public type.
- `DragKind::Selection` — новый вариант.
- `App`: новые поля `selection: Option<Selection>`, `toast: Option<(String, Instant)>`.
- `App::toast_text()` — новый public метод.

---

## Issue #22: Мышь в интерактивном режиме терминала

**Задача:** Скролл истории терминала колесом и проброс мыши в приложения
(vim, htop, mc) в режиме Interactive (Ctrl+T).

### Что сделано

1. **TerminalModel API** (`crates/tui/src/terminal.rs`):
   - `scroll_display(delta: i32)` — скролл scrollback-истории через
     `term.scroll_display(Scroll::Delta(delta))`.
   - `scroll_to_bottom()` — сброс скролла в низ через `Scroll::Bottom`.
   - `mouse_mode() -> bool` — проверка `TermMode::MOUSE_MODE | SGR_MOUSE`
     (REPORT_CLICK / DRAG / MOTION + SGR).
   - `is_alt_screen() -> bool` — проверка `TermMode::ALT_SCREEN`.
   - Импорт `Scroll` из `alacritty_terminal::grid`.

2. **handle_interactive_mouse** (`crates/tui/src/app.rs`):
   - Если `mouse_mode() == true`: кодирование события в SGR-последовательность
     (`\x1b[<{button};{x};{y}M/m`, координаты 1-based относительно области
     терминала) и отправка в `pending_term_input`.
   - Иначе в alt-screen: колесо → стрелки `↑↑↑`/`↓↓↓` (по 3 на тик) —
     стандартное поведение для `less`/`man`.
   - Иначе (primary screen): колесо → `scroll_display(±3)`.
   - События вне `terminal_area` игнорируются.

3. **SGR mouse encoding** (`encode_sgr_mouse`):
   - Поддержка: Left/Right/Middle click, release (M/m), drag (32+button),
     motion (35), scroll (64/65).
   - Модификаторы: Shift(4), Alt(8), Ctrl(16).

4. **Сброс скролла при вводе** — клавиатурный ввод в Interactive вызывает
   `scroll_to_bottom()` перед отправкой байтов.

5. **terminal_area** — новое поле `App`, заполняется в `render_interactive`
   для hit-testing мышью.

6. **Help-bar** — добавлен `wheel scroll` в Interactive mode.

### Изменённые файлы

- `crates/tui/src/terminal.rs` — `scroll_display`, `scroll_to_bottom`,
  `mouse_mode`, `is_alt_screen`, импорт `Scroll`
- `crates/tui/src/app.rs` — `handle_interactive_mouse`, `encode_sgr_mouse`,
  `push_term_input`, поле `terminal_area`, сброс скролла при keyboard input,
  19 новых тестов
- `crates/tui/src/ui/mod.rs` — сохранение `terminal_area` в `render_interactive`
- `crates/tui/src/ui/bars.rs` — `wheel scroll` в Interactive help-bar

**Тесты:** 19 новых: scroll up/down (primary), alt-screen arrow translation,
  mouse outside area, SGR encoding (click/release/scroll/modifiers/right/
  middle drag/motion), mouse_mode default/enabled, alt_screen default/enabled,
  scroll_display, scroll_to_bottom, push_term_input (append/new).

**Публичные контракты:**
- `TerminalModel::scroll_display(i32)`, `scroll_to_bottom()`,
  `mouse_mode() -> bool`, `is_alt_screen() -> bool` — новые public методы.
- `App::terminal_area: Rect` — новое public поле.

**DoD (требует ручной проверки):**
- Колесо скроллит историю в интерактивном режиме.
- В `htop`/`mc` по SSH клики и колесо доходят до приложения.
- В `less` колесо листает (трансляция в стрелки).

---

## Issue #23: Полировка, устойчивость, документация, релиз 0.2.0

**Задача:** Финальная полировка TUI — стабильность layout, деградация без мыши,
обновление help-бара, документации и версии.

### Что сделано

1. **Стабильность layout** — убран `needs_clear` на non-TextDelta agent-события.
   Полный `terminal.clear()` теперь выполняется только при смене режима
   (`prev_mode != app.mode`). Редизайн (borderless layout) не оставляет
   артефактов, мерцание устранено.

2. **Деградация без мыши** — `EnableMouseCapture` теперь выполняется отдельно
   от `EnterAlternateScreen`. Ошибка mouse capture логируется `warn!` и не
   прерывает работу. Приложение работает без мыши (клавиатурные эквиваленты
   для всех действий).

3. **Help-бар обновлён** — Normal mode: добавлены `wheel scroll`, `click expand`,
   `drag copy`. Thinking mode: `pgup/pgdn` заменён на `wheel scroll`.

4. **Тесты** — добавлены:
   - Scroll clamp: zero when content fits, zero height no panic, exact fit.
   - Hit test: tiny terminal (40×5).
   - Markdown-lite: empty string, only markers, multiple code spans.
   - SSE parser: malformed data line, partial chunk.

5. **Документация:**
   - `README.md`: добавлены Mouse Support и Streaming Responses в Features;
     обновлена таблица Keyboard Shortcuts (Ctrl+T, mouse wheel/click/drag).
   - `USER_GUIDE.md`: добавлен раздел 4.2 «Управление мышью» с таблицей
     действий; обновлены горячие клавиши (Enter = confirm selected button,
     Tab, Ctrl+P); нумерация разделов сдвинута.

6. **Версия** — поднята до `0.2.0` в `workspace.package`.

### Изменённые файлы

- `crates/tui/src/runner.rs` — убран `needs_clear`, раздельный EnableMouseCapture
- `crates/tui/src/ui/bars.rs` — обновлены help-items для Normal и Thinking
- `crates/tui/src/app.rs` — 4 новых теста (scroll clamp + hit_test tiny)
- `crates/tui/src/ui/text.rs` — 3 новых теста (markdown-lite edge cases)
- `crates/agent/src/glm.rs` — 2 новых теста (SSE malformed + partial)
- `README.md` — Features + Keyboard Shortcuts обновлены
- `USER_GUIDE.md` — раздел «Мышь» + обновлённые хоткеи
- `Cargo.toml` — версия 0.2.0
- `PROGRESS.md` — этот раздел

**Тесты:** 9 новых (4 scroll clamp/hit_test + 3 markdown + 2 SSE).

**Публичные контракты:** без изменений.

**DoD (требует ручной проверки):**
- Smoke-тест: запуск → агент → стриминг → подтверждение кликом → разворот
  кликом → выделение/копия → Ctrl+T → скролл колесом → Ctrl+T → Ctrl+C →
  сессия сохранена.
- Ресайз 40×10 — нет паник.
- Запуск без mouse capture — работает.

---

## 25. Issue #40: TUI panic-hook — восстановление терминала при панике

**Milestone:** Engine v0.3.0. **Ветка:** `fix/40-panic-hook-terminal-restore`.

**Что сделано:**
- Добавлен `PanicHookGuard` — RAII-структура в `runner.rs`, устанавливающая
  panic-hook ДО `enable_raw_mode()`. Хук восстанавливает терминал (DisableMouseCapture,
  LeaveAlternateScreen, disable_raw_mode) ПЕРЕД печатью паники — текст виден и
  выделяется мышью.
- Hook снимается через `drop(_hook_guard)` ДО штатного teardown (для чистоты,
  чтобы избежать двойного DisableMouseCapture). На error-path снимается
  автоматически через Drop.
- Штатный путь выхода (Ctrl+C) не изменился.

**Изменённые файлы:**
- `crates/tui/src/runner.rs` — `PanicHookGuard` + установка в `run()`

**Тесты:** без изменений (242 passed, 0 failed, 5 ignored). Поведение проверяется
  ручным тестом (panic в debug-сборке → терминал восстановлен).

**Публичные контракты:** без изменений (`PanicHookGuard` — private).

**DoD (требует ручной проверки):**
- В debug-сборке вызвать панику внутри event loop → терминал в нормальном
  состоянии, текст паники читаемо и выделяется мышью.
- Штатный выход (Ctrl+C) работает как раньше.

---

## Issue #41: TUI: hover не должен менять действие Enter в диалоге подтверждения

**Задача:** Наведение мыши на кнопку подтверждения не должно менять
`confirm_selected` — safety-дефолт «Enter = Deny» должен сохраняться до
явного действия пользователя (Tab/←/→ или клик).

**Решение:**
- В `app.rs::handle_mouse` ветка `Moved`: удалена строка
  `self.confirm_selected = approve`. Hover обновляет только `hovered_button`
  (визуальная подсветка).
- В `ui/confirm.rs::render_buttons`: добавлена визуальная дифференциация —
  selected (активная для Enter) = инверсия + BOLD, hovered = UNDERLINED,
  обычная = plain. Убрано преждевременное `hovered_button = None` в начале
  рендера (состояние hover должно сохраняться между кадрами).
- Тесты: `mouse_hover_updates_selected` → переименован в
  `mouse_hover_does_not_change_confirm_selected` (assert: hover не меняет
  `confirm_selected`); добавлен `repeated_hover_does_not_change_confirm_selected`
  (многократный hover не меняет выбор).

**Изменённые файлы:**
- `crates/tui/src/app.rs` — фикс `Moved` + тесты
- `crates/tui/src/ui/confirm.rs` — hover styling + doc-comment

**Тесты:** 243 passed, 0 failed, 5 ignored.

**Публичные контракты:** без изменений.

**DoD:** наведение мыши не влияет на действие Enter; дефолт Deny сохраняется
  до явного действия пользователя.

---

## Issue #42: SSE — не терять хвост потока без завершающего перевода строки

**Задача:** При завершении потока (`stream.next() == None`) остатки `raw_buffer`
и `SseState.buffer` молча выбрасывались. Если сервер закрыл соединение без
завершающего `\n` — терялась финальная дельта или `[DONE]`.

**Решение:**
- В `chat_stream` ветка `None`: если `raw_buffer` непуст — декодировать остаток,
  прогнать через `state.process_chunk(&(leftover + "\n"))`, эмитнуть дельты.
- `SseState::flush()`: новый метод, обрабатывающий незавершённую строку в
  `self.buffer` как полную (добавляет `\n` и вызывает `process_chunk`).
- Вызывается после flush `raw_buffer` и до `into_response()`.

**Изменённые файлы:**
- `crates/agent/src/glm.rs` — `flush()` + flush в `chat_stream` + 3 теста

**Тесты:** 246 passed, 0 failed, 5 ignored.
  - `sse_flush_partial_line_without_newline` — дельта "end" не теряется
  - `sse_flush_done_without_newline` — `[DONE]` без `\n` обрабатывается
  - `sse_flush_empty_buffer_noop` — пустой хвост не меняет поведение

**Публичные контракты:** без изменений (`SseState` — private).

---

## Issue #43: Engine 0.1 — публичные события агента (AgentEvent + sink)

**Задача:** Создать UI-агностический `AgentEvent` enum + `EventSink` в `filar-agent`,
эмитить события во всех ключевых точках `Agent::run`. TUI должен использовать
`filar_agent::AgentEvent` без собственной копии. Дополнительно: `ChatResponse`
изменён с enum (Text XOR ToolCalls) на struct (text + tool_calls) чтобы
сохранять preamble-текст в истории при наличии tool calls.

**Решение:**

### Часть A: AgentEvent + EventSink
- **`crates/agent/src/events.rs`** (новый): `AgentEvent` enum (`#[non_exhaustive]`)
  с вариантами: `Started`, `TextDelta(String)`, `CommandProposed { command, explanation,
  destructive }`, `CommandFinished { command, output, denied }`, `Finished(String)`,
  `Error(String)`. `EventSink = Arc<dyn Fn(AgentEvent) + Send + Sync>`.
- **`crates/agent/src/lib.rs`**: модуль `events` публичный, реэкспорт `AgentEvent` + `EventSink`.
- **`crates/agent/src/agent.rs`**: `AgentBuilder::event_sink(sink)` — опциональный sink.
  `run()` рефакторен в `run()` + `run_loop()`: внешний `run()` эмитит `Started` →
  `run_loop()` → `Finished`/`Error`. `process_tool_call()` эмитит `CommandProposed`
  перед подтверждением и `CommandFinished` для всех исходов (blocked/denied/error/success).
- **TUI:** `event.rs` — старый `AgentEvent` enum заменён на `TuiEvent` с вариантами:
  `Agent(filar_agent::AgentEvent)`, `Thinking`, `ConfirmationRequest { ... }`,
  `TransportChanged { ... }`. `runner.rs::spawn_agent` — `EventSink` форвардит
  `AgentEvent` → `TuiEvent::Agent(...)`. `TuiExecutor` лишён `event_tx` — события
  команд идут через sink. `app.rs::handle_agent_event` — match на `TuiEvent::Agent`
  с вложенным match на `filar_agent::AgentEvent`.

### Часть B: ChatResponse → struct
- **`crates/agent/src/lib.rs`**: `ChatResponse` — struct с `text: String` и
  `tool_calls: Vec<ToolCall>` (оба поля всегда присутствуют). Конструкторы
  `text()` и `tool_calls(text, calls)`, метод `has_tool_calls()`.
- **`crates/agent/src/glm.rs`**: `try_into_chat_response()` и `into_response()`
  собирают оба поля. Тесты обновлены.

**Решения дизайна:**
- `run()`/`run_loop()` split — гарантирует ровно один `Finished`/`Error` на всех
  путях (включая max-iterations).
- `CommandFinished { denied: true }` — TUI handler пропускает update command block
  для denied команд, сохраняя старое поведение (блок уже добавлен в `ConfirmationRequest`).
- Shell escape (`!cmd`) — runner строит `CommandFinished` вручную из `CommandResult`,
  т.к. агент не запущен.

**Изменённые файлы:**
- `crates/agent/src/events.rs` — новый: `AgentEvent` + `EventSink`
- `crates/agent/src/lib.rs` — `ChatResponse` struct, модуль `events`
- `crates/agent/src/glm.rs` — обновлён под struct API
- `crates/agent/src/agent.rs` — `event_sink()`, `run()`/`run_loop()`, emit, 2 теста
- `crates/tui/src/event.rs` — `TuiEvent` (замена `AgentEvent`)
- `crates/tui/src/runner.rs` — `EventSink` bridge, `TuiExecutor` без `event_tx`
- `crates/tui/src/app.rs` — `handle_agent_event` на `TuiEvent`
- `crates/tui/src/confirmer.rs` — `TuiEvent` вместо `AgentEvent`
- `crates/tui/src/lib.rs` — реэкспорт `TuiEvent`

**Тесты:** 249 passed, 0 failed, 5 ignored.
  - `event_sink_sequence_tool_call` (DoD): mock LLM с одним tool call → sink получает
    Started → CommandProposed → CommandFinished → Finished
  - `event_sink_denied_command`: CommandFinished с `denied: true`

**Публичные контракты:**
- NEW: `filar_agent::AgentEvent` (enum, `#[non_exhaustive]`), `filar_agent::EventSink` (type alias)
- NEW: `AgentBuilder::event_sink(sink: EventSink) -> Self`
- CHANGED: `filar_agent::ChatResponse` — enum → struct (`text` + `tool_calls`)
- CHANGED: `filar_tui::event::TuiEvent` (was `AgentEvent`) — wrapping `filar_agent::AgentEvent`
- REMOVED: дубликаты TUI event-вариантов (Started, TextDelta, CommandExecuted, Finished, Error)

### Review fixes (PR #51, CodeRabbit)

- **TextDelta через оба хука**: `on_text_delta` и `event_sink` теперь работают
  одновременно — раньше `on_text_delta` полностью перекрывал sink.
- **Blocked ≠ denied**: для `ConfirmDecision::Blocked` больше не эмитится
  `CommandFinished` — blocked не является user denial, и TUI не должен показывать
  блок команды. Причина блокировки отправляется только в LLM как tool context.
- **println! в smoke-тестах**: удалены отладочные `println!` (AGENTS.md).
- **CommandProposed explanation**: TUI теперь сохраняет metadata из `CommandProposed`
  и использует её в `CommandFinished` для auto-approved команд (которые не прошли
  через `ConfirmationRequest`). Новое поле `App::pending_proposal`.
- **Double terminal event в shell escape**: `Finished` больше не эмитится после
  `Error` — только один терминальный event на запуск.

---

## Issue #44: Engine 0.2 — стриминг в LlmClient (сверка и закрытие)

**Задача:** Сверить, что стриминг в `LlmClient` полностью реализован, и закрыть
issue. Если чего-то не хватает — дополнить.

**Результат:** Все 3 шага уже реализованы в предыдущих задачах (#43, SSE flush).
Добавлен недостающий DoD-тест.

**Сверка по шагам:**
1. ✅ `LlmClient::chat_stream` с дефолтной реализацией-фолббэком через `chat()`
   — `crates/agent/src/lib.rs:40-48`
2. ✅ `GlmClient` реализует SSE-стриминг (`"stream": true`, парсинг `data:`-строк
   с буферизацией разрывов чанков, аккумуляция tool_calls по index, `[DONE]`)
   — `crates/agent/src/glm.rs:137-230`
3. ✅ `Agent::run` использует `chat_stream`, пробрасывая дельты в
   `AgentEvent::TextDelta` через sink — `crates/agent/src/agent.rs:307-319`

**Сверка по DoD:**
- ✅ Unit-тест SSE-парсера (разрыв посреди `data:`, tool_calls по кускам) —
  11 тестов в `glm.rs`, включая `sse_parse_text_stream_chunked`,
  `sse_parse_tool_calls_stream`, `sse_parse_partial_chunk`
- ✅ **Добавлен** `event_sink_streaming_text_delta` — мок-LLM со стримом
  (`MockStreamingLlm`) → sink получает TextDelta до Finished.
  Последовательность: Started → TextDelta×3 → Finished.
- ✅ Нестримящие реализации `LlmClient` продолжают работать (фоллбэк) —
  `MockLlm` реализует только `chat()`, все тесты проходят.

**Изменённые файлы:**
- `crates/agent/src/agent.rs` — `MockStreamingLlm` + тест `event_sink_streaming_text_delta`

**Тесты:** 250 passed, 0 failed, 5 ignored.

**Публичные контракты:** без изменений (сверка существующей реализации).

---

## Issue #45: Engine 0.3 — отмена и таймауты (CancellationToken в Agent::run)

**Задача:** Добавить возможность отмены выполняющегося агента через
`CancellationToken`, а также таймауты на подтверждение команды и выполнение
команды.

**Решение:**

### 1. CancellationToken в Agent
- `AgentBuilder::cancellation(token: CancellationToken)` — устанавливает токен.
- `with_cancellation()` — свободная функция, оборачивающая future в
  `tokio::select!` с `token.cancelled()`. Возвращает `Err("cancelled")`.
- `Agent::run()` при ошибке проверяет `is_cancelled()` и эмитит
  `AgentEvent::Cancelled` вместо `AgentEvent::Error`.
- LLM-запросы (`chat`, `chat_stream`), подтверждение команды и выполнение
  команды — все обёрнуты в `with_cancellation`.
- При отмене во время выполнения команды вызывается `executor.cancel()`.

### 2. AgentEvent::Cancelled
- Новый вариант в `AgentEvent` (terminal event, `#[non_exhaustive]`).

### 3. confirm_timeout(Duration)
- `AgentBuilder::confirm_timeout(duration)` — таймаут на подтверждение.
- При таймауте: `CommandFinished { denied: true, output: "Confirmation timed out" }`,
  команда считается denied, агент продолжает работу.

### 4. command_timeout(Duration)
- `AgentBuilder::command_timeout(duration)` — таймаут на выполнение.
- При таймауте: `executor.cancel()`, `CommandFinished { output: "Command timed out" }`,
  агент продолжает работу.

### 5. TUI integration
- `App.cancellation: Option<CancellationToken>` — хранит токен текущего запуска.
- `spawn_agent` создаёт токен, сохраняет в `App`, передаёт в `AgentBuilder`.
- Ctrl+C в Thinking mode: `token.cancel()` + немедленный возврат в Normal mode.
- `AgentEvent::Cancelled`: очищает токен, финализирует состояние.

### DoD-тесты
- `cancellation_emits_cancelled_event` — HangingLlm + CancellationToken →
  Started → Cancelled.
- `confirm_timeout_treats_as_denied` — HangingConfirmer + confirm_timeout(100ms) →
  CommandFinished { denied: true, "timed out" } → агент продолжает → Finished.

**Изменённые файлы:**
- `crates/agent/Cargo.toml` — `tokio-util` dependency
- `crates/agent/src/events.rs` — `Cancelled` variant
- `crates/agent/src/agent.rs` — поля, builder methods, `with_cancellation`,
  `run()`/`run_loop()` cancellation, `process_tool_call()` timeouts, 2 DoD-теста
- `crates/tui/Cargo.toml` — `tokio-util` dependency
- `crates/tui/src/app.rs` — `cancellation` field, Ctrl+C handler, `Cancelled` handler
- `crates/tui/src/runner.rs` — `spawn_agent` принимает `CancellationToken`

**Тесты:** 252 passed, 0 failed, 5 ignored.

**Публичные контракты:**
- `AgentEvent` — новый вариант `Cancelled` (non-breaking, `#[non_exhaustive]`).
- `AgentBuilder` — новые методы: `cancellation()`, `confirm_timeout()`, `command_timeout()`.
