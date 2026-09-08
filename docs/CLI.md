# Command reference

For everyday use, run `gloop`. Add `--lang ja` for Japanese or `--repo PATH`
to select a project. The workspace opens without a subcommand.

| Command | Purpose |
| --- | --- |
| `gloop` | Open the terminal workspace. |
| `gloop run` | Run a workflow or AI task in the foreground. |
| `gloop start` | Start a task that continues in the background. |
| `gloop tasks [ID]` | List background tasks or read a result. |
| `gloop stop ID` | Cancel a background task. |
| `gloop graph` | Open the advanced editor; subcommands manage graph files. |
| `gloop provider` | Configure AI tools and check availability. |
| `gloop ui` | Open the browser workspace. |
| `gloop debug` | Inspect run status, output, and execution records. |

`--repo`, `--lang`, and `--trust-project-profiles` work before or after a
subcommand. Project selection controls runs, tasks, graph/template discovery, and
provider configuration. Explicit file operands are relative to the current working
directory; `--repo` does not change that directory. With no PATH operand, debug
inspect/logs/replay use the project directory.

Run `gloop COMMAND --help` for that command's options. Display language applies
to localized views and output; parser help and some CLI messages remain English.

## Compatibility

Existing scripts continue to accept the old spellings below, but new instructions
and the command help use only the canonical form. JSON output and exit codes are
shared with the canonical implementation.

| Older spelling | Use now |
| --- | --- |
| `gloop tui --lang ja` | `gloop --lang ja` |
| `gloop graph tui --lang ja` | `gloop graph --lang ja` |
| `gloop status ID` | `gloop debug status ID` |
| `gloop inspect PATH` | `gloop debug inspect PATH` |
| `gloop logs PATH` | `gloop debug logs PATH` |
| `gloop replay PATH` | `gloop debug replay PATH` |
| `gloop graph update NAME` | `gloop graph edit NAME` |
| `gloop graph init --list` | `gloop graph list` |

`graph update` still accepts only saved project templates; `graph edit` also
accepts graph files and built-ins. `graph init --list` retains its short template
list; `graph list` includes graph files and validation status as well.

### Building reusable workflows

Both Manual and Auto can save reusable workflows. Manual also opens your existing
YAML files for editing and execution. `gloop graph` retains the shortcut-driven
editor; the default TUI's Manual mode uses a compact graph canvas.

Create a graph from a template, validate it, and inspect its shape:

```bash
gloop graph new workflow.yaml \
  --name direct \
  --goal "Summarize the latest changes" \
  --template direct \
  --request "Summarize the latest changes"

gloop graph validate workflow.yaml
gloop graph explain workflow.yaml
gloop graph render workflow.yaml --format mermaid
```

Try a blind parallel review example:

```bash
gloop graph validate examples/multi-provider-review.yaml
gloop run --graph examples/multi-provider-review.yaml --repo .
```

See [examples/multi-provider-review.yaml](../examples/multi-provider-review.yaml), with context in
[examples/review-input.md](../examples/review-input.md). The review graph uses built-in profiles (`claude`, `opencode`, and `qwen`) and requires whichever provider CLIs/auth are installed for your setup.

Run a two-designer wall-bounce: `claude` (model `fable`) and `codex` (model
`gpt-5.6-sol`) produce blind independent designs, critique each other's
proposal, revise in light of the critique, and a final node integrates both.

```bash
gloop graph new design.yaml --template design-wall-bounce --request "Design the sync engine"
gloop run --graph design.yaml --repo .
```

### Orchestration patterns

Three templates cover the common multi-agent shapes (see `examples/`):

```bash
# Council: two blind designs -> one integrated design -> implementation ->
# three reviewer panel -> reconciled verdict (like a /council flow).
gloop graph new council.yaml --template council --request "Design the rate limiter"

# Decompose: one model splits the task into up to 4 packages, lightweight
# worker lanes execute them in parallel, an integrator assembles the result.
gloop graph new decompose.yaml --template decompose-fanout-reduce --request "Refactor the module"

# Implement-test-loop: implement, then a bounded loop runs your test command;
# on failure a fixer consumes the failure details and the loop retries until
# the command passes (edit the placeholder test command first).
gloop graph new work.yaml --template implement-test-loop --loop-cap 3 \
  --request "Implement the feature and keep the test suite green"
```

The test-fix loop is a `Loop` node: each iteration runs `test`; a failure
edge routes the failure details to `fix`, and the loop repeats until `test`
succeeds, hits `--loop-cap`, or stagnates. This is how gloop combines graphs
and loops to push toward the goal.

The template is also selectable in the TUI (`t` opens a template picker with
previews) and in
`gloop graph new --interactive`.

Create a graph interactively, in the style of TAKT's authoring flow:

```bash
gloop graph new workflow.yaml --interactive
```

Start the resident Graph Agent TUI when you want to choose the graph, harness,
profile, model, and task from one keyboard-first workspace:

```bash
gloop graph
# select the display language:
gloop graph --lang ja
```

The TUI keeps the existing Graph IR and foreground runtime. Use `1/2/3` for
Overview / Graph Builder / Run Monitor, `i` for the natural-language task
(multi-line: `Enter` saves, `Alt+Enter` (or `Shift+Enter` where the terminal
reports it) inserts a newline, `Esc` cancels),
`t/p/m` to pick template/profile/model from preview pickers, `v` to validate
(the issue list opens automatically), `s` to save, and `r` to run (auto-saves
first). In Graph Builder, `a` inserts an AI node after the selected node and
connects it automatically. Press `K` to switch that node between `agent`,
`reduce`, and `synthesize`; the detail panel explains the selected purpose.
Each row shows its outgoing connections. Use `c`, move to a target, and press
`Enter` to add another connection. During a run, `o` opens the selected node's
output, and `?` opens help. `q` or `Ctrl-C` cancels an active run; `q` exits
when idle.

Model catalogs are read from each configured harness in the background. If
`m` is pressed while discovery is still running, the picker waits and opens
automatically when the choices arrive. A failed or unsupported catalog shows
the reason and offers retry (`r`) or explicit manual entry (`e`) instead of
dropping directly into a blank text field. Long catalogs support
`PageUp`/`PageDown` and `Home`/`End`.

The interface language follows your system locale (`GLOOP_LANG`, `LC_ALL`,
`LC_MESSAGES`, or `LANG`; Japanese and English are supported) and can be
switched live with `l` or forced with `--lang`.
See [TUI_DESIGN.md](TUI_DESIGN.md) for the screen model.

### Watching runs from scripts and agents

`gloop debug status` reads one run's journal live, so humans and AI agents can poll
progress and intermediate outputs while the run is still in flight:

```bash
# start a run in the background with a stable id
gloop run --graph workflow.yaml --repo . --run-id my-task &

# poll it from another terminal, script, or supervisor agent
gloop debug status my-task --json
gloop debug status --json            # newest run
gloop debug status my-task --wait    # block until finished, exit with the run's code
```

The JSON payload reports `phase` (`initializing` / `running` / `finished`),
per-node status, attempts, intermediate outputs, the recent event tail, and
the merged final `summary` once the run completes. `--json` always exits 0
when the query succeeds; `--wait` exits with the run's own status code
(`0` success, `2` blocked/human gate, `3` verification/execution failure,
`5` budget exhausted, `130` cancelled).

Save a reusable project template interactively (the wizard builds graphs node-by-node and selects providers from your configured profiles) or non-interactively from a built-in base:

```bash
gloop graph init
gloop graph init --name my-review-flow --from review-fix-loop \
  --description "Bounded review and fix loop" \
  --request "Review the latest diff"

gloop graph list
gloop graph new workflow.yaml --template my-review-flow
```

See everything available in the current project:

~~~text
gloop graph list
gloop graph list --lang ja
~~~

'graph list' shows built-in templates, saved project templates and graph YAML files, including node/edge counts and
validation status. Start editing by copying a name from the list:

~~~text
gloop graph edit plan-implement-verify --gui
~~~

Built-in templates are read-only until you save them. The first save creates
'.gloop/graphs/plan-implement-verify.yaml'; later edits open that saved graph.
For a named reusable project template, use:

~~~text
gloop graph init --name my-flow --from plan-implement-verify --gui --lang ja
gloop graph edit my-flow --gui
~~~

Open the graph in a local browser with an n8n-style visual editor. The screen
has three areas: choose a processing type on the left, see the whole workflow
in the middle, and configure the selected step on the right. Available types
are AI processing, local command execution, result verification, and a human
approval checkpoint. Each new step starts with a descriptive name, empty saved
input, and a realistic placeholder example instead of a hidden no-op value.
Technical fields stay behind "Technical settings". The editor only binds to loopback, shows enabled
execution tools with per-tool model choices (discovered at launch from each CLI's supported list
command when available), and writes through gloop's normal
validation and atomic-save path:

```bash
gloop graph init --gui --lang en
gloop graph edit workflow.yaml --gui --lang ja
gloop graph edit my-review-flow --repo /path/to/project --gui
```

Without `--gui`, `graph edit` uses a terminal wizard for selective node and
edge edits. It accepts a graph YAML path or any name shown by `gloop graph list`.

If you do not know what to type, use this order:

1. gloop graph list
2. Pick one name or file from the output.
3. For a visual editor, run: gloop graph edit NAME --gui
4. Click a step, choose what it should do, leave the AI/app and model at their
   defaults if unsure, and press Save.
5. Run gloop graph validate PATH and then gloop run --graph PATH --repo .

The bundled skill at skills/gloop-graph-authoring/SKILL.md contains the same
flow in short, copyable instructions for an agent or assistant that does not
know gloop yet.

Run a saved graph in the foreground:

```bash
gloop run --graph workflow.yaml --repo /path/to/project
```

For a small task, create and run a one-node graph directly. `--profile` selects the harness/provider and `--model` remains an arbitrary model id or alias:

```bash
gloop run "Fix the failing authentication test" \
  --profile codex \
  --model gpt-5.3-codex-spark \
  --non-interactive \
  --repo /path/to/project
```

Use `--dry-run` to validate and print the generated graph without invoking a provider. With `--json`, stdout contains exactly one final JSON value; progress is written to stderr only in human-readable mode.

Run artifacts are stored under `<repo>/.gloop/runs/<run-id>`. They can be inspected without rerunning any model:

```bash
gloop debug inspect .gloop/runs/<run-id> --json
gloop debug logs .gloop/runs/<run-id> --json
gloop debug replay .gloop/runs/<run-id> --json
```
