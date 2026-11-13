// promptfoo grading provider: Jev (TypeSafe decision model) as the judge of
// `llm-rubric` asserts, over the OpenRouter Decisions API (#665).
//
// Why not a chat model: a decision model returns a probability, not text, so
// it has no reasoning to eat the grader's token limit — the failure mode of
// `mistralai/mistral-large` since 2026-11-12 (`No output`, `Could not extract
// JSON`). Jev is billed by input tokens only.
//
// promptfoo calls `callApi(prompt, context)` with the rendered grading prompt
// and `context.vars` = { ...test vars, output, rubric }. We ask Jev one `noul`
// (yes/no) question — does the answer satisfy the rubric? — and hand promptfoo
// back the JSON verdict it expects: {"pass", "score", "reason"}.
//
// The key comes from OPENROUTER_API_KEY and never appears in a verdict, an
// error or a log line. A failed call is an `error` (the rubric fails), never
// a silent PASS.

'use strict';

const DEFAULT_ENDPOINT = 'https://openrouter.ai/api/alpha/decisions';
const DEFAULT_MODEL = 'typesafe/jev-1.14';
const DEFAULT_TIMEOUT_MS = 60000;
const PASS_THRESHOLD = 0.5;
/** Cap on an error body echoed into a verdict. */
const ERROR_BODY_LIMIT = 300;

const INSTRUCTIONS =
  "Does the assistant's answer satisfy this grading rubric? Judge only the " +
  'answer shown in the state; the rubric says when it passes and when it ' +
  'fails.';

const CRITERIA = {
  true: "The answer meets the rubric's PASS condition.",
  false: "The answer meets the rubric's FAIL condition, or does not meet its PASS condition.",
};

/** The agent's answer as text: prose as-is, tool calls as JSON. */
function answerText(output) {
  if (output == null) return '(no answer)';
  if (typeof output === 'string') return output;
  return JSON.stringify(output, null, 1);
}

/**
 * `<Tag>…</Tag>` content of the default llm-rubric prompt — a JSON array of
 * chat messages whose last one holds `<Output>` and `<Rubric>` — for a call
 * that came without grading vars.
 */
function tagged(prompt, tag) {
  if (typeof prompt !== 'string') return null;
  let text = prompt;
  try {
    const messages = JSON.parse(prompt);
    if (Array.isArray(messages)) text = messages.map((m) => (m && m.content) || '').join('\n');
  } catch {
    // A plain-text prompt: search it as is.
  }
  // The system message carries examples with the same tags: the graded
  // answer is the last one.
  const all = [...text.matchAll(new RegExp(`<${tag}>\\n?([\\s\\S]*?)\\n?</${tag}>`, 'g'))];
  return all.length ? all[all.length - 1][1] : null;
}

/**
 * The Decisions API request for one rubric. `vars` are the grading vars
 * promptfoo passes (`output`, `rubric`, and the test's `question`).
 */
function buildRequest(model, vars, prompt) {
  const rubric = (vars && vars.rubric) || tagged(prompt, 'Rubric');
  if (!rubric) return { error: 'jev grader: no rubric in the grading call' };
  const output = vars && 'output' in vars ? vars.output : tagged(prompt, 'Output');
  const question = vars && vars.question;
  const state =
    (question ? `A user sent this request to a terminal assistant for server administrators:\n${question}\n\n` : '') +
    `The assistant answered (prose and/or the tool calls it issued):\n${answerText(output)}`;
  return {
    request: {
      model,
      state,
      questions: {
        verdict: {
          type: 'noul',
          instructions: `${INSTRUCTIONS}\n\nRubric:\n${String(rubric).trim()}`,
          criteria: CRITERIA,
        },
      },
    },
  };
}

/** `text` cut to the error limit, with any occurrence of the key masked. */
function excerpt(text, key) {
  const cut = String(text).slice(0, ERROR_BODY_LIMIT);
  return key ? cut.split(key).join('***') : cut;
}

/** Verdict JSON for promptfoo from Jev's `noul` probability. */
function verdict(p, model) {
  const pass = p >= PASS_THRESHOLD;
  return JSON.stringify({
    pass,
    score: p,
    reason: `jev ${model}: p(pass)=${p.toFixed(2)} (threshold ${PASS_THRESHOLD})`,
  });
}

class JevGrader {
  /**
   * `options.config`: `model`, `endpoint`, `timeoutMs` (all optional).
   * `options.fetch` and `options.env` exist for tests only.
   */
  constructor(options = {}) {
    const config = options.config || {};
    this.env = options.env || process.env;
    this.model = config.model || this.env.JEV_GRADER_MODEL || DEFAULT_MODEL;
    this.endpoint = config.endpoint || DEFAULT_ENDPOINT;
    this.timeoutMs = config.timeoutMs || DEFAULT_TIMEOUT_MS;
    this.fetch = options.fetch || globalThis.fetch;
    this.label = options.label || `jev-grader:${this.model}`;
  }

  id() {
    return this.label;
  }

  async callApi(prompt, context) {
    const key = this.env.OPENROUTER_API_KEY || '';
    if (!key) return { error: 'jev grader: OPENROUTER_API_KEY is not set' };
    const built = buildRequest(this.model, context && context.vars, prompt);
    if (built.error) return { error: built.error };

    let resp;
    try {
      resp = await this.fetch(this.endpoint, {
        method: 'POST',
        headers: { Authorization: `Bearer ${key}`, 'Content-Type': 'application/json' },
        body: JSON.stringify(built.request),
        signal: AbortSignal.timeout(this.timeoutMs),
      });
    } catch (e) {
      const why = e && e.name === 'TimeoutError' ? 'timeout' : (e && e.message) || String(e);
      return { error: `jev grader: request failed: ${excerpt(why, key)}` };
    }
    const text = await resp.text().catch(() => '');
    if (!resp.ok) {
      return { error: `jev grader: HTTP ${resp.status}: ${excerpt(text, key)}` };
    }
    let body;
    try {
      body = JSON.parse(text);
    } catch {
      return { error: `jev grader: response is not JSON: ${excerpt(text, key)}` };
    }
    const answer = body && body.answers && body.answers.verdict;
    const p = answer && Number(answer.noul);
    if (!Number.isFinite(p) || p < 0 || p > 1) {
      return { error: `jev grader: no noul answer in response: ${excerpt(text, key)}` };
    }
    const usage = body.usage || {};
    const result = { output: verdict(p, body.model || this.model) };
    if (typeof usage.cost === 'number') result.cost = usage.cost;
    if (typeof usage.prompt_tokens === 'number') {
      result.tokenUsage = { prompt: usage.prompt_tokens, completion: 0, total: usage.prompt_tokens };
    }
    return result;
  }
}

module.exports = JevGrader;
module.exports.buildRequest = buildRequest;
module.exports.verdict = verdict;
module.exports.PASS_THRESHOLD = PASS_THRESHOLD;
