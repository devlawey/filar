// eval/scripts/smoke-check.js
//
// Reads a promptfoo results.json, prints the pass rate, and exits 0 if it meets
// the threshold (default 90), else 1. Used by the eval-smoke CI workflow to gate
// the regression smoke run.
//
// Usage: node eval/scripts/smoke-check.js <results.json> [threshold]
const fs = require('fs');

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

const passed = results.filter((r) => r.success).length;
const total = results.length;
const pct = Math.round((100 * passed) / total);
const ok = pct >= threshold;

console.log(`smoke: ${passed}/${total} (${pct}%) — threshold ${threshold}% — ${ok ? 'PASS' : 'FAIL'}`);
process.exit(ok ? 0 : 1);
