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
│   │   └── src/{lib,app,ui,event,confirmer,runner,terminal}.rs
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

- **51 unit-тест** проходят:
  - filar-agent: 33 теста (agent loop, tools, security, GLM client)
  - filar-transport: 2 теста (marker format, payload format) + 3 ignored (Docker)
  - filar-tui: 16 тестов (terminal model, key mapping, app state)
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
