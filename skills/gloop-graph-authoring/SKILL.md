---
name: gloop-graph-authoring
description: Use gloop to find, create, edit, validate, and run agent graphs for beginners.
---

# gloop graph authoring

Use this skill when a person asks to make a gloop workflow, change its nodes,
choose a harness or model, or run an existing graph.

## Safe beginner flow

Run these commands from the project directory. Do not invent a filename before
checking the list.

~~~text
1. gloop graph list
2. Pick one name or file from the output.
3. For a visual editor, run:
   gloop graph edit NAME --gui
4. In the browser, click a step and configure its name and processing details.
   To add one, choose AI processing, command execution, result verification,
   or an approval checkpoint on the left. To connect steps, click the right
   output of one card and then the left input of another. If unsure, keep the
   execution tool and model at their defaults, then press Save.
5. Validate the saved path shown by gloop:
   gloop graph validate PATH
6. Run it only after validation succeeds:
   gloop run --graph PATH --repo .
~~~

NAME may be a built-in template such as direct, plan-implement-verify,
council, decompose-fanout-reduce, or implement-test-loop, a saved project
template, or a graph YAML path.

Choosing a pattern:

- council — two blind designs -> integrated design -> implementation -> three
  reviewer panel -> reconciled verdict. Use for high-stakes design/review.
- decompose-fanout-reduce — one model decomposes the task into up to 4
  packages, lightweight worker lanes run them in parallel, one integrator
  assembles the deliverable. Use for wide parallelizable work.
- implement-test-loop — implement, then a bounded loop runs the test command;
  on failure a fixer consumes the failure details and the loop retries until
  the command passes, hits the loop cap, or stagnates. Replace the
  placeholder test command with the real suite (e.g. pnpm run test) before
  running.

If the person wants a reusable project template, use the visual initializer:

~~~text
gloop graph init --gui
~~~

Or start from a known template:

~~~text
gloop graph init --name my-flow --from plan-implement-verify --gui
~~~

For a saved project template, update is the friendly alias:

~~~text
gloop graph update my-flow --gui
~~~

## When there is no browser

Use the selective terminal editor. It asks before changing nodes, edges, or
graph settings:

~~~text
gloop graph edit NAME
~~~

Choose Save and finish. If a menu or validation error is unclear, do not guess:
show the exact error and run gloop graph list again.

## English and Japanese

All commands pick their display language from the system locale (`GLOOP_LANG`,
`LC_ALL`, `LC_MESSAGES`, or `LANG`; `ja*` selects Japanese). Force a language
with `--lang`:

~~~text
gloop graph edit NAME --gui --lang en
gloop graph edit NAME --gui --lang ja
gloop graph tui --lang ja
~~~

Inside the TUI, press `l` to switch English/日本語 live.

The browser editor looks like a simple flow board:

- left: add "AI processing", "Command execution", "Result verification", or
  "Approval checkpoint";
- middle: see the whole flow, drag cards to arrange them, and connect dots;
- right: set the selected step's display name, instructions, execution tool,
  and model. Choose automatic to keep the runtime default, or select a named
  tool before choosing one of its models. Model choices come from the selected
  execution tool's defaults and discovered CLI list (when supported). Existing
  custom model IDs remain editable under Technical settings.
  If listing fails or the tool has no list command, use Technical settings →
  Custom model ID.

The flow name and goal are visible on the right when no card is selected. Node
ids, edge kinds, fan-out, and other technical fields are under "Technical settings"
/ "技術設定". Do not open those settings unless the person
specifically asks for them.

For command execution and result verification, enter the command exactly as it
would be written in a terminal, for example `cargo test --workspace`. The GUI
splits the executable and arguments safely when saving.

## Orchestrating runs from an agent (supervisor pattern)

A cheap model can build and start a graph, and a supervisor (human or another
agent) can poll progress and collect intermediate + final results without
holding the run open:

~~~text
1. Start the run with a stable id you choose, in the background:
   gloop run --graph PATH --repo . --run-id my-task &
2. Poll live status (safe while the run is in flight):
   gloop status my-task --json
3. Or block until the run finishes and get the run's exit code:
   gloop status my-task --wait --json
4. For full post-run inspection:
   gloop inspect .gloop/runs/my-task
~~~

`gloop status --json` fields that matter to a supervisor:

- `run.phase`: `initializing`, `running`, or `finished`.
- `run.nodes[]`: per-node `status`, `attempts`, intermediate `output`, `error`.
- `run.events_tail`: the most recent journal events.
- `run.last_event_age_ms`: how stale the last event is; use it to detect a
  crashed or stuck run (the journal alone cannot distinguish them).
- `run.summary`: present once the run finishes; contains the final results.

Querying status always exits 0 when the query itself succeeds; use
`--wait` when the exit code must reflect the run outcome (`0` success,
`2` blocked/human gate, `3` verification/execution failure, `5` budget
exhausted, `130` cancelled). With no run id, `gloop status` reports the
newest run under `.gloop/runs/`.

The resident TUI (`gloop graph`) is the human-friendly equivalent: it shows
template/profile pickers with previews, validates with a visible issue list,
auto-saves before running, and displays each node's output (`o`) while and
after a run.

## Important rules for an assistant

- Never run a graph before gloop graph validate PATH succeeds.
- Never overwrite a graph because a name looks similar. Use the exact path from
  gloop graph list.
- Built-in templates are not edited in place. The first save creates a copy
  under .gloop/graphs/.
- If the GUI reports that the file changed while it was open, reload it and
  save again; do not force the old copy over the new one.
- An execution tool is the local CLI or provider profile that runs an AI step.
  A model is the model identifier passed to that tool. If unsure, keep both at
  their defaults.
- If a command fails, preserve the complete error, fix only the stated issue,
  and retry once. Do not silently fall back to another harness or model.
