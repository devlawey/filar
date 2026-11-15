// eval/scripts/smoke-check.js
//
// Reads a promptfoo results.json, prints the pass rate, and exits 0 if it meets
// the threshold (default 90), else 1. Used by the eval-smoke CI workflow to gate
// the regression smoke run.
//
// For every failed result it also prints why (#676): the test description, each
// assert that did not pass (type, score, reason) and a short excerpt of the
// model's answer (tool calls and the start of its text). The CI log is then
// enough to see how a case failed without downloading the report artifact.
// Only test data, the model's answer and assert reasons are printed — never the
// environment or provider request headers.
//
// Usage: node eval/scripts/smoke-check.js <results.json> [threshold]
const fs = require('fs');
const path = require('path');
const { extractToolCalls } = require(path.join(__dirname, '..', 'asserts.js'));

/** Longest excerpt of one field, in characters, before it is cut with `…`. */
const MAX_FIELD = 300;

/**
 * Collapse whitespace (newlines included), defuse runner commands and cut to
 * `max` characters.
 *
 * The excerpts carry model text and judge reasons. The Actions runner reads
 * `##[…]` anywhere in a line as a legacy workflow command, so a reply holding
 * `##[error]` or `##[stop-commands]` could annotate the job or swallow the
 * `::endgroup::` below; `## [` is inert. A `::command::` needs line start,
 * which an excerpt never has: every printed line begins with our own prefix.
 */
function clip(value, max = MAX_FIELD) {
  if (value == null) return '';
  const text = (typeof value === 'string' ? value : JSON.stringify(value))
    .replace(/\s+/g, ' ')
    .replace(/##\[/g, '## [')
    .trim();
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/** The test's description, falling back to its index. */
function describe(result, index) {
  return (
    (result.testCase && result.testCase.description) ||
    result.description ||
    `test #${index + 1}`
  );
}

/** The provider label/id the result came from, if any. */
function providerOf(result) {
  const p = result.provider;
  if (!p) return '';
  if (typeof p === 'string') return p;
  return p.label || p.id || '';
}

/** The text part of a provider output, tolerating the shapes asserts.js handles. */
function outputText(output) {
  if (output == null) return '';
  if (typeof output === 'string') return output;
  if (typeof output.content === 'string') return output.content;
  const msg =
    output.message ||
    (output.choices && output.choices[0] && output.choices[0].message);
  if (msg && typeof msg.content === 'string') return msg.content;
  return '';
}

/** One line per tool call: `name(args)`, args clipped. */
function toolCallLines(output) {
  return extractToolCalls(output).map((tc) => {
    const fn = tc.function || tc;
    return `${fn.name || '?'}(${clip(fn.arguments)})`;
  });
}

/** Asserts that did not pass, as `{ type, score, reason }`. */
function failedAsserts(result) {
  const grading = result.gradingResult;
  const components = (grading && grading.componentResults) || [];
  const failed = components
    .filter((c) => c && c.pass === false)
    .map((c) => ({
      type: (c.assertion && c.assertion.type) || 'assert',
      score: c.score,
      reason: c.reason,
    }));
  if (failed.length > 0) return failed;
  if (grading && grading.reason) {
    return [{ type: 'grading', score: grading.score, reason: grading.reason }];
  }
  if (result.error || result.failureReason) {
    return [{ type: 'error', score: result.score, reason: result.error || result.failureReason }];
  }
  return [{ type: 'unknown', score: result.score, reason: 'no grading result or error recorded' }];
}

/** Text report of one failed result, as an array of lines. */
function failureReport(result, index) {
  const provider = providerOf(result);
  const lines = [`✗ ${describe(result, index)}${provider ? ` [${provider}]` : ''}`];
  for (const a of failedAsserts(result)) {
    const score = typeof a.score === 'number' ? ` score=${a.score}` : '';
    lines.push(`  - ${a.type}${score}: ${clip(a.reason)}`);
  }
  const output = result.response && result.response.output;
  const calls = toolCallLines(output);
  for (const call of calls) lines.push(`  tool: ${call}`);
  const text = clip(outputText(output));
  if (text) lines.push(`  text: ${text}`);
  if (calls.length === 0 && !text) lines.push('  (no model output)');
  return lines;
}

/** Full report: summary line, then a `::group::` with every failure. */
function report(results, threshold) {
  const passed = results.filter((r) => r.success).length;
  const total = results.length;
  const pct = Math.round((100 * passed) / total);
  const ok = pct >= threshold;
  const lines = [`smoke: ${passed}/${total} (${pct}%) — threshold ${threshold}% — ${ok ? 'PASS' : 'FAIL'}`];
  const failures = results
    .map((r, i) => [r, i])
    .filter(([r]) => !r.success);
  if (failures.length > 0) {
    lines.push(`::group::failed cases (${failures.length})`);
    for (const [r, i] of failures) lines.push(...failureReport(r, i));
    lines.push('::endgroup::');
  }
  return { ok, lines };
}

function main() {
  const file = process.argv[2];
  const threshold = Number(process.argv[3] || 90);
  if (!file) {
    console.error('usage: smoke-check.js <results.json> [threshold]');
    process.exit(2);
  }

  let data;
  try {
    data = JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch (e) {
    console.error(`failed to read/parse ${file}: ${e.message}`);
    process.exit(2);
  }

  const results = (data.results && data.results.results) || [];
  if (results.length === 0) {
    console.error(`no test results found in ${file}`);
    process.exit(2);
  }

  const { ok, lines } = report(results, threshold);
  console.log(lines.join('\n'));
  process.exit(ok ? 0 : 1);
}

if (require.main === module) main();

module.exports = { clip, failedAsserts, failureReport, report };
