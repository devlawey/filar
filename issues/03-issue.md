<!-- labels: feat,tui,mouse | milestone: TUI Modernization v0.2.0 | size: S -->

> **Обязательный контекст:** перед работой прочитать `docs/DESIGN_PHILOSOPHY.md`
> в корне репозитория — все решения принимаются в его рамках.
> Это задача 3 из 11 плана `docs/TUI_MODERNIZATION_PLAN.md`.
> **Зависит от:** задача 2 «Кэширование layout чата (фундамент для мыши и скролла)» — не начинать, пока их PR не смержены в main.
>
---

# Инфраструктура мыши: захват, событие, скролл колесом

**Контекст.** Мышь не захватывается вовсе. Начинаем с самого ценного — скролла.

**Шаги:**

1. В `runner.rs::run()`: при инициализации добавить `EnableMouseCapture`,
   при завершении — `DisableMouseCapture` (в обоих путях выхода, включая ошибочный):
   ```rust
   crossterm::execute!(stdout, EnterAlternateScreen, EnableMouseCapture)...
   // teardown:
   crossterm::execute!(io::stdout(), DisableMouseCapture, LeaveAlternateScreen).ok();
   ```
2. В event loop `runner.rs` обработать `Event::Mouse(m)`:
   ```rust
   Some(Ok(Event::Mouse(m))) => { app.handle_mouse(m); needs_redraw = true; }
   ```
3. В `app.rs` добавить `pub fn handle_mouse(&mut self, m: MouseEvent)`.
   Первая функциональность — колесо в области чата (все режимы, кроме Interactive):
   - `MouseEventKind::ScrollUp` внутри `self.chat_area` → `scroll += 3`;
   - `ScrollDown` → `scroll = scroll.saturating_sub(3)`;
   - клампить `scroll` максимумом (`layout_cache.lines.len().saturating_sub(visible_height)`),
     чтобы нельзя было укрутить в пустоту. Тот же кламп применить к PgUp.
4. Индикатор «вы не внизу»: если `scroll > 0`, в правом нижнем углу области чата
   рисовать ненавязчивую метку `↓ N new` тусклым цветом (`theme.fg_muted`), где N —
   количество строк ниже вьюпорта. Клик по ней (обработаем в задаче 4) и `End` —
   сброс в низ. Добавить обработку `KeyCode::End` при пустом вводе → `scroll = 0`.

**Проверка на Windows Terminal и conhost обязательна** (проект Windows-first):
скролл колесом должен работать в обоих.

**DoD:**
- Колесо мыши прокручивает чат вверх/вниз в Normal/Thinking/Confirming.
- Скролл клампится, метка `↓ N new` появляется/исчезает корректно.
- PgUp/PgDn работают как раньше.
- Захват мыши корректно снимается при выходе (после закрытия приложения
  выделение текста в терминале ОС работает).

---

---

## Критерии готовности (общие, в дополнение к DoD выше)

- [ ] `cargo build --workspace` зелёный
- [ ] `cargo clippy --all-targets -- -D warnings` зелёный
- [ ] `cargo test --workspace` зелёный
- [ ] `PROGRESS.md` обновлён (что сделано, принятые решения, изменения контрактов)
- [ ] Поведение вне скоупа задачи не изменилось (хоткеи, `!`, Ctrl+T/P/C, секреты, сессии)
