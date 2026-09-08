# Independent tasks and the terminal workspace

`gloop` (or `gloop tui`) opens the task TUI for the current project.
The home screen lists recently saved graphs first: Enter opens the selected graph,
and `r` runs it. **+ Graph · Manual** starts a blank canvas; **Auto** proposes a
workflow from a request. **1 AI** keeps the single-task path, and **Background run**
launches a saved workflow with the independent lifecycle below.

In an independent task's result view, ↑/↓ scroll, Tab selects an action, and Enter activates it.
Choose **Ask another model / follow up** to pass the result to a new task. Choose
**Stop this task** to cancel. `q` or Ctrl-C closes the view while the task continues.
Prompts accept multiline paste; Alt-Enter inserts a newline and Enter advances.

`gloop ui` opens the optional local browser workspace. Both interfaces use the same
independent workers and task history.

## Manual: author and run your own graph

Manual opens directly on the graph. It uses the same Graph IR and runtime as
`gloop run`; no planner generates or rewrites the workflow. Loading a file preserves
node kinds, provider/model choices, workspace settings, connections, conditions,
budgets, and policies. The Auto plan's eight-step limit does not apply.

| Key | Action |
| --- | --- |
| `a` | Type an instruction and Enter to add an AI step after the selection. |
| `Enter` | Edit the selected instruction directly. On an empty graph, add AI. |
| `A` | Add a sibling branch, sharing the selected step's prerequisites. |
| `c` | Select a target and Enter to connect; Esc cancels. |
| `p` / `m` | Choose the selected AI's tool / model. |
| `r` | Validate and run the displayed graph immediately. |
| `s` | Save; newly created graphs get an available workflow-N name. |
| `O` | Open a saved graph or enter a YAML path. |
| `Tab` | Commands, tests, approvals, node settings, connections, limits, Save as. |

A new AI step reuses the selected AI's tool/model. Switching its tool clears the
old tool's model override. Cancelling the new-instruction input adds nothing.
Numbered arrows show branches and merges; status symbols appear in the same graph
during execution. The selected instruction or result is visible beside it.
Full output remains available with `o`.

The first instruction supplies the initial goal and step label; neither needs a
separate input screen. These remain editable. Limits are visible above the canvas:
a new graph starts with at most 16 AI calls and 1800 seconds, while an opened graph
retains its own limits. **Tab → Execution settings** provides an optional review;
`r` starts immediately after validation without another confirmation screen.
Invalid graphs, empty commands, and cyclic connections are rejected. Advanced node
kinds and conditional edges remain editable in their YAML settings.

The Manual monitor supports interactive approvals; `q` or Ctrl-C requests cancellation
while a run is active. After completion, select a node and press `o` for its output.
The graph snapshot, journal, summary, and artifacts are stored in `.gloop/runs/ID/`.

**Save as** refuses an existing file name. Normal **Save** checks that the opened
file has not changed elsewhere before updating it. Switching graphs with unsaved
edits offers a choice to return or discard those edits. Text fields support multiline
paste, Alt-Enter for a newline, Ctrl-U to clear, and Enter to apply.

For execution that continues independently of the monitor, save the graph and choose
**Background run** from home, or use `gloop start --graph PATH --max-calls N`.
That path uses the independent task lifecycle below; graphs needing interactive
approval should run in the Manual monitor.

## From a request to a reviewed workflow

The planner receives the request, a bounded project file outline, and a README
excerpt. One invocation proposes 1–8 steps as JSON with names, instructions,
completion criteria, relative file paths, and prerequisite step IDs. The worker
validates the structure, limits, paths, IDs, and acyclic dependencies. An invalid
proposal fails visibly; it does not retry or execute the proposed work.

The TUI opens a successful proposal as an editable list. Every step initially uses
the selected planning tool/model. Open a step to inspect its full instructions and
criteria, assign a different tool/model, or change prerequisites. Page Up/Down scroll
long descriptions. Adding, removing, and editing steps makes no AI calls. Removing
a step retains its prerequisites for steps that depended on it. Cycles are rejected.

**Run these steps** prepares the reviewed graph and opens a separate confirmation
screen. Execution begins only after **Start task**. Steps run one at a time, passing
prerequisite outputs along data edges. The runtime allows at most one invocation
per step and does not ask a model to decide the next action. Completion criteria
are instructions for the selected AI to verify, not independently enforced tests.

**Save this workflow** writes `.gloop/graphs/NAME.yaml` without overwriting an
existing file. It stores the exact reviewed instructions, assignments, dependencies,
and call budget. The home menu's **Background run** can run it later. Edits are
kept in the current TUI until you save or submit them; reopening the planning job
loads its original proposal. Creating another proposal requires an explicit new call.

CLI proposal-only submission is also available:

```sh
gloop start "Add a search screen and verify it" --plan --profile codex --json
gloop tasks PLAN_TASK_ID --wait --timeout 60 --json
```

`--plan` fixes the invocation limit at one and cannot be combined with `--graph`,
`--after`, or a second opinion. Planning jobs record `job.request.kind: "planning"`;
the validated proposal is in `nodes.plan.output`. The TUI history can reopen it for
review. The optional browser retains its direct-task workflow.

The planning prompt allows project-file inspection and requests no implementation
or subagent delegation. It is not a separate filesystem sandbox; provider
permissions remain authoritative.

## Start and reconnect

```sh
gloop tui --repo /path/to/project --lang ja
gloop ui --repo /path/to/project --no-open  # print the loopback URL
gloop start "Investigate the failing test" --profile codex --model gpt-5.6-luna --json
gloop tasks --repo /path/to/project --json
```

The selected CLI must already be installed and signed in. The workspace probes
executable availability and discovers models using gloop's existing provider
catalogs. Availability means the executable/version check passed; it does not
prove authentication or access to a particular model. API profiles use the existing
provider configuration and environment-variable credential references.

Project-local provider profiles still require `--trust-project-profiles`. This
flag is captured in the task request and applied by the independent worker. The
workspace does not change provider configuration, grant permissions, or install CLIs.

Closing the TUI, browser, workspace server, or launcher does not stop an independent
task. Each task runs in its own process group on Unix, with stdin detached and
stdout/stderr stored locally. It does not inherit a supervisor conversation.
The machine must stay running; this is not a remote execution service.

## Results and handoff

```sh
gloop tasks TASK_ID --json
gloop tasks TASK_ID --wait --timeout 60 --json
gloop start "Review this result" --profile claude --after TASK_ID --json
gloop start "Implement it and request a second opinion" \
  --profile codex --review-profile claude --max-calls 2 --json
gloop start --graph workflow.yaml --max-calls 8 --timeout 1800 --json
```

`--after` requires a finished task with readable execution results in the same
project. It starts a new invocation; it does not resume a provider's private chat
session. The previous request, status, provider/model names, results, and errors
form a reference attachment capped at 24 KiB on a UTF-8 boundary. Older turns are
not automatically replayed. `job.handoff_bytes` and `job.handoff_truncated` record
what was attached. The new user request is kept outside that bounded attachment.

The second-opinion step receives the first step's result through the existing
graph dependency/context machinery. It is asked to inspect evidence without
editing files; that request is not a separate filesystem sandbox. Provider
permissions remain authoritative.

`gloop tasks ID --json` returns one object containing `id`, `job`, `status`,
`finished`, `error`, `exit_code`, `run_dir`, and, when a journal exists, `nodes`,
`events`, and `summary`. `nodes` contains complete recorded outputs and their
artifact paths. Listing tasks returns `{ "tasks": [...], "errors": [...] }`;
unreadable task records appear in `errors` rather than disappearing silently.

`--wait` has an explicit 1–3600 second deadline and exits with the task's result:
0 completed, 2 blocked, 3 failed/interrupted/verification failed, 5 budget exhausted,
130 cancelled. A wait timeout exits 1 and leaves the independent task running.
Plain `--json` inspection exits 0 when the read succeeds, including failed tasks.
Neither status inspection nor waiting invokes a model.

## Stop, retry, and limits

```sh
gloop stop TASK_ID --json
gloop tasks TASK_ID --wait --timeout 30 --json
```

Stopping creates a local cancellation request. The worker passes cancellation to
the existing runtime, which terminates provider/command process groups and saves
the terminal result. A request to stop is not itself evidence of cancellation;
wait for `status: "cancelled"`. Stop is idempotent for finished tasks. A corrupted
journal does not prevent recording a stop request.

An OS file lock permits one independent task at a time per project. A competing
task is rejected, or records a failure if two launchers race before either worker
acquires the lock. This protects the shared working directory; parallel steps
within a deliberately authored graph still follow that graph's existing policies.
The lock is released by the OS even when a worker crashes. A missing worker is
reported as interrupted after a startup grace period instead of remaining active
forever. Worker liveness is checked using its OS lock, avoiding stale PID reuse.

No failed task retries automatically. Inspect the output and any file changes
before submitting a new request. `--id ID` makes retries of the *same submission*
idempotent: an identical request returns the existing task, while different content
with that ID fails. The browser uses a stable submission ID so a lost HTTP response
does not require blindly starting another worker.

Planning uses one invocation for the proposal, then up to one per reviewed step
only after execution is confirmed. The TUI and browser use one AI invocation for
a direct task and two for a second opinion.
CLI tasks default to 3; `--max-calls` accepts 1–32 and `--timeout` accepts 10–3600
seconds. Saved graphs retain their own stricter budgets. These are runtime bounds,
not promises about monetary cost or the number of internal turns a provider uses.

## Local data and server boundary

- `.gloop/jobs/ID/request.json`: immutable task and handoff metadata.
- `.gloop/jobs/ID/graph.yaml`: the exact graph snapshot submitted to the runtime.
- `.gloop/jobs/ID/process.json`, `started.json`, `completion.json`: worker lifecycle.
- `.gloop/jobs/ID/worker.log`: launcher/worker diagnostics.
- `.gloop/runs/ID/`: existing graph snapshot, journal, summary, and output artifacts.

The workspace reuses the editor's loopback HTTP server, per-launch token, origin
check, bounded authenticated request bodies, and atomic-write primitives. The
HTML is public only on loopback; every data or mutation API requires the token.
An unrelated website cannot submit tasks with its own origin. No API key is
embedded in the workspace. Managed `.gloop` paths reject symlink redirection.
