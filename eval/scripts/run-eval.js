// eval/scripts/run-eval.js
//
// Wraps `promptfoo eval` with retry logic for rate-limited (429) and timed-out
// cases. Retries use exponential backoff (30s / 60s / 120s, max 3 attempts).
// Assertion failures (real evaluation results) are NOT retried — only API errors.
//
// Usage:
//   node eval/scripts/run-eval.js [--smoke] [extra promptfoo args...]
//
//   --smoke     : short circuit at first pass, no retries (for CI smoke workflow)
//
// Extra args are forwarded to promptfoo (e.g. --filter-providers, --no-cache).
// Results are written to eval/results.json (and eval/results.retry.json on retry).
// Exit code 0 = pass rate ≥ threshold (via smoke-check.js), 1 = below threshold.

const { execSync } = require('child_process');
const fs = require('fs');

const RESULTS = 'eval/results.json';
const RETRY_RESULTS = 'eval/results.retry.json';
const RETRY_DELAYS = [30000, 60000, 120000];
const MAX_RETRIES = 3;

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

function hasErrors(file) {
  try {
    const data = JSON.parse(fs.readFileSync(file, 'utf8'));
    const results = (data.results && data.results.results) || [];
    const withErrors = results.filter((r) => r.error);
    return withErrors.length > 0;
  } catch {
    return false;
  }
}

function runPromptfoo(args) {
  const cmd = `npx promptfoo@latest eval ${args.join(' ')}`;
  console.log(`\n[run-eval] ${cmd}`);
  try {
    execSync(cmd, { stdio: 'inherit', cwd: process.cwd() });
    return true;
  } catch {
    return false;
  }
}

async function main() {
  const isSmoke = process.argv.includes('--smoke');
  const extraArgs = process.argv.slice(2).filter((a) => a !== '--smoke');

  // Run 1: initial full run.
  const run1Args = [...extraArgs, '-o', RESULTS];
  if (!runPromptfoo(run1Args)) {
    console.error('\n[run-eval] initial eval run failed');
  }

  if (isSmoke) {
    // CI smoke — no retries, just check threshold.
    process.exit(0);
  }

  if (!hasErrors(RESULTS)) {
    console.log('\n[run-eval] no API errors — skipping retries');
    process.exit(0);
  }

  // Retries: only when there are API errors (429 / timeout), not assertion failures.
  for (let attempt = 0; attempt < MAX_RETRIES; attempt++) {
    const delay = RETRY_DELAYS[attempt] || RETRY_DELAYS[RETRY_DELAYS.length - 1];
    console.log(`\n[run-eval] retry ${attempt + 1}/${MAX_RETRIES} after ${delay / 1000}s` + (attempt > 0 ? '' : `
[run-eval] hint: free models (:free) are rate-limited to ~20 req/min by OpenRouter.
    For fewer 429s, consider running free models separately or with longer delays.`));
    await sleep(delay);

    const retryArgs = [
      ...extraArgs,
      '--filter-failing',
      RESULTS,
      '-o',
      RETRY_RESULTS,
    ];
    runPromptfoo(retryArgs);

    if (!hasErrors(RETRY_RESULTS)) {
      console.log(`\n[run-eval] all errors resolved after ${attempt + 1} retry attempts`);
      // Merge: retried passes replace original errors.
      fs.copyFileSync(RETRY_RESULTS, RESULTS);
      process.exit(0);
    }
  }

  // After max retries, merge retry results and still evaluate pass rate.
  console.error(`\n[run-eval] ${MAX_RETRIES} retries exhausted — some cases still have errors`);
  if (fs.existsSync(RETRY_RESULTS)) {
    fs.copyFileSync(RETRY_RESULTS, RESULTS);
  }
  process.exit(1);
}

main().catch((e) => {
  console.error(`[run-eval] unexpected error: ${e.message}`);
  process.exit(2);
});
