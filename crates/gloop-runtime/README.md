# gloop-runtime

`gloop-runtime` executes a compiled graph in the calling process. It has no
queue, daemon, background worker, or implicit remote fallback.

## Runtime boundaries

- `WorkspaceSpec::Current` is the primary execution mode.
- `WorkspaceSpec::Inherit` reuses a successfully completed predecessor's
  recorded workspace. The graph must order that predecessor before the
  inheriting node.
- `WorkspaceSpec::Worktree` creates isolated sibling worktrees under
  `<repo>/.gloop/worktrees/<run-id>/` and is recorded with branch/name metadata.
  The repository is prechecked once for clean state (excluding `.gloop`), the
  source base commit is captured from `HEAD` (or overridden by an explicit
  workspace base), and failed/cancelled worktrees are retained for replay/inspection.
  On final successful node completion, the source worktree is committed when
  `auto_commit` is enabled and then marked as clean/dirty in the manifest.
- The worktree manifest is emitted as an artifact with SHA-256 metadata and is
  referenced in run-level journal/summary coverage.
- `WorkspaceSpec::Readonly` fails unless an enforced filesystem sandbox is
  added; the runtime does not pretend that an ordinary directory is read-only.
- Command/process cancellation uses process groups on Unix (`ProcessGroup`) and
  direct-child on non-Unix platforms.

Each run writes `graph.json`, atomic `summary.snapshot.json` checkpoints, final
`summary.json`, and per-attempt `stdout.txt`, `stderr.txt`, and output files.
`journal.jsonl` is the only path with per-append durability syncing (`sync_data`)
and is SHA-256 hash-chained; other artifact writes use temp-file write+rename
semantics, so durability is effectively best-effort on those paths.
Replay rejects sequence gaps, changed rows, broken hash links, a missing
terminal event, and an interrupted final JSONL row. Completed-run inspection
also verifies the journal artifact hash recorded by the final summary, so a
complete-row tail deletion is rejected. Interior damage is never ignored.
