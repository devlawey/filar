<!-- labels: feat,tui,mouse,terminal | milestone: TUI Modernization v0.2.0 | size: M -->

> **Обязательный контекст:** перед работой прочитать `docs/DESIGN_PHILOSOPHY.md`
> в корне репозитория — все решения принимаются в его рамках.
> Это задача 10 из 11 плана `docs/TUI_MODERNIZATION_PLAN.md`.
> **Зависит от:** задача 3 «Инфраструктура мыши: захват, событие, скролл колесом» — не начинать, пока их PR не смержены в main.
>
---

# Мышь в интерактивном режиме терминала

**Контекст.** В режиме Interactive (Ctrl+T) свой эмулятор на `alacritty_terminal`.
Нужно: скролл истории терминала колесом и проброс мыши в приложения на удалёнке
(vim, htop, mc), если они её запросили.

**Шаги:**

1. В `TerminalModel` (terminal.rs): API для scrollback —
   `pub fn scroll_display(&mut self, delta: i32)` через
   `term.scroll_display(Scroll::Delta(delta))`, и `pub fn mouse_mode(&self) -> bool`
   по `term.mode()` (`TermMode::MOUSE_MODE`-флаги: REPORT_CLICK/DRAG/MOTION + SGR).
2. `handle_mouse` в режиме Interactive:
   - если приложение в терминале запросило мышь (`mouse_mode() == true`) —
     кодировать событие в SGR-последовательность (`\x1b[<b;x;yM/m`, координаты
     1-based относительно области терминала) и слать в `pending_term_input`
     (в терминах существующего механизма проброса байтов);
   - иначе: колесо → `scroll_display(±3)` (просмотр истории), любой ввод с
     клавиатуры → сброс скролла в низ (`Scroll::Bottom`).
3. Учесть alternate screen: в alt-screen колесо без mouse-mode транслировать
   в стрелки ↑↑↑/↓↓↓ (по 3 на тик) — стандартное поведение эмуляторов, чтобы
   колесо «просто работало» в less/man.
4. В help-баре режима Interactive добавить `wheel scroll`.

**DoD:**
- Колесо скроллит историю локального PowerShell в интерактивном режиме.
- В `htop`/`mc` по SSH (тест через docker/sshd из репо) клики и колесо
  доходят до приложения.
- В `less` колесо листает (трансляция в стрелки).

---

---

## Критерии готовности (общие, в дополнение к DoD выше)

- [ ] `cargo build --workspace` зелёный
- [ ] `cargo clippy --all-targets -- -D warnings` зелёный
- [ ] `cargo test --workspace` зелёный
- [ ] `PROGRESS.md` обновлён (что сделано, принятые решения, изменения контрактов)
- [ ] Поведение вне скоупа задачи не изменилось (хоткеи, `!`, Ctrl+T/P/C, секреты, сессии)
