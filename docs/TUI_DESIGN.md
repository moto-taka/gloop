# gloop Graph Agent CLI TUI

This document describes the resident terminal UX added around gloop's existing
Graph IR and deterministic runtime. The TUI is a presentation layer: it does
not become a planner, does not select a fixed set of roles, and does not add a
daemon or queue.

## 1. Current problems

Observed in the repository:

- gloop already has the important execution primitives: strict versioned Graph
  IR, data/control/resource/conditional/failure edges, fan-out/fan-in, bounded
  loops and retries, provider profiles, artifacts, journal replay, and a
  `ProgressEvent` channel.
- authoring in `wizard.rs` is a sequence of `dialoguer` prompts. It is useful
  for first-time creation but loses the user's spatial/contextual view of a
  graph between questions.
- `gui.html` is an n8n-style browser editor and is a separate interaction mode;
  it is not a resident task-and-run workspace.
- normal progress output is a stream of one-line events. It does not keep a
  stable node board with profile/model, current attempt, last error, and output
  selection visible at the same time.
- the CLI surface requires the user to know which subcommand to enter before
  choosing a graph, provider, model, or task.

The TUI therefore adds a stable shell around the existing primitives instead of
moving graph semantics into presentation code.

## 2. Recommended UX

`gloop graph` opens the resident session. `gloop graph tui` is an explicit alias.

1. **Overview** opens with the last session or a small selector row for graph
   template, profile, and model. Nothing is hard-coded as Planner/Writer/
   Reviewer.
2. Press `i` and enter the natural-language task. The task is shown as the
   single source of intent at the top of every screen.
3. Press `2` to inspect the Graph Builder. Pick a template, add or remove
   nodes, edit a node, and connect nodes without leaving the screen.
4. Press `v` to validate. Validation is the existing `Graph::validate`, shown
   as counts plus selectable issue details.
5. Press `s` to save the graph YAML, or `r` to run it immediately.
6. **Run Monitor** keeps a node board, event log, selected node detail, and final
   result in one stable layout. `ProgressEvent` is the only runtime-to-TUI
   stream; the TUI never infers scheduler state from terminal text.
7. When the run finishes, the selected node can show its normalized output or
   artifact path. The existing `inspect`, `logs`, and `replay` commands remain
   available for full-fidelity post-run inspection.

## 3. Screen mockups

### Overview / task intake

```text
┌─ gloop graph ──────────────────────────────────────────────────────────────┐
│ Overview                 Graph Builder                 Run Monitor           │
├──────────────────────────────┬─────────────────────────────────────────────┤
│ SELECTION                     │ GRAPH FLOW                                  │
│                              │                                             │
│ Task       summarize this…   │  01 request   [agent] claude / fable         │
│ Template   parallel-research │  02 research  [agent] qwen   / qwen3.8-max   │
│ Profile    claude             │  03 reduce    [reduce] claude / fable        │
│ Model      fable              │                                             │
│ Graph      3 nodes / 2 edges  │  ○ ready → ◐ running → ✓ succeeded           │
│ Save       .gloop/graphs/...  │                                             │
│                              │                                             │
│ i task  t template  p profile │                                             │
│ m model  2 builder  r run     │                                             │
└──────────────────────────────┴─────────────────────────────────────────────┘
 i task  t template  p profile  m model  r run  s save  q quit
```

### Builder

```text
┌─ Graph Builder ────────────────────────────────────────────────────────────┐
│ ▸ request      agent      claude / fable        retry 1   fan-out 1         │
│   research-a   agent      qwen   / qwen3.8-max  retry 2   fan-out 3         │
│   research-b   agent      qwen   / qwen3.8-max                              │
│   reduce       reduce     claude / fable                                    │
├──────────────────────────────┬─────────────────────────────────────────────┤
│ NODE LIST                     │ NODE EDITOR                                 │
│ j/k select                    │ NODE research-a                            │
│ a add agent                   │ kind: agent                                 │
│ e edit prompt                 │ profile: qwen                              │
│ x remove                      │ model: qwen3.8-max                          │
│ c connect → Enter             │ fan-out: 3                                  │
│                              │ retry: 2 attempts / 10s                     │
│                              │ PROMPT                                       │
│                              │ investigate the task from an independent…   │
│                              │ EDGES: request → research-a                 │
│                              │        research-a → reduce                  │
└──────────────────────────────┴─────────────────────────────────────────────┘
```

### Run monitor

```text
┌─ Run Monitor · running ────────────────────────────────────────────────────┐
│ ◐ request        Running       claude / fable                               │
│ ○ research-a     Ready         qwen / qwen3.8-max                           │
│ · research-b     Pending       qwen / qwen3.8-max                           │
│ · reduce         Pending       claude / fable                               │
├──────────────────────────────┬─────────────────────────────────────────────┤
│ EVENT STREAM                  │ RUNTIME / RESULTS                           │
│  12 request       running     │ graph: parallel-research                    │
│  13 research-a    ready       │ max_parallel: 3                             │
│  14 research-b    ready       │ selected node: request                      │
│  15 request       output      │ attempt: 1                                   │
│                              │ output: …                                    │
│ Ctrl-C cancels the run       │ artifacts: .gloop/runs/<id>/...              │
└──────────────────────────────┴─────────────────────────────────────────────┘
```

## 4. Keyboard model

| Key | Global behavior |
|---|---|
| `1`, `2`, `3` | Overview, Builder, Run Monitor |
| `Tab` | Next screen / next focus region in the expanded builder |
| `j/k` or arrows | Move the active list selection |
| `i` | Natural-language task input |
| `t` | Template selector |
| `p` | Harness/profile selector |
| `m` | Model selector; manual entry is always available |
| `v` | Validate with the existing Graph validator |
| `s` | Atomic save to the selected graph/preset target |
| `r` | Start a validated foreground run |
| `c`, then `Enter` | Create an edge from the selected node to the target |
| `a` / `x` | Add/remove a node in Builder |
| `e` | Edit the selected node's prompt or fields |
| `Esc` | Close an editor, cancel a connection, or close a palette |
| `Ctrl-C` | Cancel the active runtime run; quit when idle |
| `q` | Quit when idle |

Input fields support cursor movement, Home/End, backspace, delete, and blank
model input to restore provider-default model routing. Mouse is optional and
never required.

## 5. Graph Builder specification

### Template gallery

Templates are data, not roles. The initial gallery can include the existing
built-ins (`direct`, `plan-implement-verify`, `parallel-research-reduce`, and
`review-fix-loop`) plus saved project templates. A template is copied into an
editable draft; built-ins stay read-only.

### Visual list

The primary editing representation is a stable, keyboard-addressable list of
nodes grouped by topological order. Each row shows:

```text
selection · node id · kind · profile · model · fan-out · retry · status
```

This intentionally avoids a horizontally scrolling ASCII diagram as the only
representation. A compact edge summary and optional Mermaid/DOT preview can be
opened beside it. Terminal width must not change whether a graph is editable.

### Node editor

The editor has a small common section and kind-specific sections:

- common: id, label, workspace, resources, timeout, context files/limit,
  continue-on-failure, retry attempts/backoff/rebind profiles;
- agent: inline/file prompt, profile, arbitrary model id, fan-out, output
  contract;
- reduce/synthesize: prompt, profile, model, output contract;
- command/verify: argv, environment, output limit;
- gate: message and default decision;
- loop/subgraph: nested graph, bounded iteration, condition, and nesting
  warnings.

The editor uses the existing structs and then calls `Graph::validate`; it does
not maintain a second schema.

### Edges and conditions

`c` starts an edge, then the target is selected. The edge editor chooses one of
the existing kinds: `data`, `control`, `resource`, `conditional`, or `failure`.
For conditional/failure edges, a form edits terminal status, output substring,
JSON pointer, and JSON equality. Invalid cycles and invalid condition/status
combinations are rejected before the draft is mutated.

### Parallel, reduce, retry, and conditional flow

- parallel work is represented by the existing `fan_out` plus multiple ready
  outgoing branches; the graph policy's `max_parallel` remains the scheduler
  cap;
- fan-in is an explicit `reduce` or `synthesize` node with data edges from the
  branches;
- retry edits the existing bounded `RetryPolicy`, including ordered profile
  rebinding for agent-like nodes;
- conditional and failure edges remain explicit and visible rather than being
  hidden in a role convention;
- loops are nested bounded graphs with an explicit completion condition and
  stagnation guard. No TUI action creates an unbounded loop.

## 6. Harness / provider / model selector

The selector is hierarchical but stores only the graph's existing fields:

```text
Harness/profile       model                         capabilities
────────────────────────────────────────────────────────────────
claude (command)     fable                          ✓ text  ✓ tools
codex  (command)     gpt-5.3-codex-spark             ✓ text  ✓ native sandbox
pi     (command)     [discovered catalog]            ✓ text  ?
qwen   (command)     qwen3.8-max                    ✓ text  ?
custom (HTTP)        any user-provided model id      ✓ text
```

The rows above are illustrative labels, not defaults. At runtime the list is
built from `profiles.toml`, enabled/capability checks, and the provider's
discovery result. A selector row has one of four states: available, disabled,
discovery unsupported, or discovery failed (with a short reason). A manual
model entry is always offered because gloop deliberately accepts arbitrary
model ids and remote catalogs can change.

The local CLI verification for this design found:

- Claude Code `2.1.232`; its `--help` documents `--model <model>` and explicitly
  lists `fable` as a valid alias example. The TUI should pass the user-entered
  value, not invent a canonical model id.
- `bl text chat --help` documents `--model`, `--message`, `--system`,
  `--output`, and the default `qwen3.8-max`. `bl model list --output json`
  reported model id `qwen3.8-max` with display name `Qwen3.8-Max`.

These facts are useful for local presets, but they do not become hard-coded
Graph roles or mandatory providers.

## 7. Save format and presets

Keep the existing YAML Graph IR as the canonical executable document:

```text
<repo>/.gloop/graphs/<name>.yaml     saved graph drafts
<repo>/.gloop/templates/<name>.yaml  reusable project templates
<repo>/.gloop/runs/<run-id>/         artifacts, journal, summary, replay data
<user config>/gloop/profiles.toml    user harness/provider profiles
<repo>/.gloop/profiles.toml          trusted project profile overlay
```

Add a small optional preset layer only for UX convenience. A preset references
an existing graph/template and overlays task, profile, model, and run policy;
it must not duplicate the Graph IR or silently rewrite node roles. Presets are
written atomically and are versioned with a schema version so they can be
discarded without affecting executable graph YAML.

The TUI stores only view state (last screen, selected graph, focus, and recent
task history) separately from graph semantics. Secrets remain environment
references in profiles and never enter a preset, journal, or screen snapshot.

## 8. TUI/runtime boundary

```text
TUI: selection, draft editing, validation display, save intent, event display
  │
  ├─ Graph::validate / Graph::to_yaml
  ├─ ProfileStore / ProviderRegistry selection data
  └─ Runtime::run(RunOptions { progress: ProgressEvent channel, cancellation })
        │
        └─ scheduler, provider invocation, retry, artifacts, journal, summary
```

The TUI never implements dependency readiness, retries, conditional evaluation,
parallel limits, output contracts, or provider fallback. It observes the
runtime's typed progress events and renders the final `RunSummary`. The
existing non-interactive and JSON paths remain unchanged.

## 9. OpenTUI decision

OpenTUI is a strong UX reference and a plausible future implementation for a
Node/TypeScript front end: its official documentation describes a Zig native
core, TypeScript/React/Solid bindings, flexbox layout, keyboard/mouse
components, and a C ABI for other languages. Its native Node renderer requires
Node.js 26.4.0 with experimental FFI.

It is not the best first dependency for this repository. gloop is a Rust 2024
workspace with a Rust 1.88 floor, and a direct OpenTUI integration would add a
Zig build/runtime artifact, Node/FFI packaging, a second application runtime,
and a Rust-to-C ABI boundary before the Graph UX is proven.

The recommended order is:

1. native Rust `ratatui` + `crossterm` for the resident TUI and its vertical
   slice;
2. keep `tui` dependent on typed core/provider/runtime APIs so the presentation
   boundary is replaceable;
3. revisit OpenTUI if the product needs React/Solid component reuse or a
   separate high-fidelity terminal front end after a stable C ABI binding and
   distribution plan exist.

Alternatives considered:

| Option | Fit now | Reason |
|---|---:|---|
| OpenTUI | medium/low | excellent component model; cross-runtime build/FFI cost |
| ratatui + crossterm | high | native Rust, mature terminal backend, typed event loop |
| dialoguer only | medium | good prompts, poor resident multi-pane state |
| browser GUI only | medium | already present, but not terminal-first or SSH-friendly |

## 10. Independent review synthesis

The local review request was sent independently to Claude Code with the exact
help-documented `--model fable` argument and to `bl text chat` with the exact
 model id `qwen3.8-max`. Their recommendations are kept as separate inputs
until integration:

### Claude Fable

Fable inspected the repository and identified nine concrete UX gaps: default
human gates are auto-resolved by `DefaultHumanGate`, progress is a one-line
stream and drops attempt/data fields, live incomplete journals cannot be read by
the current commands, authoring is split across dialoguer/browser/YAML, the
wizard is deeply modal, command discovery is costly, model discovery is only
connected to the GUI path, capability errors appear late, and every command is
a cold start. It also called out the existing pure editor helpers in
`wizard.rs` as the most important reusable asset.

Its primary recommendation was `ratatui` + `crossterm`, with a stable Home / Run
Setup / Run / Results flow, a command palette (`Ctrl-K`), a real TUI human-gate
channel, and a typed full-event stream if attempt-level details are needed. It
recommended implementing the first vertical slice as resident shell + live
execution + human gate, then the builder, then provider discovery/presets. It
also warned about terminal restoration, TTY detection, gate time consuming the
wall budget, and event-channel retention limits.

### Qwen 3.8 Max

The exact local `bl text chat --model qwen3.8-max` invocation was attempted
twice. `bl auth status --output json` reported the `token-plan` profile as
authenticated, but the first model call failed before a response with
`UND_ERR_HEADERS_TIMEOUT: Headers Timeout Error` and the bounded retry ended
with `Request timed out`. Therefore there is no Qwen design proposal to
summarize or compare; inventing one would violate the independent-review
requirement. The only evidence-backed difference is that Fable produced a
repository-grounded review while the Qwen provider was unavailable at review
time.

### Integration

The integrated design keeps Fable's typed runtime boundary, reuse of the
wizard's pure editor functions, explicit human-gate/error states, and
three-pane run visibility. It narrows the launch surface to the requested
`gloop graph` / `gloop graph tui` instead of changing argument-less `gloop`
behavior, and starts with the smallest safe slice: template/profile/model/task
selection, node-list builder, typed progress, atomic save, and foreground run.
Neither review's role naming is treated as a runtime convention.

## 11. Integrated final design

The smallest complete user loop is:

```text
gloop graph
  → choose template/profile/model
  → i: enter task
  → 2: inspect/edit graph
  → v: validate
  → s: save or r: run
  → 3: monitor typed node events
  → inspect selected output / artifacts
```

The session is not a hidden LLM agent. Once the graph is saved, execution is
the existing foreground runtime and can proceed without a permanently running
parent model. Profiles, models, graph topology, and prompts are editable per
node. The TUI makes the choices easy without replacing them with conventions.

## 12. Implementation layout

Implemented vertical slice:

```text
crates/gloop-cli/src/cli.rs       `gloop graph` default and `graph tui` alias
crates/gloop-cli/src/tui.rs       resident shell, builder, run monitor, input
crates/gloop-cli/src/commands.rs shared profile/model graph binding helpers
Cargo.toml                        ratatui + crossterm workspace dependencies
crates/gloop-cli/Cargo.toml       CLI dependency wiring
```

Recommended follow-up split after UX validation:

```text
crates/gloop-cli/src/tui/
  app.rs          state machine and actions
  screens.rs      overview/builder/run widgets
  input.rs        text editing and keymap
  selector.rs     profile/model discovery view
  persistence.rs  preset/view-state save
```

The first slice intentionally stays in one module so the behavior can be
validated before introducing a large UI abstraction tree.

## 13. Prototype scope and verification

The prototype supports:

- `gloop graph` / `gloop graph tui` launch;
- template cycling, profile cycling, manual model input, natural-language task
  input;
- node list navigation, add/remove agent, inline prompt edit, data-edge
  creation with cycle/validation rejection;
- validation, atomic graph save, runtime start/cancel, typed event stream, node
  status board, human-gate approve/reject/default handling, and final
  summary/output display.

It deliberately leaves the full inspector field matrix, edge-kind condition
form, saved preset catalog, and model discovery cache for the next slice. Those
features should reuse the existing wizard/GUI helpers and core schema rather
than create a second graph format.
