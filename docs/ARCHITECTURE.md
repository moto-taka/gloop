# gloop architecture

`gloop` has four Rust crates and one versioned Graph IR.

## Crates

- `gloop-core` owns graph/state types, strict validation, deterministic compilation, Mermaid/DOT rendering, and JSON Schema generation.
- `gloop-provider` owns layered profiles, capability routing, safe command invocation, OpenAI-compatible and Anthropic HTTP adapters, probing, and secret environment references.
- `gloop-runtime` owns the non-LLM scheduler, bounded retry/loops, budgets, resource locking, process execution, artifacts, the hash-chained journal, inspection, and scheduler replay.
- `gloop-cli` owns Clap commands, the interactive graph wizard, human/JSON presentation, Ctrl-C cancellation, and exit-code mapping.

## Foreground flow

```text
goal or Graph YAML
        |
        v
strict validation -> deterministic compile -> ready-node scheduler
                                             | agent/reduce/synthesize
                                             +-> provider registry -> CLI or HTTP adapter
                                             | command/verify
                                             +-> child process
                                             | loop/subgraph/gate
                                             +-> in-process controller
        |
        v
attempt artifacts + hash-chained journal + final summary
        |
        +-> inspect / logs / scheduler replay
```

The scheduler contains no model calls of its own. Models only run inside agent-like nodes. Independent ready nodes run up to `max_parallel`; nodes claiming the same resource are serialized even without a fake dependency edge.

## Provider selection

An agent-like node can name an optional `profile` and `model`. An explicit profile is validated and probed; otherwise the registry selects the highest-priority available profile satisfying the node/request capabilities. Project configuration overlays user configuration, which overlays built-ins.

The runtime renders ordered context files before compact dependency JSON and the
current task. It supplies the exact shared-prefix byte boundary and a
workspace/prefix-derived routing key to the adapter. OpenAI profiles opt into
Responses with `api = "responses"` and explicit prefix caching with
`cache_prefix = true`; command profiles may pass the key using `cache_key_args`.
Existing profiles retain their default behavior. Cache routing does not resume
provider conversations, and the adapter records provider-reported cache usage
without assuming a hit.

Configured output schemas are sent before generation and validated afterward.
The runtime journals `node_usage` before output validation so rejected answers
retain their token receipts. Input totals include cache reads and writes once;
reasoning tokens are a subset of output. Pi extraction selects completed
assistant text and excludes thinking blocks, prompt echoes, and error responses.

Provider errors retain a stable class through the runtime so the CLI can distinguish unresolved profiles, unavailable adapters, cancellation, budgets, and ordinary execution failures without matching human error text.
Verification failures retain their class through nested subgraphs and loops. Failure/failed-status edges receive a bounded metadata object containing the predecessor status, redacted error class/message, and artifact references when no normalized output exists.

## State and replay

Each run gets a fresh, validated run id and directory. Every node attempt records stdout, stderr, and normalized output. Journal rows contain a strict sequence and hash chain; replay rejects gaps, invalid transitions, post-finish events, truncated tails, and rows whose stored hash no longer matches. `summary.snapshot.json` is updated after terminal node transitions and `summary.json` is written once at completion.
When worktree mode runs, the run-level worktree manifest is written as an artifact with a recorded SHA-256 hash and attached to run events and summary coverage for inspect/replay traceability.

Scheduler replay reconstructs recorded state. Re-invoking an LLM with the same prompt is outside scheduler replay and is not deterministic.

## Security properties

- Graph/config structs reject unknown fields.
- Commands are spawned with argv arrays; prompts are not passed through an implicit shell.
- Prompt files and output-schema files are canonicalized and must remain inside the selected workspace.
- Output and context sizes are bounded.
- Profile secrets are environment references; debug/doctor output does not reveal secret values.
- Artifact path components are validated and run directories cannot be silently reused.

## Deliberate boundaries

- No daemon or cross-project queue.
- No provider-account leasing or background submit/watch protocol.
- `readonly` cannot claim enforcement without a filesystem sandbox and fails explicitly.
- `worktree` is implemented in the runtime and uses retained sibling worktrees with captured base commit, optional explicit base override, retry reuse by node identity, and no automatic merge/push/delete of retained branches.
- `inherit` records and reuses the exact owning node workspace path (`workspace identity`) before scheduling that dependency.
- `worktree` failures can leave dirty worktrees retained for inspection and replay evidence, including final auto-commit state when enabled.
- Process cancellation uses process groups on Unix and falls back to direct-child cancellation where process-group control is unavailable.
