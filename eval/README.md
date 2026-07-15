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

```text
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

## Environment variable (no secrets in the repo)

Models are accessed via [OpenRouter](https://openrouter.ai/). The API key is
read automatically from the `OPENROUTER_API_KEY` environment variable — it is
**never** committed (methodology §6). Set it once (User scope — persists across
shells and reboots):

```powershell
[Environment]::SetEnvironmentVariable("OPENROUTER_API_KEY", "sk-or-v1-...", "User")
```

Open a new shell afterwards (or use `$env:OPENROUTER_API_KEY = "sk-or-v1-..."`
for the current session only).

## Running

```bash
# run all configured models against all smoke tests (writes eval/results.json)
npx promptfoo@latest eval -c eval/promptfooconfig.yaml

# open the local web report (table: tests × models, PASS/FAIL, latency, cost)
npx promptfoo@latest view

# export a shareable report
npx promptfoo@latest eval -c eval/promptfooconfig.yaml -o results.html
```

To run fewer models, comment out their `- id: openrouter:...` block in
`promptfooconfig.yaml`. Run outputs (`results.*`, `.promptfoo/`) are gitignored.

## Models (changing them)

The three models live in the `providers:` list of `promptfooconfig.yaml`:

```yaml
providers:
  - id: openrouter:z-ai/glm-5.2
  - id: openrouter:qwen/qwen3.6-35b-a3b
  - id: openrouter:meta-llama/llama-3.1-8b-instruct
```

- The id is `openrouter:<slug>`, where `<slug>` is the model's OpenRouter ID
  (find it on https://openrouter.ai/models, e.g. `openai/gpt-5.4`).
- To swap/add/remove a model, edit these `id:` lines (and the `label:`).
- The tool schema is attached to every provider via the `&filar_tools` /
  `*filar_tools` YAML anchor — you do **not** repeat it per model.
- `tool_choice` is unset (= `auto`), matching filar's production behaviour: the
  model decides whether to call `run_command`. Set `tool_choice: "required"` on
  a provider to force a tool call (useful to confirm a model *can* call tools);
  do not use `required` for the safety-inversion cases — it would force a call
  even when the model should refuse.

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

- **OpenRouter as the router.** filar has no LiteLLM gateway; models are
  reached through OpenRouter (`openrouter:<slug>` providers) — a comparable
  single-endpoint router. The key is read from `OPENROUTER_API_KEY`.
- **Cost is available.** OpenRouter returns per-request usage and cost, so the
  promptfoo report includes cost (unlike the earlier no-gateway note).
- **Tool call, not text.** Asserts inspect tool-call structure (filar
  adaptation called out in the issue).
- **Tools live in `config.tools`.** promptfoo's OpenRouter provider does not
  forward the top-level `tools` key, so the schema is attached to each
  provider's `config.tools` via the `&filar_tools` YAML anchor.

## Dataset ownership

The dataset is filar's own, anonymised (methodology §4.5: no real hosts, IPs,
users or paths). It is populated in #73 and extended from real incidents
(methodology §10: "a bad answer in production → a new case").
