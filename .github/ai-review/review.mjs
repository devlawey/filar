#!/usr/bin/env node
/**
 * AI PR reviewer via OpenRouter.
 *
 * Читает диф pull request'а, отправляет его выбранной модели через
 * OpenRouter (OpenAI-совместимый /chat/completions) и публикует ревью
 * с инлайн-комментариями на конкретные строки.
 *
 * Зависимостей нет — только встроенный fetch (Node 20+).
 */

import { readFileSync, existsSync, appendFileSync } from 'node:fs';

const env = process.env;

const GITHUB_TOKEN = req('GITHUB_TOKEN');
const OPENROUTER_API_KEY = req('OPENROUTER_API_KEY');
const REPO = req('GITHUB_REPOSITORY'); // owner/repo
const PR_NUMBER = Number(req('PR_NUMBER'));
const MODEL = env.MODEL || 'openai/gpt-5.5';

const MAX_DIFF_CHARS = int(env.MAX_DIFF_CHARS, 120_000);
const MAX_FILE_CHARS = int(env.MAX_FILE_CHARS, 25_000);
const MAX_COMMENTS = int(env.MAX_COMMENTS, 25);
const INCREMENTAL = (env.INCREMENTAL ?? 'true') !== 'false';
const TEMPERATURE = Number(env.TEMPERATURE ?? '0.1');
const DRY_RUN = env.DRY_RUN === 'true';

const [OWNER, NAME] = REPO.split('/');
const API = 'https://api.github.com';
const MARKER = '<!-- ai-review:v1 -->';

/** Файлы, которые ревьюить бессмысленно (шум и трата токенов). */
const IGNORE = [
  /(^|\/)Cargo\.lock$/,
  /(^|\/)package-lock\.json$/,
  /(^|\/)pnpm-lock\.yaml$/,
  /(^|\/)yarn\.lock$/,
  /\.(png|jpe?g|gif|ico|svg|webp|pdf|zip|exe|dll|so|dylib|bin|woff2?|ttf)$/i,
  /(^|\/)(target|node_modules|dist|build)\//,
  /\.min\.(js|css)$/,
];

// ---------------------------------------------------------------------------
// helpers
// ---------------------------------------------------------------------------

function req(name) {
  const v = env[name];
  if (!v) {
    console.error(`FATAL: переменная окружения ${name} не задана`);
    process.exit(1);
  }
  return v;
}

function int(v, dflt) {
  const n = parseInt(v ?? '', 10);
  return Number.isFinite(n) ? n : dflt;
}

async function gh(path, opts = {}) {
  const res = await fetch(path.startsWith('http') ? path : `${API}${path}`, {
    ...opts,
    headers: {
      accept: 'application/vnd.github+json',
      authorization: `Bearer ${GITHUB_TOKEN}`,
      'x-github-api-version': '2022-11-28',
      'content-type': 'application/json',
      'user-agent': 'ai-review',
      ...(opts.headers || {}),
    },
  });
  if (!res.ok) {
    const text = await res.text();
    const err = new Error(`GitHub ${res.status} ${path}: ${text.slice(0, 500)}`);
    err.status = res.status;
    throw err;
  }
  return res.status === 204 ? null : res.json();
}

async function ghPaged(path) {
  const out = [];
  for (let page = 1; page <= 20; page++) {
    const sep = path.includes('?') ? '&' : '?';
    const batch = await gh(`${path}${sep}per_page=100&page=${page}`);
    if (!Array.isArray(batch) || batch.length === 0) break;
    out.push(...batch);
    if (batch.length < 100) break;
  }
  return out;
}

function ignored(path) {
  return IGNORE.some((re) => re.test(path));
}

function summaryOut(md) {
  if (env.GITHUB_STEP_SUMMARY) {
    try {
      appendFileSync(env.GITHUB_STEP_SUMMARY, md + '\n');
    } catch {
      /* не критично */
    }
  }
}

// ---------------------------------------------------------------------------
// разбор патча
// ---------------------------------------------------------------------------

/**
 * Разбирает unified diff одного файла.
 * Возвращает строки с номерами в НОВОМ файле — для якорей комментариев.
 */
function parsePatch(patch) {
  const rows = [];
  let newLine = 0;
  for (const raw of patch.split('\n')) {
    const hunk = /^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(raw);
    if (hunk) {
      newLine = parseInt(hunk[1], 10);
      rows.push({ type: 'hunk', text: raw });
      continue;
    }
    if (raw.startsWith('\\')) continue; // "\ No newline at end of file"
    if (raw.startsWith('+')) {
      rows.push({ type: 'add', newLine, text: raw.slice(1) });
      newLine++;
    } else if (raw.startsWith('-')) {
      rows.push({ type: 'del', text: raw.slice(1) });
    } else {
      rows.push({ type: 'ctx', newLine, text: raw.slice(1) });
      newLine++;
    }
  }
  return rows;
}

/** Множество строк нового файла, на которые GitHub примет комментарий. */
function anchorableLines(patch) {
  const set = new Set();
  for (const r of parsePatch(patch)) {
    if (r.type === 'add' || r.type === 'ctx') set.add(r.newLine);
  }
  return set;
}

/** Рендер патча с явными номерами строк — чтобы модель точно ссылалась. */
function renderPatch(path, patch) {
  const lines = [`### FILE: ${path}`];
  for (const r of parsePatch(patch)) {
    if (r.type === 'hunk') lines.push(r.text);
    else if (r.type === 'add') lines.push(`${String(r.newLine).padStart(5)} + ${r.text}`);
    else if (r.type === 'ctx') lines.push(`${String(r.newLine).padStart(5)}   ${r.text}`);
    else lines.push(`      - ${r.text}`);
  }
  return lines.join('\n');
}

// ---------------------------------------------------------------------------
// сбор контекста
// ---------------------------------------------------------------------------

async function lastReviewedSha() {
  const reviews = await ghPaged(`/repos/${OWNER}/${NAME}/pulls/${PR_NUMBER}/reviews`);
  for (let i = reviews.length - 1; i >= 0; i--) {
    const body = reviews[i].body || '';
    if (body.includes(MARKER)) {
      const m = /<!--\s*ai-review:sha=([0-9a-f]{7,40})\s*-->/.exec(body);
      if (m) return m[1];
    }
  }
  return null;
}

function projectRules() {
  const parts = [];
  for (const f of ['AGENTS.md', 'CONTRIBUTING.md']) {
    if (existsSync(f)) {
      const text = readFileSync(f, 'utf8').slice(0, 12_000);
      parts.push(`--- ${f} ---\n${text}`);
    }
  }
  return parts.join('\n\n');
}

// ---------------------------------------------------------------------------
// OpenRouter
// ---------------------------------------------------------------------------

async function askModel(system, user) {
  const body = {
    model: MODEL,
    temperature: TEMPERATURE,
    messages: [
      { role: 'system', content: system },
      { role: 'user', content: user },
    ],
  };

  let lastErr;
  for (let attempt = 1; attempt <= 3; attempt++) {
    try {
      const res = await fetch('https://openrouter.ai/api/v1/chat/completions', {
        method: 'POST',
        headers: {
          authorization: `Bearer ${OPENROUTER_API_KEY}`,
          'content-type': 'application/json',
          'HTTP-Referer': `https://github.com/${REPO}`,
          'X-Title': 'ai-review',
        },
        body: JSON.stringify(body),
      });

      if (res.status === 429 || res.status >= 500) {
        lastErr = new Error(`OpenRouter ${res.status}: ${(await res.text()).slice(0, 300)}`);
        await new Promise((r) => setTimeout(r, attempt * 4000));
        continue;
      }
      if (!res.ok) {
        throw new Error(`OpenRouter ${res.status}: ${(await res.text()).slice(0, 500)}`);
      }

      const json = await res.json();
      const msg = json.choices?.[0]?.message;
      const content = msg?.content ?? '';
      if (!content.trim()) throw new Error('OpenRouter вернул пустой ответ');
      return { content, usage: json.usage || {} };
    } catch (e) {
      lastErr = e;
      if (attempt === 3) break;
      await new Promise((r) => setTimeout(r, attempt * 4000));
    }
  }
  throw lastErr;
}

/** Достаёт JSON из ответа модели (терпимо к ```json-обёрткам и болтовне вокруг). */
function extractJson(text) {
  const fenced = /```(?:json)?\s*([\s\S]*?)```/.exec(text);
  const candidates = [fenced?.[1], text];
  for (const c of candidates) {
    if (!c) continue;
    try {
      return JSON.parse(c.trim());
    } catch {
      /* пробуем дальше */
    }
    const s = c.indexOf('{');
    const e = c.lastIndexOf('}');
    if (s !== -1 && e > s) {
      try {
        return JSON.parse(c.slice(s, e + 1));
      } catch {
        /* пробуем дальше */
      }
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// main
// ---------------------------------------------------------------------------

async function main() {
  const pr = await gh(`/repos/${OWNER}/${NAME}/pulls/${PR_NUMBER}`);
  const headSha = pr.head.sha;
  console.log(`PR #${PR_NUMBER} "${pr.title}" head=${headSha.slice(0, 8)} model=${MODEL}`);

  // Полный список файлов PR — из него строим карту допустимых якорей.
  const prFiles = await ghPaged(`/repos/${OWNER}/${NAME}/pulls/${PR_NUMBER}/files`);
  const anchors = new Map();
  for (const f of prFiles) {
    if (f.patch) anchors.set(f.filename, anchorableLines(f.patch));
  }

  // Содержимое для ревью: инкрементально (только новое с прошлого ревью) либо весь PR.
  let files = prFiles;
  let incrementalFrom = null;
  if (INCREMENTAL) {
    const since = await lastReviewedSha();
    if (since && since !== headSha) {
      try {
        const cmp = await gh(`/repos/${OWNER}/${NAME}/compare/${since}...${headSha}`);
        if (Array.isArray(cmp.files) && cmp.files.length) {
          files = cmp.files;
          incrementalFrom = since;
          console.log(`инкрементально: ${since.slice(0, 8)}..${headSha.slice(0, 8)}`);
        }
      } catch (e) {
        console.log(`инкрементальный режим недоступен (${e.message}), ревьюю весь PR`);
      }
    }
  }

  // Фильтрация и сборка дифа.
  const skipped = [];
  const chunks = [];
  let total = 0;
  for (const f of files) {
    if (!f.patch) {
      if (f.status !== 'removed') skipped.push(`${f.filename} (бинарный/без патча)`);
      continue;
    }
    if (f.status === 'removed') continue;
    if (ignored(f.filename)) {
      skipped.push(`${f.filename} (в игнор-листе)`);
      continue;
    }
    let patch = f.patch;
    if (patch.length > MAX_FILE_CHARS) {
      patch = patch.slice(0, MAX_FILE_CHARS) + '\n... [патч обрезан]';
      skipped.push(`${f.filename} (обрезан до ${MAX_FILE_CHARS} символов)`);
    }
    const rendered = renderPatch(f.filename, patch);
    if (total + rendered.length > MAX_DIFF_CHARS) {
      skipped.push(`${f.filename} (не влез в лимит дифа)`);
      continue;
    }
    total += rendered.length;
    chunks.push(rendered);
  }

  if (chunks.length === 0) {
    console.log('нечего ревьюить');
    summaryOut('AI-ревью: изменений для ревью нет.');
    return;
  }

  const rules = projectRules();
  const systemPrompt = existsSync('.github/ai-review/prompt.md')
    ? readFileSync('.github/ai-review/prompt.md', 'utf8')
    : 'You are a senior code reviewer. Reply with JSON only.';

  const userPrompt = [
    `## Pull request`,
    `Title: ${pr.title}`,
    pr.body ? `Description:\n${String(pr.body).slice(0, 4000)}` : '',
    incrementalFrom
      ? `\nЭто ИНКРЕМЕНТАЛЬНОЕ ревью: ниже только изменения, добавленные после предыдущего ревью (${incrementalFrom.slice(0, 8)}).`
      : '',
    rules ? `\n## Правила проекта (обязательны к проверке)\n${rules}` : '',
    `\n## Diff`,
    `Формат: номер строки в НОВОМ файле, затем "+" (добавлено), " " (контекст) или "-" (удалено).`,
    `Комментарии ставь ТОЛЬКО на строки, у которых есть номер.`,
    '',
    chunks.join('\n\n'),
  ]
    .filter(Boolean)
    .join('\n');

  console.log(`диф: ${chunks.length} файлов, ~${total} символов`);

  const { content, usage } = await askModel(systemPrompt, userPrompt);
  const parsed = extractJson(content);

  // Если модель не смогла в JSON — публикуем её текст как обычное summary.
  if (!parsed || typeof parsed !== 'object') {
    console.log('не удалось разобрать JSON, публикую как текст');
    await postReview(headSha, `${MARKER}\n<!-- ai-review:sha=${headSha} -->\n\n${content}`, []);
    return;
  }

  const rawComments = Array.isArray(parsed.comments) ? parsed.comments : [];
  const comments = [];
  const unanchored = [];

  for (const c of rawComments.slice(0, MAX_COMMENTS * 2)) {
    const path = String(c.path || '').trim();
    const line = Number(c.line);
    const sev = String(c.severity || 'minor').toLowerCase();
    const bodyText = String(c.body || '').trim();
    if (!path || !bodyText) continue;

    const badge = { blocker: '🔴 blocker', major: '🟠 major', minor: '🟡 minor', nit: '🔵 nit' }[sev] || '🟡 minor';
    const body = `**${badge}**\n\n${bodyText}`;

    if (anchors.has(path) && Number.isFinite(line) && anchors.get(path).has(line)) {
      comments.push({ path, line, side: 'RIGHT', body });
    } else {
      unanchored.push(`- \`${path}\`${Number.isFinite(line) ? `:${line}` : ''} — ${badge}: ${bodyText}`);
    }
    if (comments.length >= MAX_COMMENTS) break;
  }

  const parts = [
    MARKER,
    `<!-- ai-review:sha=${headSha} -->`,
    `## 🤖 AI-ревью`,
    '',
    String(parsed.summary || '').trim() || '_Модель не вернула summary._',
  ];

  if (unanchored.length) {
    parts.push('', '<details><summary>Замечания без привязки к строке</summary>', '', ...unanchored, '', '</details>');
  }
  if (skipped.length) {
    parts.push('', `<details><summary>Пропущено файлов: ${skipped.length}</summary>`, '', ...skipped.map((s) => `- ${s}`), '', '</details>');
  }

  const cost = usage.total_tokens
    ? `${usage.prompt_tokens ?? '?'} in / ${usage.completion_tokens ?? '?'} out токенов`
    : 'токены не отчитаны';
  parts.push(
    '',
    '---',
    `<sub>Модель: \`${MODEL}\` · ${cost}${incrementalFrom ? ` · инкрементально с \`${incrementalFrom.slice(0, 8)}\`` : ''} · ` +
      `перезапуск: комментарий \`/ai-review\`</sub>`
  );

  const reviewBody = parts.join('\n');

  if (DRY_RUN) {
    console.log('--- DRY RUN ---');
    console.log(reviewBody);
    console.log(JSON.stringify(comments, null, 2));
    return;
  }

  await postReview(headSha, reviewBody, comments);
  summaryOut(`AI-ревью опубликовано: ${comments.length} инлайн-комментариев, модель \`${MODEL}\`, ${cost}.`);
}

/** Публикует ревью; при отказе по инлайнам — падает обратно на обычный комментарий. */
async function postReview(commitId, body, comments) {
  try {
    await gh(`/repos/${OWNER}/${NAME}/pulls/${PR_NUMBER}/reviews`, {
      method: 'POST',
      body: JSON.stringify({ commit_id: commitId, body, event: 'COMMENT', comments }),
    });
    console.log(`ревью опубликовано (${comments.length} инлайн-комментариев)`);
  } catch (e) {
    console.log(`не удалось опубликовать ревью с инлайнами: ${e.message}`);
    const appendix = comments.length
      ? '\n\n<details><summary>Инлайн-замечания (не удалось привязать)</summary>\n\n' +
        comments.map((c) => `- \`${c.path}:${c.line}\` — ${c.body.replace(/\n+/g, ' ')}`).join('\n') +
        '\n\n</details>'
      : '';
    await gh(`/repos/${OWNER}/${NAME}/issues/${PR_NUMBER}/comments`, {
      method: 'POST',
      body: JSON.stringify({ body: body + appendix }),
    });
    console.log('опубликовано как обычный комментарий');
  }
}

main().catch((e) => {
  console.error(e);
  process.exit(1);
});
