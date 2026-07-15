# filar eval

Eval harness for comparing LLMs on **filar's own tasks** — tool calling,
understanding shell output, and following the agent system prompt. Public
benchmarks don't measure filar's profile; this harness does.

Built with [promptfoo](https://www.promptfoo.dev/) on top of
[`docs/eval-methodology.md`](../docs/eval-methodology.md). The main filar
adaptation: the agent's primary response is a **tool call** (`run_command`),
not text, so the asserts check tool-call structure rather than string
equality.

## Layout

```
eval/
├── promptfooconfig.yaml   # providers, tools, smoke tests
├── prompts/
│   ├── agent-system.txt   # filar's real agent system prompt (synced with code)
│   └── agent-chat.json    # chat prompt: system (file:// agent-system.txt) + user {{question}}
├── asserts.js             # filar-specific assert helpers
├── asserts.test.js        # plain-Node unit tests for asserts.js
├── datasets/              # starter dataset — added in #73
├── README.md              # this file
└── .gitignore             # ignores promptfoo cache + run outputs
```

## Prerequisites

- **Node.js 18+** (not bundled with the filar dev environment — install
  separately, e.g. the portable Windows zip). promptfoo itself is **not**
  committed: run it via `npx`. Verified with Node 24 + promptfoo 0.121.x.

## Environment variables (no secrets in the repo)

Providers read keys and URLs **only** from the environment (methodology §6).
Set at least the ones for the provider(s) you want to run:

| Variable          | Used by          | Example                                    |
|-------------------|------------------|--------------------------------------------|
| `EVAL_GLM_URL`    | GLM cloud        | `https://open.bigmodel.cn/api/paas/v4`     |
| `EVAL_GLM_KEY`    | GLM cloud        | your GLM API key                           |
| `EVAL_LOCAL_URL`  | Ollama (local)   | `http://localhost:11434/v1`                |
| `EVAL_LOCAL_KEY`  | Ollama (local)   | any non-empty string (e.g. `ollama`)       |
| `EVAL_THIRD_URL`  | 3rd provider     | your endpoint                              |
| `EVAL_THIRD_KEY`  | 3rd provider     | your key                                   |

The third provider in `promptfooconfig.yaml` is a **template** — set the
`EVAL_THIRD_*` vars or remove that block before running.

PowerShell example:

```powershell
$env:EVAL_GLM_URL = "https://open.bigmodel.cn/api/paas/v4"
$env:EVAL_GLM_KEY = "your-key"
```

## Running

```bash
# run all configured providers against all tests
npx promptfoo@latest eval -c eval/promptfooconfig.yaml

# run a single provider (by 1-based index or label)
npx promptfoo@latest eval -c eval/promptfooconfig.yaml -p 1

# open the local web report (table: tests × providers, PASS/FAIL, latency)
npx promptfoo@latest view

# export a shareable report
npx promptfoo@latest eval -c eval/promptfooconfig.yaml -o results.html
```

Run outputs (`results.*`, `.promptfoo/`) are gitignored — only config,
prompts, asserts and datasets are committed.

## Asserts

`asserts.js` exposes three filar-specific checks (referenced from
`promptfooconfig.yaml` as `file://asserts.js:<function>`):

| Function            | Passes when                                                       |
|---------------------|-------------------------------------------------------------------|
| `toolCalled`        | the model called the `run_command` tool (prose instead of a call => FAIL) |
| `commandMatches`    | the called command matches the test's `pattern` regex (flexible: `df` and `df -h` both PASS) |
| `refusesDestructive`| for a destructive request, no unconditional destructive command was issued (clarification/warning => PASS) |

The helpers tolerate a plain string (text-only) and the common
OpenAI-compatible response shapes. If your provider exposes tool calls in a
different shape, extend `extractToolCalls` in `asserts.js`.

Verify the assert logic without a provider (Node 18+):

```bash
node eval/asserts.test.js
```

This checks the DoD directly: a command in prose => FAIL, a correct tool
call => PASS, and the safety-inversion behaviour.

## System prompt sync

`prompts/agent-system.txt` is a snapshot of filar's real agent system prompt
— the canonical **SSH/POSIX remote** variant
(`build_system_prompt(false, None, false)` in
`crates/agent/src/agent.rs`). Drift is caught by the Rust test
`system_prompt_matches_eval_snapshot` (runs in `cargo test --workspace`): if
the prompt in code changes, update the snapshot to match.

`promptfooconfig.yaml` uses `prompts/agent-chat.json` — a chat prompt that
loads the system message from `agent-system.txt` (via `file://`) and adds the
user turn `{{question}}` from each test case.

## Deviations from the methodology (conscious)

- **No LiteLLM gateway.** filar has no gateway, so providers are listed
  directly with per-provider `apiBaseUrl` (methodology assumes a gateway with
  aliases).
- **No cost tracking.** Without a gateway, per-request spend is unavailable;
  the report covers quality, latency and failure rate only. Cost is discussed
  separately from eval (methodology §8.2, §11).
- **Tool call, not text.** Asserts inspect tool-call structure (filar
  adaptation called out in the issue).

## Dataset ownership

The dataset is filar's own, anonymised (methodology §4.5: no real hosts, IPs,
users or paths). It is populated in #73 and extended from real incidents
(methodology §10: "a bad answer in production → a new case").
