# gloop Graph Schema Notes

This document gives concise notes for machine-consuming or manual tooling.

## Graph API

- `apiVersion`: must be `gloop.dev/v1alpha1`
- `kind`: must be `Graph`
- `metadata`: name/version/labels/description
- `spec.goal`: required
- `spec.nodes`: non-empty
- `spec.edges`: validated against node ids and DAG shape

## Graph IDs and stability

- Node ids and graph names use lowercase DNS-like naming:
  - first char: `a-z`
  - following chars: `a-z`, `0-9`, `_`, `-`
- Graph names, node ids, edge endpoints, and resource ids are limited to 256 bytes.
- Validation produces errors and warnings:
  - errors block compile
  - warnings may still compile (for example shared resource serialization warning)

## Node policy and budgets

- `spec.policies.max_parallel` must be between 1 and 256 and is applied to the scheduler, nested graphs, and provider fan-out work.
- `context.max_bytes` on every node is bounded to 64 MiB (67_108_864 bytes).
- `retry.backoff_seconds` is bounded to 31_536_000 seconds.
- `node.timeout_seconds` and `run budgets wall_time_seconds` are each bounded to 31_536_000 seconds.
- `retry.max_attempts` must be between 1 and 16.
- `retry.rebind_profiles`: ordered fallback profiles for `agent`/`reduce`/`synthesize`; at most one entry per attempt after the first
- Provider retries are fail-closed. HTTP 429 is retryable because the call was rejected. Rebinding is allowed for failures detected before invocation (for example missing/unavailable profiles). Timeout, transport, 408/409/425/5xx, process, output-limit, and invalid-output failures are not retried or rebound because their side effects are uncertain.
- `subgraph` and `loop` failures are not retried as whole composite attempts; earlier inner nodes/iterations may already have completed. Configure retry on the specific inner agent-like nodes.
- `run budgets` are carried through execution state and summary models
- `fan_out` is an `agent`-only setting; `reduce` and `synthesize` execute with `fan_out = 1`.
- `output.max_bytes` on `agent`, `reduce`, `synthesize`, `command`, and `verify` nodes is bounded to 64 MiB (67_108_864 bytes).
- run output artifacts are accounted against a 64 MiB retained-output budget across attempts and nodes.

## Strict schema behavior

- `metadata`, `spec`, `node`, and node-`kind` maps use strict schema fields.
- Unknown fields are rejected (`serde(deny_unknown_fields)`) at graph, node, and kind parse level.
- Shared common node fields are `id`, `label`, `requires`, `resources`, `retry`, `timeout_seconds`, `workspace`, `context`, `continue_on_failure`, and `kind`.
- Kind-specific fields:
  - `agent`: `prompt`, `profile`, `model`, `fan_out`, `output`
  - `reduce`/`synthesize`: `prompt`, `profile`, `model`, `output`
  - `command`/`verify`: `argv`, `env`, `output`
  - `gate`: `message`, `default`
  - `loop`: `graph`, `until`, `max_iterations`, `stagnation_after`
  - `subgraph`: `graph`

## Workspace and reuse

- `WorkspaceSpec::Inherit { node }` requires a direct edge from `node` to the inheriting node in the same graph.
- `WorkspaceSpec::Current` uses the enclosing/default workspace path and propagates that workspace to nested executions (for nested graphs and loops).
- `WorkspaceSpec::Inherit { node }` reuses the exact recorded workspace of the named predecessor node (via workspace owner identity), and requires that predecessor to have completed successfully before reuse.
- `WorkspaceSpec::Worktree` captures run-time base commit (`HEAD` by default), supports explicit `base` override, and uses one retained isolated node worktree per run/node. Worktree paths are deterministic to the same node on retry.
- `WorkspaceSpec::Readonly` is validated but currently rejected at execution time without a filesystem sandbox.
- `WorkspaceSpec::Inherit { node }` requires a successful status edge from the source node.

## Agent-like node `profile` / `model`

- `agent`, `reduce`, `synthesize` nodes support optional fields:
  - `profile: String`
  - `model: String`
- `profile` and `model` are validated as non-empty when present.
- Profile references (including retry rebind entries) are capped at 64 bytes; model ids/aliases are capped at 512 bytes.
- `profile` and `model` are independent inputs to execution:
  - `profile` selects/requests the provider profile used for the node attempt.
  - `model` is passed as the requested model override for provider calls.
- Provider execution prefers a requested model over profile default model configuration.
- When `model` is absent, OpenAI/Anthropic profiles contribute their configured `model`; command profiles contribute no default model.
- Profile rebinding on retries affects `profile` only (`retry.rebind_profiles`), while `model` remains tied to the node definition.

## Loop and conditions

- Loops are bounded by `max_iterations` (1 through 1,024) and `stagnation_after`.
- Nested `loop`/`subgraph` structure is limited to depth 32 and 10,000 total nested nodes. Checked workload estimates reject overflow and excessive loop work before recursive validation or execution.
- Loop completion conditions use a node id plus `succeeded`/`skipped` status and an optional output contract (`json_pointer` + `equals`/`output_contains`). Failed, blocked, or cancelled nested nodes propagate failure instead of satisfying `until`.

## Edge constraints

- `conditional` edges require `when`
- condition statuses must be terminal; a `failure` edge may omit status or use only `failed`
- self-edges are rejected for outer graphs
- duplicate `(from,to,kind)` is accepted as warning
- unknown ids are rejected

## Event/state model

- `RunEventKind`: run start/finish, node lifecycle events, retry, and loop lifecycle markers.
- `FinalStatus`: `ready_for_human`, `failed`, `blocked`, `verification_failed`, `budget_exhausted`, `cancelled`

## Inline `gloop run --model`

- `--model` belongs to inline run generation (`gloop run <goal> ...`) and is applied only when no `--graph` file is used.
- In that path, `--model` is written to every generated `agent`/`reduce`/`synthesize` node before validation.
- `--graph` conflicts with `--model` and `--profile`; a saved graph controls model/profile behavior through its node fields.
