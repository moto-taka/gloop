# gloop

English | [日本語](README.ja.md)

Run your AI tools together from one terminal workspace. Build a workflow, run independent steps in parallel, and pass their results to the next step.

- Assign a different tool, model, and instruction to each step.
- Create workflows yourself or ask an AI to propose the steps for review.
- Watch progress live, or start background tasks and return to their results later.
- Keep outputs and execution history in your project for inspection and replay.

## See it in motion

**Run a workflow in the terminal.** Three local checks run in parallel before a final step combines their results.

[![A gloop workflow running in the terminal](assets/videos/gloop-terminal.gif)](https://github.com/moto-taka/gloop/releases/download/v0.8.1/gloop-terminal.mp4)

**Independent steps, combined results.** Split work across tools and bring their outputs together.

[![Three independent steps feeding one combined result](assets/videos/gloop-parallel.gif)](https://github.com/moto-taka/gloop/releases/download/v0.8.1/gloop-parallel.mp4)

## Install

With [Homebrew](https://brew.sh/) on macOS or Linux:

```bash
brew install moto-taka/tap/gloop
```

Or install from GitHub with Cargo:

```bash
cargo install --git https://github.com/moto-taka/gloop --locked gloop-cli
```

## Quickstart

Open gloop in your project:

```bash
cd /path/to/project
gloop
```

The display language follows your system locale. Use `gloop --lang en` for English or `gloop --lang ja` for Japanese. Saved graphs appear first: **Enter** opens one, and **r** runs it.

To build a workflow, choose **+ Graph · Manual**:

1. Press **a**, type an instruction, and press **Enter** to add an AI step.
2. Use **p** to choose its AI tool and **m** to choose its model.
3. Add more steps. **A** adds a branch; **c** connects existing steps.
4. Press **r** to run the workflow and **s** to save it.

**Enter** edits a step, **O** opens a saved graph, and **Tab** opens detailed settings. **q** stops an active Manual run or returns home when idle. Editing a graph makes no AI calls.

Choose **Auto** to have an AI propose a workflow, then review it before running. Choose **1 AI** for a single request. For a browser interface, run `gloop ui`.

## Background tasks

Start a task and check its progress later:

```bash
gloop start "Review the changes" --profile codex
gloop tasks
gloop tasks TASK_ID
gloop stop TASK_ID
```

Auto and single-AI tasks continue after closing the workspace. Saved workflows can also run through **Background run**. Keep your machine running; these tasks execute locally. Use Manual for workflows that require interactive approval.

See the [workspace guide](docs/TASKS.md) for keyboard controls, task handoff, and execution limits.

## Supported tools

Built-in profiles support **Codex, Claude Code, Qwen, Cursor Agent, Pi, and OpenCode**. Sign in to the tools you want to use, then check their availability:

```bash
gloop provider list --json
gloop provider doctor --json
```

Custom profiles can use other command-line tools, OpenAI-compatible APIs (including OpenRouter), and Anthropic-compatible APIs. Each step uses the credentials and usage allowance of its selected provider. Scheduling and result handoff do not require an LLM.

Project-local profiles require `--trust-project-profiles`. Configure only tools you trust: gloop does not add a general filesystem sandbox. See [provider configuration](docs/ADVANCED.md#provider-profiles).

## Commands

| Command | Purpose |
| --- | --- |
| `gloop` | Open the terminal workspace. |
| `gloop run --graph FILE` | Run a saved workflow in this terminal. |
| `gloop start "TASK" --profile TOOL` | Start a background task. |
| `gloop tasks [ID]` | List tasks or read a result. |
| `gloop stop ID` | Stop a background task. |
| `gloop graph --help` | Create, edit, and validate workflows. |
| `gloop provider --help` | Configure AI tools and check availability. |
| `gloop ui` | Open the browser workspace. |
| `gloop debug --help` | Inspect run status and execution records. |

Use `--repo PATH` to work on another project. Commands that return results support `--json` for scripts.

## Documentation

- [Workspace and background tasks](docs/TASKS.md)
- [Command reference](docs/CLI.md)
- [Advanced configuration, execution limits, and known limitations](docs/ADVANCED.md)
- [Workflow examples](examples)
- [Graph schema](docs/SCHEMA.md)
- [Architecture](docs/ARCHITECTURE.md)

[Contributing](CONTRIBUTING.md) · [Security](SECURITY.md) · [License](LICENSE) · [Attribution](THIRD_PARTY_NOTICES.md)
