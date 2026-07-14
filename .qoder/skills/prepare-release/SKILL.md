---
name: prepare-release
description: >
  Подготовить и выпустить релиз репозитория filar одним вызовом, без пауз:
  проверить готовность, поднять версию (Cargo.toml/CHANGELOG/ENGINE_API) и
  запушить bump-коммит прямо в main, создать теги и опубликовать релиз через API,
  а если релиз затрагивал движок — поставить engine-тег. Запускать вручную через
  /prepare-release, передавая ДВА аргумента: версию и тип ОС.
  Пример: /prepare-release 0.1.1 windows
---

# prepare-release

Подготовить и выпустить новый релиз filar по SemVer и нашему флоу (тег = триггер,
GitHub Actions собирает бинарник, заметки генерируются автоматически из PR). Скилл
выполняет весь ритуал **без пауз** — от бампа версии до публикации релиза и
engine-тега. Останавливается только при провале preflight.

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

3. **Git-тег обязан существовать ДО создания релиза.** Если опубликовать релиз через
   API без предварительно запушенного тега `vX.Y.Z`, GitHub создаст мусорный
   `untagged-...` релиз. Обход: сначала `git tag vX.Y.Z` + `git push origin vX.Y.Z`,
   затем POST релиза с `draft: false` и `tag_name: "vX.Y.Z"`.

### Рецепт: GitHub API без gh CLI

```powershell
# 1. Получить токен из git credential store
$credOutput = "protocol=https`nhost=github.com`n`n" | git credential fill 2>&1
$token = ($credOutput | Select-String "password=(.+)" ).Matches.Groups[1].Value

# 2. Записать тело release notes в файл через Write tool (UTF-8)
#    (Write tool пишет файлы в UTF-8, PowerShell 5.x — нет)

# 3. Сначала создать и запушить тег (ОБЯЗАТЕЛЬНО до POST релиза)
git tag vX.Y.Z
git push origin vX.Y.Z

# 4. Прочитать файл как UTF-8 и опубликовать релиз сразу (draft: false)
$bodyText = [System.IO.File]::ReadAllText("c:\dev\warper\release_body.txt", [System.Text.Encoding]::UTF8)
$bodyObj = @{ body = $bodyText; draft = $false; tag_name = "vX.Y.Z"; name = "Filar vX.Y.Z"; target_commitish = "main"; generate_release_notes = $true } | ConvertTo-Json -Depth 5
$bodyBytes = [System.Text.Encoding]::UTF8.GetBytes($bodyObj)
$response = Invoke-RestMethod -Uri "https://api.github.com/repos/devlawey/filar/releases" `
  -Method Post `
  -Headers @{ "Authorization" = "Bearer $token"; "Accept" = "application/vnd.github+json"; "User-Agent" = "filar" } `
  -Body $bodyBytes `
  -ContentType "application/json; charset=utf-8"

# 5. Проверить, что workflow запустился
$runs = Invoke-RestMethod -Uri "https://api.github.com/repos/devlawey/filar/actions/runs?per_page=3" `
  -Headers @{ "Authorization" = "Bearer $token"; "Accept" = "application/vnd.github+json"; "User-Agent" = "filar" }
```

### Рецепт: обновление тела релиза (если кириллица сломалась)

Если тело релиза уже создано с `????????` вместо кириллицы:
1. Запиши правильный текст в файл через Write tool.
2. Прочитай через `[System.IO.File]::ReadAllText(path, [System.Text.Encoding]::UTF8)`.
3. Отправь PATCH с `@{ body = $text } | ConvertTo-Json` через `[System.Text.Encoding]::UTF8.GetBytes()`.

## Процедура

Соблюдай правила из AGENTS.md в корне репозитория. Этот скилл — единственное место
с явным исключением: релизный version-bump пушится прямо в `main`, а релиз
публикуется без запроса подтверждения. Всё остальное (любые не-релизные правки,
force-push, пересоздание тегов) — по обычным запретам AGENTS.md.

1. **Разбери аргументы.** Из $ARGUMENTS возьми версию и тип ОС.
   - Нормализуй версию: тег — `vX.Y.Z` (с префиксом `v`), значение в Cargo.toml —
     `X.Y.Z` (без `v`). Если формат не SemVer (`MAJOR.MINOR.PATCH`) — остановись и
     попроси корректную версию.
   - Проверь, что тип ОС — один из допустимых. Иначе остановись и спроси.

2. **Preflight-проверки. Составь отчёт пользователю и не двигайся дальше при провале:**
   - `main` актуален; `cargo build --workspace` и `cargo test --workspace` — зелёные
     (юнит-тесты; `#[ignore]` не запускай).
   - **ПРЕДОХРАНИТЕЛЬ ПОРЯДКА (обязательно).** Порядок релиза ВСЕГДА: бамп версии
     → push bump-коммита в `main` → теги. Никогда наоборот. Прочитай
     `workspace.package.version` в корневом `Cargo.toml`. Если он **не равен**
     запрошенной версии `X.Y.Z` — значит бамп ещё не сделан: НЕ создавай ни
     `vX.Y.Z`, ни `engine-vX.Y.Z`, ни релиз. Сначала выполни шаг 3 (bump-коммит);
     только с `main`, где версия уже `X.Y.Z`, переходи к тегам. Тег, ставший на
     коммит с несовпадающей версией, — ошибка релиза.
   - В репозитории есть `.github/workflows/release.yml`.
   - **Workflow поддерживает запрошенную ОС.** Проверь, какие ОС собирает release.yml.
     Если запрошенная ОС (или любая из них при `all`) НЕ покрыта workflow (например
     просят `macos`, а workflow только Windows) — ОСТАНОВИСЬ и сообщи пользователю, что
     сначала нужно расширить workflow под эту ОС. Не создавай релиз, который соберёт не
     все обещанные бинарники.
   - Версия ещё не выпущена: тега `vX.Y.Z` не существует и одноимённого релиза нет.
   - (Если задаче соответствует milestone) все его issue закрыты — иначе предупреди.

3. **Подними версию и запушь bump-коммит в `main`.** В этот же коммит входит вся
   релизная «бухгалтерия», чтобы теги ставились на уже готовый коммит:
   - На актуальном `main` (после `git pull`).
   - В корневом `Cargo.toml`, секция `[workspace.package]`, поменяй `version` на `X.Y.Z`.
   - **CHANGELOG.md:** переименуй `## [Unreleased]` → `## [X.Y.Z] - <YYYY-MM-DD>`
     (сегодняшняя дата), заведи сверху новую пустую `## [Unreleased]` (чтобы
     будущие PR было куда дописывать), и обнови сравнительные ссылки внизу
     (`[Unreleased]: …/compare/vX.Y.Z...HEAD`, `[X.Y.Z]: …/compare/<prev>...vX.Y.Z`).
   - **Если релиз затрагивает движок** (были PR с label `transport`/`agent`/`core`
     или менялись `crates/core|transport|agent`): в `docs/ENGINE_API.md` обнови
     примеры зависимостей на `tag = "engine-vX.Y.Z"`.
   - Прогони `cargo build --workspace` — обновится `Cargo.lock` (внутренние крейты
     на новую версию); включи его в коммит.
   - Обнови `PROGRESS.md`: отметь подготовку релиза vX.Y.Z и что в него входит.
   - Один коммит: `chore(release): bump version to X.Y.Z`.
   - **Без паузы (исключение для релизного бампа):** закоммить и запушь этот
     коммит НАПРЯМУЮ в `main` (`git push origin HEAD:main`) — без PR и без ожидания
     мержа. Это единственное разрешённое исключение из PR-правила (см. AGENTS.md,
     раздел «Git и ветки»): только для чистого version-bump-коммита релиза и только
     когда preflight (сборка/тесты) зелёный. Force-push в `main` остаётся под
     запретом. (Если прямой push отклонён branch-protection — тогда открой PR,
     смержи его сам через API и продолжай без остановки.)

4. **Сразу публикуй релиз (без пауз и без запроса подтверждения):**
   - Убедись, что ты на `main` с только что запушенным bump-коммитом
     (`git checkout main` — ты уже там; `HEAD` = version-bump).
   - Создай тег на этом коммите и запушь: `git tag vX.Y.Z` + `git push origin vX.Y.Z`.
   - Опубликуй релиз через GitHub API сразу как `draft: false` (см. рецепт выше):
     - `tag_name: "vX.Y.Z"`, `name: "Filar vX.Y.Z"`, `target_commitish: "main"`,
       `draft: false`, `generate_release_notes: true`.
     - **Конвенция именования:** `name` = `Filar vX.Y.Z` (например `Filar v0.3.0`),
       не просто `vX.Y.Z` — так показывается на главной странице репозитория.
     - Заметки (body) запиши в файл через Write tool (UTF-8!), прочитай через
       `[System.IO.File]::ReadAllText(..., [System.Text.Encoding]::UTF8)` и отправь
       как `[System.Text.Encoding]::UTF8.GetBytes(json)` — иначе кириллица сломается.
     - В начало заметок добавь сводку: что нового, результаты preflight.
     - Тег обязан существовать ДО создания релиза (иначе релиз станет
       `untagged-...`) — поэтому пушим тег на шаге выше, а не после.
   - **Проверь, что не создался дубликат-релиз** (`untagged-...`). Если на странице
     релизов появился мусорный `untagged-...` релиз — удали его через API (DELETE
     `/releases/{id}`), а его assets (бинарник) перенеси на правильный релиз.
     Также удали мусорный тег `untagged-...` (локально и на remote).
   - **Проверь имя бинарника.** Workflow называет файл `filar-{tag_name}-windows-x86_64.exe`.
     Если tag_name был `untagged-...`, переименуй asset через API
     (PATCH `/releases/assets/{id}` с `name: "filar-vX.Y.Z-windows-x86_64.exe"`).
     Также удали старые бинарники от предыдущих релизов, если они случайно прикрепились.
   - Проверь, что workflow `release.yml` запустился (через API `actions/runs`).
   - Дай пользователю ссылки: на релиз и на workflow run.
   - Подсказать, что статус сборки виден во вкладке Actions.

5. **Тег движка `engine-vX.Y.Z` (если релиз затрагивал движок).**
   Проверь, были ли в этом milestone/релизе PR с label `transport`, `agent` или
   `core` (или изменялись `crates/core|transport|agent`, `Cargo.toml`). Если да —
   после публикации релиза поставь тег движка на ТОТ ЖЕ коммit, что и `vX.Y.Z`:
   `git tag engine-vX.Y.Z` и `git push origin engine-vX.Y.Z`. Это точка
   зависимости для внешних потребителей (бот/мобилка), задокументированная в
   `docs/ENGINE_API.md` — обнови там пример `tag = "engine-vX.Y.Z"`, если ещё не
   обновлён в bump-коммите. Тег движка ставится ТОЛЬКО с `main`, где версия уже `X.Y.Z`.

## Ограничения
- **Теги неизменяемы.** Тег, который мог быть опубликован (запушен в origin) и на
  который могут ссылаться внешние потребители, НИКОГДА не пересоздаётся через
  `git tag -f` / `git push -f`. Cargo кэширует git-зависимости по коммиту тега —
  молча переехавший тег ломает воспроизводимость сборок. Если тег встал не на тот
  коммит: честно удали его (`git tag -d <tag>` + `git push origin :refs/tags/<tag>`),
  при необходимости удали ставший draft GitHub Release, и создай тег заново на
  правильном коммите. Force для тегов — под запретом.
- **Без пауз.** Этот скилл выполняется от начала до конца без остановок на
  подтверждение: bump-коммит пушится прямо в `main`, релиз публикуется сразу
  (`draft: false`). Единственное, что останавливает процесс — провал preflight
  или непокрытая workflow-ом ОС.
- **Исключение для прямого push.** Прямой push в `main` разрешён ТОЛЬКО для
  чистого version-bump-коммита релиза (`chore(release): bump version to X.Y.Z`).
  Любые другие изменения по-прежнему идут через PR. Force-push в `main` запрещён
  всегда.
- Не создавай релиз, если preflight провалился или workflow не покрывает запрошенную ОС.
- Тег (`vX.Y.Z`) и версия в Cargo.toml (`X.Y.Z`) должны совпадать.
- Временные файлы (`release_body.txt` и т.п.) удаляй после использования.
- НЕ выводи GitHub-токен в лог/вывод — он читается из `git credential fill` молча.

## Справка по нумерации (SemVer)
- Только баг-фиксы → +PATCH (`0.1.0` → `0.1.1`).
- Новая обратно-совместимая функциональность → +MINOR (`0.1.1` → `0.2.0`).
- Ломающие изменения → +MAJOR (`0.x` → `1.0.0` — когда поведение стабильно).
