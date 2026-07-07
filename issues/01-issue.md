<!-- labels: refactor,tui | milestone: TUI Modernization v0.2.0 | size: S -->

> **Обязательный контекст:** перед работой прочитать `docs/DESIGN_PHILOSOPHY.md`
> в корне репозитория — все решения принимаются в его рамках.
> Это задача 1 из 11 плана `docs/TUI_MODERNIZATION_PLAN.md`.
> **Зависимостей нет** — можно начинать сразу.
>
---

# Модуль темы и рефакторинг рендера

**Контекст.** Сейчас `ui.rs` (~440 строк) — один файл с жёстко зашитыми цветами.
Прежде чем менять внешний вид и добавлять мышь, нужно навести порядок.

**Шаги:**

1. Создать `crates/tui/src/theme.rs` со структурой:
   ```rust
   pub struct Theme {
       pub bg: Color,            // фон (обычно Color::Reset — фон терминала)
       pub fg: Color,            // основной текст
       pub fg_dim: Color,        // вторичный текст (серый)
       pub fg_muted: Color,      // самый тусклый (подсказки, плейсхолдеры)
       pub accent: Color,        // главный акцент (пользователь, фокус) — Cyan
       pub success: Color,       // agent, approved — Green
       pub warning: Color,       // thinking, внимание — Yellow
       pub danger: Color,        // ошибки, деструктивные команды — Red
       pub surface: Color,       // фон "приподнятых" элементов (статус-бар, кнопки)
       pub selection_bg: Color,  // фон выделения текста мышью
   }
   impl Theme { pub fn default_dark() -> Self { ... } }
   ```
   Добавить методы-хелперы: `theme.user_style()`, `theme.agent_style()`,
   `theme.error_style()`, `theme.dim()`, `theme.muted()` и т.п.
2. Разбить `ui.rs` на модуль `crates/tui/src/ui/`:
   - `ui/mod.rs` — `pub fn render(...)` + layout;
   - `ui/theme.rs` — тема (перенести из п.1);
   - `ui/chat.rs` — рендер истории чата;
   - `ui/input.rs` — поле ввода / confirm / password;
   - `ui/bars.rs` — статус-бар и help-бар;
   - `ui/text.rs` — `strip_emoji`, `wrap_text` (перенести без изменений).
3. Заменить все прямые `Color::*` на обращения к `Theme`. Экземпляр темы хранить
   в `App` (поле `pub theme: Theme`).
4. Поведение и внешний вид на этом шаге **не меняются** (кроме того, что цвета
   теперь идут из одной точки).

**DoD:**
- `cargo build` и `cargo clippy -- -D warnings` без ошибок.
- В `crates/tui/src/ui/` нет ни одного литерала `Color::` вне `theme.rs`.
- Внешний вид приложения идентичен прежнему (ручная проверка).
- Существующие тесты проходят.

---

---

## Критерии готовности (общие, в дополнение к DoD выше)

- [ ] `cargo build --workspace` зелёный
- [ ] `cargo clippy --all-targets -- -D warnings` зелёный
- [ ] `cargo test --workspace` зелёный
- [ ] `PROGRESS.md` обновлён (что сделано, принятые решения, изменения контрактов)
- [ ] Поведение вне скоупа задачи не изменилось (хоткеи, `!`, Ctrl+T/P/C, секреты, сессии)
