# Advanced usage

[Back to the README](../README.md)

## Prompt, context, harness, graph, and loop engineering

These are first-class parts of gloop's graph IR rather than hidden conventions:

- **Prompt engineering** — provider nodes accept inline prompts or external prompt packages with optional version metadata. Packages support bounded variable substitution (`{{name}}`), node identity (`{{node_id}}`), explicit dependency insertion (`{{dependencies}}`), and dedicated `reduce`/`synthesize` stages for turning many outputs into a deliberate next prompt. Output contracts can require text or JSON validated against a schema.
- **Context engineering** — every node declares its context budget and can select workspace files plus predecessor outputs. Context is rendered deterministically, kept inside the selected workspace, and rejected when it exceeds the byte limit; this makes the prompt/context boundary inspectable in run artifacts. gloop provides explicit context composition, not an implicit vector database or automatic retrieval layer.
- **Harness engineering** — provider profiles isolate the execution harness from graph logic. The same node can target built-in CLIs (Codex, Claude, Qwen, Cursor, Pi, OpenCode), any generic command, OpenAI-compatible HTTP (including OpenRouter), or Anthropic-compatible HTTP. Capability routing, credential references, output normalization, process-group cancellation, workspaces, retries, artifacts, and journal replay are handled by the runtime around each invocation.
- **Graph and loop engineering** — outer graphs are validated DAGs with fan-out/fan-in, conditional and failure edges, resource serialization, and nested subgraphs. Repetition is explicit and bounded through `loop` nodes with success/output/JSON conditions, stagnation guards, and a hard iteration cap. Use `gloop graph new --interactive` to author these flows from the CLI.

The boundary is intentional: gloop supplies the controllable building blocks for these practices; it does not silently rewrite prompts, invent context, or run an unbounded autonomous loop.

## Execution model

The runtime implements:

- deterministic DAG compilation and foreground scheduling;
- fan-out/fan-in, conditional failure edges, subgraphs, and bounded nested loops;
- maximum parallelism (256), graph nesting (32), nested node count (10,000), model-call and wall-time budgets;
- bounded retry (at most 16 attempts) with ordered profile rebinding, loops capped at 1,024 iterations;
- fail-closed provider retry safety: HTTP 429 rejections may retry, and profile rebinding may recover failures detected before invocation; timeout, transport, 408/409/425/5xx, process, oversized-output, and invalid-output failures are never replayed because the original request may already have been accepted;
- failed `subgraph`/`loop` attempts are never replayed as a whole because earlier children or iterations may already have succeeded; put retry policies on the individual inner provider nodes instead;
- serialization of nodes that claim the same resource;
- command and verification nodes with capped output;
- run-wide retained-output accounting for stdout/stderr/raw output (64 MiB total);
- `max_parallel` is a scheduler-wide cap; nested graphs inherit and can lower it (effective cap is the tighter bound in scope);
- typed provider failures and status-specific CLI exit codes;
- per-attempt stdout, stderr, normalized output, summary snapshots, and a hash-chained JSONL journal;
- scheduler replay and completed-run inspection.

Roles are prompt data, not hardcoded runtime concepts. Graph structure, profile, model, prompt, output schema, and retry policy remain configurable per node.

## Provider profiles

Profiles are layered in this order:

1. built-ins;
2. the OS-specific user config directory (`gloop/profiles.toml`);
3. `<project>/.gloop/profiles.toml` (opt-in only via `--trust-project-profiles`).

The built-in command profiles are `codex`, `claude`, `qwen`, `cursor-agent`, `pi`, and `opencode`.

Profile kinds:

- `command` for local executables.
- `openai` for OpenAI-compatible HTTP providers.
- `anthropic` for Anthropic-compatible HTTP providers.

Provider execution uses environment-sourced credentials (`*_env`/`headers_from`) and local command profiles run with a constrained environment:

- the command process clears inherited environment and only reintroduces required allowlist entries and mapped `env_from` values,
- sensitive provider keys are redacted from logs and event output.
- command version probes check required credential presence but do not inject mapped `env_from` secret values into the probe process.

`HOME` is intentionally retained for command profiles so authenticated harnesses such as
Codex CLI, Claude Code, Qwen, Pi, and OpenCode can reuse their normal user login. Treat every
configured command harness as trusted local code: it can read files that its operating-system
user can read, including harness-owned authentication state. Project profiles remain disabled
unless `--trust-project-profiles` is supplied. Gloop does not add a general filesystem sandbox;
among the built-ins, `native_sandbox` is currently declared only by the Codex profile.

Example generic command profile:

```toml
[profiles.my-agent]
kind = "command"
argv = ["my-agent", "run"]
prompt_mode = "argument"
prompt_args = ["--prompt", "{prompt}"]
model_args = ["--model", "{model}"]
version_args = ["--version"]
output = "jsonl"
output_pointer = "/result"
timeout_seconds = 900
```

Example OpenAI-compatible profile:

```toml
[profiles.local-openai]
kind = "openai"
base_url = "http://127.0.0.1:8000/v1"
model = "local-model"
# Optional for endpoints that do not require authentication:
# api_key_env = "LOCAL_OPENAI_API_KEY"
```

OpenRouter uses the same adapter. Keep the key in the environment and choose any
OpenRouter model id in the profile:

```toml
[profiles.openrouter]
kind = "openai"
base_url = "https://openrouter.ai/api/v1/"
model = "openai/gpt-oss-20b:free"
api_key_env = "OPENROUTER_API_KEY"
timeout_seconds = 90
parameters = { max_tokens = 256 }
```

Copy [examples/openrouter-profiles.toml](../examples/openrouter-profiles.toml) to
`.gloop/profiles.toml`, then run with `--trust-project-profiles`. The example
also includes `openrouter-deepseek-v4-flash` and `openrouter-luna`; provider
model catalogs can change, so confirm their current model ids before a run.

[examples/openrouter-json.yaml](../examples/openrouter-json.yaml) is an OpenRouter
JSON-schema example. Reasoning models can consume output budget before emitting
assistant text, so use practical `max_tokens` values.

For the OpenAI Responses API, set `api = "responses"`. To cache the shared
`context.files` prefix explicitly, also set `cache_prefix = true`; this requires
a model supporting explicit prompt cache breakpoints, such as GPT-5.6 or later.
See [examples/openai-cache-profiles.toml](../examples/openai-cache-profiles.toml)
and [examples/openai-cache-prefix.yaml](../examples/openai-cache-prefix.yaml).
Merge the profile into `.gloop/profiles.toml`, supply `OPENAI_API_KEY` through the
environment, and run:

```bash
gloop run --graph examples/openai-cache-prefix.yaml --trust-project-profiles
```

This is an API-key connection, separate from the Codex CLI's ChatGPT login.
Actual cache usage must be checked in `node_usage` events; configuration alone
does not establish a cache hit. [Context/cache semantics](SCHEMA.md#caching-a-shared-file-prefix-through-the-responses-api)
explain the boundary and routing key.

With an existing Pi ChatGPT login, the alternative
[Pi profile](../examples/pi-cache-profiles.toml) passes a stable routing key
while starting each node with empty history. Merge it into `.gloop/profiles.toml`
and run `gloop run --graph examples/pi-cache-prefix.yaml --trust-project-profiles`.
This example uses GPT-5.5 and requires Pi's `--session-id` and `--no-session`
options (tested with Pi 0.85.1). Check the actual `node_usage` receipts; a routing
key alone does not guarantee a cache hit.

Example Anthropic-compatible profile:

```toml
[profiles.anthropic-api]
kind = "anthropic"
model = "claude-sonnet-4-5"
api_key_env = "ANTHROPIC_API_KEY"
max_tokens = 8192
```

Secrets are referenced by environment-variable name and are not written into the profile. Useful commands:

```bash
gloop provider list --json
gloop provider probe codex --json
gloop provider doctor --json
gloop provider add my-agent 'kind = "command" argv = ["my-agent"]'
```

## Graph IR

The schema version is `gloop.dev/v1alpha1`. Supported node kinds are `agent`, `command`, `reduce`, `synthesize`, `verify`, `gate`, `loop`, and `subgraph`. Supported edge kinds are `data`, `control`, `resource`, `conditional`, and `failure`.

Unknown fields are rejected. Outer graphs must be acyclic; repetition is represented by a bounded `loop` node. Agent-like nodes accept optional `profile` and `model` bindings, while output contracts can require text or JSON and an inline or file-based JSON Schema.

Generate the complete machine-readable schema with:

```bash
gloop graph schema --json
```

See [docs/SCHEMA.md](SCHEMA.md) and the graphs in [examples](../examples).

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | ready for human / command succeeded |
| 2 | blocked or gate rejected |
| 3 | node or verification failure |
| 4 | provider/adapter unavailable |
| 5 | budget exhausted |
| 6 | invalid graph or arguments |
| 7 | unresolved provider profile/capability |
| 130 | cancelled |

## Known limitations

- Manual and `gloop run` execute in the foreground; `gloop start` and Auto/direct tasks use independent background workers. There is no cross-project queue or account lease.
- `readonly` is intentionally unsupported without a filesystem sandbox; the run fails explicitly when requested.
- `worktree` workspace mode uses a dedicated runtime manager for isolated sibling worktrees. It:
  - runs a Git preflight with `.gloop` excluded from cleanliness checks,
  - captures the base commit from `HEAD` (or uses a caller-provided explicit base),
  - generates unique retained worktree branches and paths per run/node,
  - reuses the same node worktree across retries,
  - preserves dirty worktrees on failed/cancelled attempts,
  - does not push, merge, or auto-delete retained worktrees.
- Worktree mode disables hooks, external filesystem monitors, and external diff/textconv helpers. It re-checks and rejects repository-local `filter.*.clean`, `filter.*.smudge`, `filter.*.process`, `diff.*.command`, and `diff.*.textconv` configuration before Git operations because those programs could otherwise execute during checkout/staging/inspection; use `current`/`inherit` or remove the local driver configuration.
- Final successful worktree nodes can auto-commit into their dedicated branch when `auto_commit` is enabled, and `inherit` reuses the true source workspace by identity.
- Cancellation is process-group based on Unix builds (`ProcessGroup`) and direct-child on non-Unix platforms.
- Profiles accept arbitrary model ids and aliases. Command profiles expose their
  model list in the GUI and TUI selectors, discovered at launch from each CLI's
  own listing command: `cursor-agent`/`pi` use `--list-models`, `opencode` uses
  `models`, `aider` uses `--list-models ""` (bundled offline catalog), `codex`
  uses `codex debug models` (only `visibility: "list"` entries), and
  `claude`/`qwen` answer a client-side `/model` probe (`claude --bare -p
  /model`, `qwen --safe-mode -p /model`) without a model call; `qwen` currently
  reports its active model only. Remote provider catalogs are still not
  enumerated, and each node invocation is fresh.
- Empty model/provider outputs are rejected when they do not satisfy node output contracts (including text/JSON output mode checks).
- Serialized HTTP provider request bodies are capped at 1 MiB; profile `parameters` maps are capped at 256 entries and 256 KiB serialized.
- Replay validates hash-chain integrity, run-id/sequence order, and schema compatibility before accepting a rerun; replay rehydrates scheduler state from events and summary checks. These unkeyed hashes detect partial/corrupt edits, not a same-user attacker who can consistently rewrite the whole run directory.
- Replay does not promise byte-identical re-execution of an LLM.
- External provider CLI/API credentials and billing remain provider-owned. Gloop surfaces provider-level auth/usage failures; it does not charge on behalf of providers.
