# Фаза 0. Filar Engine API — подготовка движка к внешним фронтендам

> **Репозиторий:** `devlawey/filar` (существующий).
> Это обязательный пререквизит для `filar-bot` и `filar-mobile`: оба потребляют
> крейты `filar-core`, `filar-transport`, `filar-agent` как git-зависимости.
> Формат: задача = issue = ветка = PR (флоу `work-one-issue`). После каждой
> задачи: `cargo build --workspace`, `cargo clippy --all-targets -- -D warnings`,
> `cargo test --workspace` — зелёные.

## Цель

Движок (core + transport + agent) становится самодостаточной библиотекой с
чистым публичным API, компилируемой под **Linux-сервер** (бот) и
**Android/iOS** (мобилка), без предположений о фронтенде (TUI/бот/мобилка) и
без привязки к Windows/env. TUI переводится на этот же API — один код на всех.

Инварианты (из AGENTS.md, действуют и здесь): zero-install на удалёнке;
ни одна команда не выполняется без подтверждения; секреты никогда не попадают
в LLM и в логи; изоляция за трейтами.

---

## Задача 0.1. Публичные события агента: `AgentEvent` + sink в движке

**Контекст.** Сейчас события агента (thinking/command/finished/error) определены
в `crates/tui/src/event.rs` — фронтенд-крейте. Бот и мобилка не могут их
использовать. Событийная модель — сердце любого фронтенда.

**Шаги:**

1. В `crates/agent/src/` создать `events.rs` с UI-агностичным enum:
   ```rust
   #[derive(Debug, Clone)]
   pub enum AgentEvent {
       /// Агент начал обработку запроса.
       Started,
       /// Дельта текста ответа LLM (стриминг).
       TextDelta(String),
       /// Агент запросил выполнение команды (до подтверждения).
       CommandProposed { command: String, explanation: String, destructive: bool },
       /// Команда выполнена (или отклонена).
       CommandFinished { command: String, output: String, denied: bool },
       /// Финальный текст ответа.
       Finished(String),
       /// Ошибка (сеть, LLM, транспорт).
       Error(String),
   }
   pub type EventSink = std::sync::Arc<dyn Fn(AgentEvent) + Send + Sync>;
   ```
2. В `AgentBuilder` добавить `.event_sink(sink: EventSink)` (опционально; по
   умолчанию — no-op). Внутри `Agent::run` эмитить события во всех ключевых
   точках цикла (начало, перед/после каждого tool call, финал, ошибки).
3. `crates/tui`: удалить дубли из `event.rs`, использовать `filar_agent::AgentEvent`;
   мост в `runner.rs::spawn_agent` теперь просто перекладывает события из sink в
   существующий канал TUI. Поведение TUI не меняется.
4. Задокументировать enum как **публичный контракт**: rustdoc на каждый вариант,
   пометка `#[non_exhaustive]` (фронтенды обязаны иметь ветку `_ =>`).


**Дополнение (ревью 0.2.0):** `ChatResponse` переделать в структуру с полями
`text: String` и `tool_calls: Vec<ToolCall>` (сейчас enum теряет стримленую
текст-преамбулу при tool calls — история LLM расходится с экраном). Обновить
GlmClient (оба пути), цикл агента и tui.

**DoD:**
- `AgentEvent` живёт в `filar-agent`, TUI использует его без собственной копии.
- Unit-тест: мок-LLM с одним tool call → sink получает последовательность
  Started → CommandProposed → CommandFinished → Finished.
- Поведение TUI не изменилось (ручной smoke-тест).

---

## Задача 0.2. Стриминг в `LlmClient` (общая точка для всех фронтендов)

**Контекст.** Совпадает с задачей 7 плана модернизации TUI. Если она уже
выполнена — сверить с этим описанием и закрыть как done. Если нет — выполнять
здесь, а TUI-задача 7 сведётся к отображению.

**Шаги:**

1. В трейт `LlmClient` добавить `chat_stream(&self, req, on_delta) -> Result<ChatResponse>`
   с дефолтной реализацией-фоллбэком через `chat()`.
2. В `GlmClient` реализовать SSE-стриминг (`"stream": true`, парсинг `data:`-строк
   с буферизацией разрывов чанков, аккумуляция tool_calls по index, `[DONE]`).
3. `Agent::run` использует `chat_stream`, пробрасывая дельты в
   `AgentEvent::TextDelta` через sink из задачи 0.1.

**DoD:**
- Unit-тест SSE-парсера (разрыв посреди `data:`, tool_calls по кускам).
- Мок-LLM со стримом → sink получает TextDelta до Finished.
- Нестримящие реализации `LlmClient` продолжают работать (фоллбэк).

---

## Задача 0.3. Отмена и таймауты: `CancellationToken` в `Agent::run`

**Контекст.** У бота и мобилки обязательна кнопка «Отмена» (в TUI это Ctrl+C
с выходом — недопустимо для долгоживущего бота). Долгий SSH-вызов не должен
вешать сессию навсегда.

**Шаги:**

1. `AgentBuilder::cancellation(token: tokio_util::sync::CancellationToken)`
   (tokio-util уже в workspace). В цикле агента: `tokio::select!` между текущим
   шагом (LLM-запрос / выполнение команды) и `token.cancelled()`.
2. При отмене: если команда уже подтверждена и выполняется — дождаться/убить по
   политике транспорта (для SSH — закрыть канал), эмитить
   `AgentEvent::Error("cancelled")`... нет: добавить в enum вариант `Cancelled`
   (enum `#[non_exhaustive]` — можно). История чата остаётся консистентной
   (частичный ответ сохраняется).
3. Таймаут ожидания подтверждения: `AgentBuilder::confirm_timeout(Duration)` —
   если `CommandConfirmer::confirm` не ответил за N секунд → трактовать как
   deny с пометкой в выводе. Дефолт: без таймаута (поведение TUI не меняется).
4. Таймаут выполнения команды: `AgentBuilder::command_timeout(Duration)`,
   дефолт — текущее поведение.

**DoD:**
- Unit-тест: отмена во время «долгого» мок-tool-call → `Cancelled`, метод
  возвращается быстро (< 1 c).
- Unit-тест: confirm_timeout истёк → команда denied, агент продолжает цикл.
- TUI собирается и работает без изменений (токен не задан — вечное ожидание).

---

## Задача 0.4. `SecretProvider`: секреты не только из env

**Контекст.** `filar-core::secrets` читает только `std::env`. На мобилке секреты
живут в Keystore/Keychain и передаются программно; у бота — в конфиге/keyring
хоста. Env остаётся дефолтом (TUI/десктоп не меняются).

**Шаги:**

1. В `filar-core` добавить трейт:
   ```rust
   pub trait SecretProvider: Send + Sync {
       /// Получить секрет по логическому имени ("GLM_API_KEY", "FILAR_SECRET_1", …).
       fn get(&self, name: &str) -> Result<String>;
   }
   pub struct EnvSecretProvider;           // текущее поведение
   pub struct StaticSecretProvider(HashMap<String, String>); // для FFI/бота/тестов
   ```
2. Все места, где движок читает секреты из env (API-ключ LLM в agent/glm,
   подстановка `$FILAR_SECRET_N` перед выполнением команды, санитизация вывода),
   перевести на `Arc<dyn SecretProvider>`, прокинутый через `AgentBuilder` и
   конструкторы транспорта. Дефолт — `EnvSecretProvider`.
3. Гарантия неутечки сохраняется и усиливается: тест — секрет из
   `StaticSecretProvider` подставлен в команду, но в `CommandFinished.output`
   и в сообщениях к LLM он замаскирован (существующая санитизация обязана
   работать с любым провайдером).
4. `StaticSecretProvider` реализует `Drop` c зануление... (не усложнять:
   достаточно `zeroize` для значений, крейт добавить с feature-флагом `zeroize`,
   по умолчанию включён).


**Дополнение (ревью 0.2.0):** перенести подстановку `$FILAR_SECRET_N` и
санитизацию вывода из `crates/tui/src/runner.rs::TuiExecutor` в движок
(обёртка `SecretSubstitutingExecutor` через `SecretProvider`); `TuiExecutor`
остаётся тонким эмиттером событий. Тесты переезжают в движок.

**DoD:**
- Ни одного прямого `std::env::var` для секретов вне `EnvSecretProvider`.
- Тесты подстановки/санитизации проходят с обоими провайдерами.
- TUI/GUI работают без изменений конфигурации.

---

## Задача 0.5. Кросс-компиляция: feature `local`, целевые платформы, CI

**Контекст.** `portable-pty` (локальный PowerShell-режим) не нужен и не
компилируется под Android/iOS. Бот и мобилка используют только SSH-режим.

**Шаги:**

1. В `filar-transport`: локальный исполнитель (`local.rs`, `portable-pty`) —
   за feature `local` (в default-features). SSH-часть (`ssh.rs`, russh) —
   безусловная. `filar-tui`/`filar-app` включают `local` явно.
2. Проверить сборку движка (core+transport без `local`+agent) под таргеты:
   - `x86_64-unknown-linux-gnu` (бот);
   - `aarch64-linux-android` (через `cargo ndk`);
   - `aarch64-apple-ios` (`cargo build --target`, только check если нет macOS
     в CI — тогда хотя бы `cargo check` через zig/осознанно отложить с пометкой).
   Устранить всплывшие платформенные зависимости (например, пути сессий в
   `session.rs` c `cfg!(windows)` — сделать базовую директорию параметром:
   `SessionStore::new(base_dir: PathBuf)`, дефолт-фабрика — текущая логика).
3. GitHub Actions: workflow `engine-targets.yml` — матрица
   `cargo check -p filar-core -p filar-transport --no-default-features -p filar-agent`
   под linux-gnu и aarch64-linux-android (NDK ставится в job).
4. `docs/ENGINE_API.md`: краткий гайд потребителя — какие крейты брать, пример
   `Cargo.toml` с git-tag зависимостью, минимальный пример «собрать агента и
   получить события» (30 строк), таблица фич.
5. Тег релиза движка: `engine-v0.3.0` (отдельная линейка тегов от релизов
   приложения). В README — строка про использование как библиотеки.

**DoD:**
- `cargo check` движка проходит под linux и android-таргет в CI.
- Пример из `docs/ENGINE_API.md` компилируется (doc-test или examples/).
- Тег `engine-v0.3.0` создан; `filar-bot`/`filar-mobile` могут на него
  сослаться.

---

## Порядок

```
0.1 (события) → 0.2 (стриминг) → 0.3 (отмена/таймауты)
0.4 (секреты) — независимо, можно параллельно после 0.1
0.5 (кросс-компиляция, доки, тег) — финал, после всех
```

## Anti-scope Фазы 0

- ❌ Не переносить движок в отдельный репозиторий (git-tag зависимостей достаточно).
- ❌ Не публиковать на crates.io.
- ❌ Не делать multi-tenancy внутри движка (несколько пользователей — забота
  фронтенда: один `Agent` на сессию; движок обязан лишь быть `Send + Sync`
  и не иметь глобального состояния — проверить и зафиксировать тестом).
- ❌ Никаких изменений UX TUI в рамках этой фазы.
