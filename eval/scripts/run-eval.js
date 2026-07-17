// eval/scripts/run-eval.js
//
// Wraps `promptfoo eval` with retry logic for rate-limited (429) and timed-out
// cases. Retries use exponential backoff (30s / 60s / 120s, max 3 attempts).
// Only cases with API errors (429 / timeout / network) are retried; assertion
// failures are preserved as-is.
//
// Usage:
//   node eval/scripts/run-eval.js [--smoke] [extra promptfoo args...]
//
//   --smoke     : short circuit at first pass, no retries (for CI smoke workflow)
//
// Extra args are forwarded to promptfoo. The promptfoo binary is resolved as
// `npx promptfoo` by default; set PROMPTFOO_BIN env to override.  Results are
// written to eval/results.json (and eval/results.retry.json on retry).
// Exit code 0 = pass rate ≥ threshold (via smoke-check.js), 1 = below threshold,
// 2 = script error (missing results, unexpected failure).

const { execSync } = require('child_process');
const fs = require('fs');

const PROMPTFOO_BIN = process.env.PROMPTFOO_BIN || 'npx promptfoo';

const RESULTS = 'eval/results.json';
const RETRY_RESULTS = 'eval/results.retry.json';
const RETRY_DELAYS = [30000, 60000, 120000];
const MAX_RETRIES = 3;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function promptfoo(args) {
  const cmd = `${PROMPTFOO_BIN} ${args.join(' ')}`;
  console.log(`\n[run-eval] ${cmd}`);
  try {
    execSync(cmd, { stdio: 'inherit', cwd: process.cwd() });
    return true;
  } catch {
    return false;
  }
}

function loadResults(file) {
  try {
    return JSON.parse(fs.readFileSync(file, 'utf8'));
  } catch {
    return null;
  }
}

function errorCaseIds(file) {
  const data = loadResults(file);
  if (!data) return [];
  const results = (data.results && data.results.results) || [];
  return results.filter((r) => r.error).map((r) => r.id || r.description);
}

// Merge retry results back into the original results, keeping passing cases from
// run 1 and overlaying retried (re-run) results for the failing subset.
function mergeResults(originalFile, retryFile, outFile) {
  const orig = loadResults(originalFile);
  const retry = loadResults(retryFile);
  if (!orig || !retry) return false;

  const origResults = (orig.results && orig.results.results) || [];
  const retryResults = (retry.results && retry.results.results) || [];

  // Build a lookup by test id/description for the retried results.
  const retryMap = new Map();
  for (const r of retryResults) {
    const key = r.id || r.description;
    if (key) retryMap.set(key, r);
  }

  // Overlay: replace each original result if retry has a (newer) version.
  const merged = origResults.map((r) => {
    const key = r.id || r.description;
    return retryMap.has(key) ? retryMap.get(key) : r;
  });

  const mergedData = { ...orig, results: { ...orig.results, results: merged } };
  fs.writeFileSync(outFile, JSON.stringify(mergedData, null, 2));
  return true;
}

function filterResultsToErrors(file, outFile) {
  const data = loadResults(file);
  if (!data) return false;

  const results = (data.results && data.results.results) || [];
  const errors = results.filter((r) => r.error);
  const passes = results.filter((r) => !r.error);

  console.log(`[run-eval] filtering: ${passes.length} pass + ${errors.length} error cases`);
  if (errors.length === 0) return false;

  const filtered = { ...data, results: { ...data.results, results: errors } };
  fs.writeFileSync(outFile, JSON.stringify(filtered, null, 2));
  return true;
}

async function main() {
  const isSmoke = process.argv.includes('--smoke');
  // Drop user-supplied -o — the wrapper manages output file naming.
  const extraArgs = process.argv.slice(2).filter((a) => a !== '--smoke').filter((a, i, arr) => {
    if (a === '-o') return false;
    if (i > 0 && arr[i - 1] === '-o') return false;
    return true;
  });

  // Run 1: initial full run.
  const run1Args = [...extraArgs, '-o', RESULTS];
  const run1ok = promptfoo(run1Args);

  if (isSmoke) {
    if (!run1ok || !fs.existsSync(RESULTS)) {
      console.error('[run-eval] smoke run failed — no results produced');
      process.exit(1);
    }
    // CI smoke — no retries; pass-rate check is a separate CI step.
    process.exit(0);
  }

  if (!run1ok) {
    console.error('[run-eval] initial eval run failed');
  }

  if (!fs.existsSync(RESULTS)) {
    console.error('[run-eval] no results file produced');
    process.exit(2);
  }

  const errorIds = errorCaseIds(RESULTS);
  if (errorIds.length === 0) {
    console.log(`\n[run-eval] no API errors — skipping retries`);
    process.exit(0);
  }
  console.log(`\n[run-eval] ${errorIds.length} cases with API errors — will retry`);

  for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
    const delay = RETRY_DELAYS[attempt] || RETRY_DELAYS[RETRY_DELAYS.length - 1];
    console.log(`\n[run-eval] retry ${attempt + 1}/${MAX_RETRIES} after ${delay / 1000}s` + (attempt > 0 ? '' : `
[run-eval] hint: free models (:free) are rate-limited to ~20 req/min by OpenRouter.
    For fewer 429s, consider running free models separately or with longer delays.`));
    await sleep(delay);

    // Filter RESULTS to error-only, then retry only those cases.
    const filterOk = filterResultsToErrors(RESULTS, 'eval/results.errors.json');
    if (!filterOk) {
      console.log('[run-eval] no errors to retry — exiting retry loop');
      break;
    }

    promptfoo([
      ...extraArgs,
      '--filter-failing',
      'eval/results.errors.json',
      '-o',
      RETRY_RESULTS,
    ]);

    if (!fs.existsSync(RETRY_RESULTS)) continue;

    const stillErrors = errorCaseIds(RETRY_RESULTS);
    // Merge retry results back: keep run-1 passes, overlay retried cases.
    mergeResults(RESULTS, RETRY_RESULTS, RESULTS);

    if (stillErrors.length === 0) {
      console.log(`\n[run-eval] all errors resolved after ${attempt + 1} retry attempts`);
      process.exit(0);
    }
    console.log(`[run-eval] ${stillErrors.length} cases still have API errors`);
  }

  console.error(`\n[run-eval] ${MAX_RETRIES} retries exhausted — some cases still have errors`);
  process.exit(1);
}

main().catch((e) => {
  console.error(`[run-eval] unexpected error: ${e.message}`);
  process.exit(2);
});
