<!-- labels: feat,agent,tui,streaming | milestone: TUI Modernization v0.2.0 | size: L -->

> **Обязательный контекст:** перед работой прочитать `docs/DESIGN_PHILOSOPHY.md`
> в корне репозитория — все решения принимаются в его рамках.
> Это задача 7 из 11 плана `docs/TUI_MODERNIZATION_PLAN.md`.
> **Зависит от:** задача 2 «Кэширование layout чата (фундамент для мыши и скролла)» — не начинать, пока их PR не смержены в main.
>
---

# Стриминг ответа LLM + спиннер

**Контекст.** Это самое заметное «осовременивание»: сейчас пользователь смотрит
на статичное «Agent is thinking...» до полного ответа. GLM API поддерживает
SSE-стриминг (`"stream": true`), reqwest уже собран с feature `stream`.

**Шаги:**

1. `crates/agent/src/glm.rs`: добавить метод стриминга. В трейт `LlmClient`
   добавить метод с дефолтной реализацией-фоллбэком (чтобы не ломать другие
   реализации/моки):
   ```rust
   async fn chat_stream(&self, req: ..., on_delta: &(dyn Fn(&str) + Send + Sync))
       -> Result<LlmResponse> { self.chat(req).await } // дефолт: без стрима
   ```
   В GLM-клиенте: `"stream": true`, читать `bytes_stream()`, парсить SSE-строки
   `data: {...}` построчно (буферизуя разрывы чанков), извлекать
   `choices[0].delta.content` → `on_delta(text)`; накапливать полный текст и
   tool_calls (дельты tool_calls аккумулировать по `index`). Строка `data: [DONE]`
   завершает поток. Вернуть собранный `LlmResponse`, идентичный нестримовому.
2. `crates/tui/src/event.rs`: новое событие `AgentEvent::TextDelta(String)`.
3. `crates/agent/src/agent.rs` / `runner.rs::spawn_agent`: пробросить колбэк,
   который шлёт `TextDelta` в канал. (Если проще архитектурно — передать
   `UnboundedSender<AgentEvent>` в builder агента как опциональный
   `on_text_delta`.)
4. `app.rs::handle_agent_event`:
   - `TextDelta(s)`: если последний блок — «стримящийся» `Agent`, дописать `s`
     к нему; иначе создать новый `ChatBlock::Agent(s)` и флаг
     `pub streaming: bool` в `App`. Инкремент `message_rev` (кэш перестроит
     только хвост — допустимо перестроить целиком, кэш быстрый);
     при `scroll == 0` держать прижатым к низу; при `scroll > 0` — не дёргать
     (пользователь читает историю), просто растёт счётчик `↓ N new`.
   - `Finished`: заменить/финализировать стримящийся блок финальным текстом
     (LLM мог вернуть текст+tool_call — финальный текст источник истины),
     `streaming = false`.
5. Спиннер: в `App` поле `pub tick: u64`, инкрементировать в
   `render_interval.tick()` (тикать даже без `needs_redraw`, когда
   `mode == Thinking` — иначе спиннер замрёт; т.е. условие тика:
   `needs_redraw || app.mode == AppMode::Thinking`). В статус-баре и в панели
   ввода в режиме Thinking: кадры `⠋⠙⠹⠸⠼⠴⠦⠧⠇⠏` (braille); ASCII-fallback
   `|/-\` если braille вне whitelist (добавить 0x2800–0x28FF в whitelist —
   современные Windows Terminal рендерят braille корректно).
   Текст: `⠹ thinking…` → после первой дельты `⠹ writing…`.
6. Панель ввода в Thinking больше не жёлтая коробка «Agent is thinking», а
   обычное поле ввода в disabled-виде (текст `fg_muted`), со спиннером слева —
   layout стабилен.

**DoD:**
- Ответ агента появляется по мере генерации, чат автоскроллится, если был внизу.
- Спиннер анимируется плавно, CPU в простое не растёт (тик только в Thinking).
- Tool calls работают как прежде (стрим корректно собирает tool_calls).
- При ошибке сети посреди стрима — `AgentEvent::Error`, частичный текст остаётся
  в чате с пометкой `System("response interrupted")`.
- Unit-тест SSE-парсера: скормить байтовые чанки с разрывом посреди `data:`-строки,
  проверить корректную сборку дельт и tool_calls.

---

---

## Критерии готовности (общие, в дополнение к DoD выше)

- [ ] `cargo build --workspace` зелёный
- [ ] `cargo clippy --all-targets -- -D warnings` зелёный
- [ ] `cargo test --workspace` зелёный
- [ ] `PROGRESS.md` обновлён (что сделано, принятые решения, изменения контрактов)
- [ ] Поведение вне скоупа задачи не изменилось (хоткеи, `!`, Ctrl+T/P/C, секреты, сессии)
