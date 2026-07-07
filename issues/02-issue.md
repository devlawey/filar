<!-- labels: refactor,tui,perf | milestone: TUI Modernization v0.2.0 | size: M -->

> **Обязательный контекст:** перед работой прочитать `docs/DESIGN_PHILOSOPHY.md`
> в корне репозитория — все решения принимаются в его рамках.
> Это задача 2 из 11 плана `docs/TUI_MODERNIZATION_PLAN.md`.
> **Зависит от:** задача 1 «Модуль темы и рефакторинг рендера» — не начинать, пока их PR не смержены в main.
>
---

# Кэширование layout чата (фундамент для мыши и скролла)

**Контекст.** Сейчас `render_chat_history` каждый кадр (до 60 fps) заново
разворачивает все `ChatBlock` в строки и переносит их по ширине. Для мыши нужен
hit-testing: «в какой блок попал клик по координате (x, y)?» — значит, нужна
кэшированная карта строк.

**Шаги:**

1. Создать `crates/tui/src/ui/layout_cache.rs`:
   ```rust
   /// Одна отрендеренная строка чата с привязкой к исходному блоку.
   pub struct RenderedLine {
       pub line: ratatui::text::Line<'static>,
       pub block_index: Option<usize>, // индекс в app.messages; None = пустая строка-разделитель
       pub region: LineRegion,         // Header | Body | OutputToggle | ...
   }
   pub enum LineRegion { Header, Body, Output, OutputToggle, Spacer }

   pub struct ChatLayoutCache {
       pub lines: Vec<RenderedLine>,
       width: u16,           // ширина, для которой построен кэш
       message_count: usize, // messages.len() на момент построения
       last_block_rev: u64,  // ревизия последнего блока (для стриминга, задача 7)
   }
   ```
2. Метод `ChatLayoutCache::rebuild(&mut self, messages: &[ChatBlock], width: u16, theme: &Theme, collapsed: &HashSet<usize>)`
   переносит логику построения строк из `chat.rs`. Инвалидация: изменилась ширина,
   изменилось число сообщений или ревизия последнего блока.
3. В `App` добавить: `pub layout_cache: ChatLayoutCache` и
   `pub message_rev: u64` (инкрементировать при любой мутации `messages`; ввести
   приватный метод `push_message()` вместо прямых `self.messages.push(...)` —
   заменить все вхождения).
4. `render_chat_history` теперь: проверить/перестроить кэш → взять срез видимых
   строк по `scroll` → отрисовать. Лимит `MAX_LINES = 500` заменить на лимит
   в 2000 кэшированных строк (кэш делает это дёшево).
5. В `App` добавить `pub chat_area: Rect` — фактическая область чата на экране,
   заполняется при каждом рендере (нужно для hit-testing в задаче 3). Аналогично
   `pub input_area: Rect`, `pub confirm_button_areas: Vec<(Rect, bool)>`
   (заполним позже).

**DoD:**
- Внешний вид не изменился.
- При длинной истории (200+ сообщений) прокрутка не тормозит (перестройка кэша
  не происходит на каждом кадре — проверить логом/счётчиком в debug).
- Unit-тест: кэш инвалидируется при изменении ширины и добавлении сообщения,
  и НЕ перестраивается при повторном вызове с теми же параметрами.

---

---

## Критерии готовности (общие, в дополнение к DoD выше)

- [ ] `cargo build --workspace` зелёный
- [ ] `cargo clippy --all-targets -- -D warnings` зелёный
- [ ] `cargo test --workspace` зелёный
- [ ] `PROGRESS.md` обновлён (что сделано, принятые решения, изменения контрактов)
- [ ] Поведение вне скоупа задачи не изменилось (хоткеи, `!`, Ctrl+T/P/C, секреты, сессии)
