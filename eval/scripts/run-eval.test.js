// eval/scripts/run-eval.test.js
//
// Plain-Node unit tests for eval/scripts/run-eval.js (no test framework, same
// style as eval/asserts.test.js).
//
// These tests need neither the network nor OPENROUTER_API_KEY: PROMPTFOO_BIN is
// pointed at a stub that records the argv it was invoked with and writes a
// minimal results file. That is enough to pin the contract that broke in #369 —
// the wrapper dropped the mandatory `eval` subcommand while making the binary
// configurable, so promptfoo failed at argument parsing and eval-smoke stopped
// running for six weeks without anybody noticing.
//
// Run with:  node eval/scripts/run-eval.test.js
// (Node 18+ is required; it is NOT installed in the filar dev environment by
//  default — see eval/README.md.)

const assert = require('assert');
const { execFileSync } = require('child_process');
const fs = require('fs');
const os = require('os');
const path = require('path');

const REPO_ROOT = path.resolve(__dirname, '..', '..');
const RUN_EVAL = path.join('eval', 'scripts', 'run-eval.js');

let passed = 0;

function check(name, fn) {
  fn();
  passed++;
  console.log('ok -', name);
}

// --- harness --------------------------------------------------------------

// A stub standing in for the promptfoo binary. It appends its argv to
// $STUB_ARGV_LOG and honours `-o <file>` by writing a results file with a single
// passing case, so the wrapper can proceed as it would with the real binary.
const STUB = `
const fs = require('fs');
const argv = process.argv.slice(2);
fs.appendFileSync(process.env.STUB_ARGV_LOG, JSON.stringify(argv) + '\\n');
const i = argv.indexOf('-o');
if (i !== -1 && argv[i + 1]) {
  fs.mkdirSync(require('path').dirname(argv[i + 1]), { recursive: true });
  fs.writeFileSync(
    argv[i + 1],
    JSON.stringify({ results: { results: [{ success: true, id: 'stub' }] } })
  );
}
`;

// Runs the wrapper with the stub as PROMPTFOO_BIN and returns the argv lines the
// stub saw. Everything happens inside a throwaway directory so the repository's
// own eval/results.json is never touched.
function runWrapper(extraArgs) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), 'filar-369-'));
  try {
    const stubPath = path.join(tmp, 'promptfoo-stub.js');
    const argvLog = path.join(tmp, 'argv.log');
    fs.writeFileSync(stubPath, STUB);
    fs.writeFileSync(argvLog, '');
    fs.mkdirSync(path.join(tmp, 'eval', 'scripts'), { recursive: true });
    fs.copyFileSync(path.join(REPO_ROOT, RUN_EVAL), path.join(tmp, RUN_EVAL));
    fs.copyFileSync(
      path.join(REPO_ROOT, 'eval', 'scripts', 'smoke-check.js'),
      path.join(tmp, 'eval', 'scripts', 'smoke-check.js')
    );

    try {
      execFileSync(process.execPath, [RUN_EVAL, ...extraArgs], {
        cwd: tmp,
        stdio: 'pipe',
        env: {
          ...process.env,
          PROMPTFOO_BIN: `"${process.execPath}" "${stubPath}"`,
          STUB_ARGV_LOG: argvLog,
        },
      });
    } catch {
      // Exit code carries the pass-rate verdict; these tests only inspect argv.
    }

    return fs
      .readFileSync(argvLog, 'utf8')
      .split('\n')
      .filter(Boolean)
      .map((line) => JSON.parse(line));
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

// --- tests ----------------------------------------------------------------

check('passes the `eval` subcommand to the promptfoo binary (#369)', () => {
  const calls = runWrapper(['--smoke', '-c', 'eval/promptfooconfig.yaml']);
  assert.ok(calls.length > 0, 'the wrapper never invoked the binary');
  assert.strictEqual(
    calls[0][0],
    'eval',
    `first argument must be the "eval" subcommand, got: ${JSON.stringify(calls[0])}`
  );
});

check('appends the subcommand before the forwarded options', () => {
  const calls = runWrapper([
    '--smoke',
    '--filter-metadata',
    'smoke=true',
    '-c',
    'eval/promptfooconfig.yaml',
  ]);
  const argv = calls[0];
  // Options belong to the `eval` subcommand, so they must all follow it —
  // otherwise promptfoo rejects them as unknown options on the root program.
  assert.strictEqual(argv[0], 'eval');
  assert.ok(
    argv.indexOf('--filter-metadata') > 0,
    `forwarded options must come after "eval": ${JSON.stringify(argv)}`
  );
});

check('writes results to eval/results.json via -o', () => {
  const calls = runWrapper(['--smoke', '-c', 'eval/promptfooconfig.yaml']);
  const argv = calls[0];
  const i = argv.indexOf('-o');
  assert.ok(i !== -1, `missing -o in ${JSON.stringify(argv)}`);
  assert.strictEqual(argv[i + 1], 'eval/results.json');
});

console.log(`\n${passed} checks passed`);
