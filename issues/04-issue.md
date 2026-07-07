<!-- labels: feat,tui,mouse | milestone: TUI Modernization v0.2.0 | size: M -->

> **Обязательный контекст:** перед работой прочитать `docs/DESIGN_PHILOSOPHY.md`
> в корне репозитория — все решения принимаются в его рамках.
> Это задача 4 из 11 плана `docs/TUI_MODERNIZATION_PLAN.md`.
> **Зависит от:** задача 3 «Инфраструктура мыши: захват, событие, скролл колесом» — не начинать, пока их PR не смержены в main.
>
---

# Скроллбар и hit-testing кликов

**Шаги:**

1. Отрисовать вертикальный скроллбар по правому краю области чата через
   `ratatui::widgets::{Scrollbar, ScrollbarState, ScrollbarOrientation::VerticalRight}`.
   Стиль: тонкий, thumb — `theme.fg_dim`, track — невидимый или `theme.fg_muted`.
   Показывать только когда контент не влезает.
2. Drag по скроллбару: в `handle_mouse` обработать `MouseEventKind::Down(Left)` и
   `Drag(Left)` в колонке скроллбара → пересчитать `scroll` пропорционально `row`.
   В `App` завести `pub mouse_drag: Option<DragKind>` (`enum DragKind { Scrollbar, Selection }`),
   сбрасывать на `Up`.
3. Общий hit-testing: приватный метод
   `fn hit_test(&self, col: u16, row: u16) -> HitZone`, где
   ```rust
   enum HitZone { Chat { line_idx: usize }, ChatEmpty, Scrollbar, Input, HelpBar, StatusBar, ConfirmButton(bool), Outside }
   ```
   `line_idx` вычисляется из `row`, `chat_area` и текущего `scroll` через
   `layout_cache` (то, что подготовили в задаче 2).
4. Клик в область ввода → перевод фокуса (пока фокус один, но: клик по колонке
   внутри введённого текста ставит `cursor_pos` в соответствующую позицию —
   учесть перенос строки так же, как в существующей математике курсора).
5. Клик по метке `↓ N new` → `scroll = 0`.

**DoD:**
- Скроллбар отображается при переполнении, перетаскивается мышью.
- Клик в поле ввода ставит курсор под клик (включая вторую строку wrap).
- Unit-тест на `hit_test`: подготовить фиктивные `Rect` и проверить попадания
  по зонам и вычисление `line_idx`.

---

---

## Критерии готовности (общие, в дополнение к DoD выше)

- [ ] `cargo build --workspace` зелёный
- [ ] `cargo clippy --all-targets -- -D warnings` зелёный
- [ ] `cargo test --workspace` зелёный
- [ ] `PROGRESS.md` обновлён (что сделано, принятые решения, изменения контрактов)
- [ ] Поведение вне скоупа задачи не изменилось (хоткеи, `!`, Ctrl+T/P/C, секреты, сессии)
