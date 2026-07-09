---
name: prepare-release
description: >
  Подготовить релиз репозитория filar: проверить готовность, поднять версию в
  Cargo.toml через PR, убедиться что release-workflow поддерживает нужную ОС, и
  создать черновик релиза с авто-заметками. После подтверждения пользователя —
  опубликовать релиз и создать тег через API. Запускать вручную через
  /prepare-release, передавая ДВА аргумента: версию и тип ОС.
  Пример: /prepare-release 0.1.1 windows
---

# prepare-release

Подготовить новый релиз filar по SemVer и нашему флоу (тег = триггер, GitHub Actions
собирает бинарник, заметки генерируются автоматически из PR). Скилл делает всю
автоматизируемую подготовку и публикацию; финальное подтверждение публикации даёт
пользователь.

**Аргументы (через $ARGUMENTS, два значения через пробел):**
1. `версия` — например `0.1.1` или `v0.1.1`.
2. `тип-ОС` — одно из: `windows`, `macos`, `linux`, `all`.

## Технические особенности окружения

Окружение пользователя — Windows + PowerShell 5.x (НЕ PowerShell Core). Это создаёт
три проблемы, которые скилл обязан обходить:

1. **Нет `gh` CLI.** Ни `gh`, ни `winget`, ни `scoop`, ни `choco` не установлены.
   Для работы с GitHub API используй `git credential fill` для получения токена и
   `Invoke-RestMethod` для запросов (см. рецепт ниже).

2. **PowerShell 5.x ломает кириллицу в JSON.** Если передать русский текст напрямую
   в `ConvertTo-Json` и отправить через `Invoke-RestMethod`, кириллица превратится
   в знаки вопроса (`????????`). Обход: запиши тело в файл через Write tool (он
   пишет UTF-8), затем прочитай через `[System.IO.File]::ReadAllText(path,
   [System.Text.Encoding]::UTF8)`, сконвертируй в JSON, и отправь байты через
   `[System.Text.Encoding]::UTF8.GetBytes(json)` (см. рецепт ниже).

3. **GitHub API не создаёт git-тег при публикации draft-релиза через PATCH.**
   Если создать draft-релиз с `tag_name: "vX.Y.Z"`, а затем опубликовать его через
   PATCH `draft: false`, тег НЕ создастся — релиз получит имя `untagged-...`.
   Обход: создай тег локально (`git tag vX.Y.Z`) и пушь его (`git push origin
   vX.Y.Z`) ДО публикации релиза, затем припиши тег к релизу через PATCH
   `tag_name: "vX.Y.Z"`.

### Рецепт: GitHub API без gh CLI

```powershell
# 1. Получить токен из git credential store
$credOutput = "protocol=https`nhost=github.com`n`n" | git credential fill 2>&1
$token = ($credOutput | Select-String "password=(.+)" ).Matches.Groups[1].Value

# 2. Записать тело запроса в файл через Write tool (UTF-8)
#    (Write tool пишет файлы в UTF-8, PowerShell 5.x — нет)

# 3. Прочитать файл как UTF-8 и отправить запрос
$bodyText = [System.IO.File]::ReadAllText("c:\dev\warper\release_body.txt", [System.Text.Encoding]::UTF8)
$bodyObj = @{ body = $bodyText; draft = $true; tag_name = "vX.Y.Z"; target_commitish = "main"; generate_release_notes = $true } | ConvertTo-Json -Depth 5
$bodyBytes = [System.Text.Encoding]::UTF8.GetBytes($bodyObj)
$response = Invoke-RestMethod -Uri "https://api.github.com/repos/devlawey/filar/releases" `
  -Method Post `
  -Headers @{ "Authorization" = "Bearer $token"; "Accept" = "application/vnd.github+json"; "User-Agent" = "filar" } `
  -Body $bodyBytes `
  -ContentType "application/json; charset=utf-8"
# Сохранить release ID: $response.id
```

### Рецепт: публикация релиза (после подтверждения пользователя)

```powershell
# 1. Создать тег локально и запушить
git tag vX.Y.Z
git push origin vX.Y.Z

# 2. Опубликовать релиз через API
$credOutput = "protocol=https`nhost=github.com`n`n" | git credential fill 2>&1
$token = ($credOutput | Select-String "password=(.+)" ).Matches.Groups[1].Value
$bodyObj = @{ draft = $false; tag_name = "vX.Y.Z" } | ConvertTo-Json
$bodyBytes = [System.Text.Encoding]::UTF8.GetBytes($bodyObj)
$response = Invoke-RestMethod -Uri "https://api.github.com/repos/devlawey/filar/releases/$RELEASE_ID" `
  -Method Patch `
  -Headers @{ "Authorization" = "Bearer $token"; "Accept" = "application/vnd.github+json"; "User-Agent" = "filar" } `
  -Body $bodyBytes `
  -ContentType "application/json; charset=utf-8"

# 3. Проверить, что workflow запустился
$runs = Invoke-RestMethod -Uri "https://api.github.com/repos/devlawey/filar/actions/runs?per_page=3" `
  -Headers @{ "Authorization" = "Bearer $token"; "Accept" = "application/vnd.github+json"; "User-Agent" = "filar" }
```

### Рецепт: обновление тела релиза (если кириллица сломалась)

Если тело релиза уже создано с `????????` вместо кириллицы:
1. Запиши правильный текст в файл через Write tool.
2. Прочитай через `[System.IO.File]::ReadAllText(path, [System.Text.Encoding]::UTF8)`.
3. Отправь PATCH с `@{ body = $text } | ConvertTo-Json` через `[System.Text.Encoding]::UTF8.GetBytes()`.

## Процедура

Соблюдай правила из AGENTS.md в корне репозитория. Ничего не публикуй и не создавай
теги без явного подтверждения пользователя.

1. **Разбери аргументы.** Из $ARGUMENTS возьми версию и тип ОС.
   - Нормализуй версию: тег — `vX.Y.Z` (с префиксом `v`), значение в Cargo.toml —
     `X.Y.Z` (без `v`). Если формат не SemVer (`MAJOR.MINOR.PATCH`) — остановись и
     попроси корректную версию.
   - Проверь, что тип ОС — один из допустимых. Иначе остановись и спроси.

2. **Preflight-проверки. Составь отчёт пользователю и не двигайся дальше при провале:**
   - `main` актуален; `cargo build --workspace` и `cargo test --workspace` — зелёные
     (юнит-тесты; `#[ignore]` не запускай).
   - В репозитории есть `.github/workflows/release.yml`.
   - **Workflow поддерживает запрошенную ОС.** Проверь, какие ОС собирает release.yml.
     Если запрошенная ОС (или любая из них при `all`) НЕ покрыта workflow (например
     просят `macos`, а workflow только Windows) — ОСТАНОВИСЬ и сообщи пользователю, что
     сначала нужно расширить workflow под эту ОС. Не создавай релиз, который соберёт не
     все обещанные бинарники.
   - Версия ещё не выпущена: тега `vX.Y.Z` не существует и одноимённого релиза нет.
   - (Если задаче соответствует milestone) все его issue закрыты — иначе предупреди.

3. **Подними версию — через PR, не прямым пушем в `main`.**
   - Ветка `chore/release-X.Y.Z` от актуального `main`.
   - В корневом `Cargo.toml`, секция `[workspace.package]`, поменяй `version` на `X.Y.Z`.
   - Обнови `PROGRESS.md`: отметь подготовку релиза vX.Y.Z и что в него входит.
   - Один коммит: `chore(release): bump version to X.Y.Z`.
   - Открой PR в `main` с кратким описанием. ОСТАНОВИСЬ и дождись, пока пользователь
     смержит этот PR — тег должен создаваться из `main` уже с поднятой версией.

4. **После мержа bump-PR — создай ЧЕРНОВИК релиза** (draft, не публикуй):
   - Переключись на `main`, подтяни изменения (`git checkout main; git pull origin main`).
   - Удали локальную ветку `chore/release-X.Y.Z`.
   - Создай draft-релиз через GitHub API (см. рецепт выше):
     - `tag_name: "vX.Y.Z"`, `target_commitish: "main"`, `draft: true`,
       `generate_release_notes: true`.
     - Заметки (body) запиши в файл через Write tool (UTF-8!), прочитай через
       `[System.IO.File]::ReadAllText(..., [System.Text.Encoding]::UTF8)` и отправь
       как `[System.Text.Encoding]::UTF8.GetBytes(json)` — иначе кириллица сломается.
     - В начало заметок добавь сводку: что нового, результаты preflight.
   - Сохрани `release_id` из ответа API — он нужен для публикации.
   - НЕ создавай тег и НЕ публикуй — только draft.

5. **Передай пользователю.** Дай короткую сводку: какая версия, какая ОС, что проверено,
   ссылку на черновик релиза. Спроси подтверждение на публикацию.

6. **После подтверждения пользователя — опубликуй релиз:**
   - Создай тег локально: `git tag vX.Y.Z` (на текущем HEAD `main`).
   - Пушь тег: `git push origin vX.Y.Z`.
   - Опубликуй релиз через API (PATCH `draft: false` + `tag_name: "vX.Y.Z"`).
   - Проверь, что workflow `release.yml` запустился (через API `actions/runs`).
   - Дай пользователю ссылки: на релиз и на workflow run.
   - Подсказать, что статус сборки виден во вкладке Actions.

## Ограничения
- Не публикуй релиз и не пушь теги без явного подтверждения пользователя — максимум
  черновик.
- Бамп версии идёт только через PR; прямой push в `main` запрещён (AGENTS.md).
- Не создавай релиз, если preflight провалился или workflow не покрывает запрошенную ОС.
- Тег (`vX.Y.Z`) и версия в Cargo.toml (`X.Y.Z`) должны совпадать.
- Временные файлы (`release_body.txt` и т.п.) удаляй после использования.
- НЕ выводи GitHub-токен в лог/вывод — он читается из `git credential fill` молча.

## Справка по нумерации (SemVer)
- Только баг-фиксы → +PATCH (`0.1.0` → `0.1.1`).
- Новая обратно-совместимая функциональность → +MINOR (`0.1.1` → `0.2.0`).
- Ломающие изменения → +MAJOR (`0.x` → `1.0.0` — когда поведение стабильно).
