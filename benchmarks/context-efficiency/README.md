# Public exercise context-efficiency pilot

This experiment compares a retained Codex conversation with a real gloop graph
that gives each model stage a fresh conversation and selected artifacts.
It uses six public Python exercises from
[Aider's Polyglot benchmark](https://aider.chat/2024/12/21/polyglot.html).
This is a custom workflow adaptation, **not an official Aider leaderboard score**.

The [2026-09-08 report](REPORT.md) records 8.9% fewer input tokens with gloop,
33.3% higher calculated API-equivalent cost, and 3/6 final task passes versus
4/6 for the retained conversation. These six single-run pairs do not establish
quality or cost superiority. Numerical evidence is in [results.json](results.json)
and [calls.csv](calls.csv).

The fixed sample, provider, prompts, limits, and exclusions are recorded in
[protocol.json](protocol.json). The tasks were sampled before generating any
solutions. Each arm runs design, implementation, public tests, review or one
repair, final public tests, and a local PR draft. No PR is submitted to GitHub.

The retained arm resumes the same session and sends only new instructions or
test results after the first stage. The gloop arm passes the task brief and
direct predecessor artifacts to fresh sessions. Its PR stage receives a short
exercise title, the review artifact, and final test results. It does not receive
the whole earlier conversation or, following approval, the whole solution again.
These choices are deliberate context selection; this is not a claim that the
runtime automatically selects the best context for arbitrary workflows.

## Reproduce

Requirements: Python 3.10+, Docker, an authenticated Codex CLI, and gloop.
The measured run used Codex CLI 0.153.4 and a gloop 0.8.1 binary; its SHA-256 is
recorded in `results.json`. The model alias was `gpt-5.4-mini`, effort `low`.
Model responses are nondeterministic, and backend snapshots/caches can change.
The commands below consume the signed-in account's model allowance.

Clone the public dataset to a fresh directory:

```sh
git clone https://github.com/Aider-AI/polyglot-benchmark.git /tmp/gloop-polyglot-benchmark
git -C /tmp/gloop-polyglot-benchmark checkout 7e0611e77b54e2dea774cdc0aa00cf9f7ed6144f
```

From the gloop repository, use a new run directory for every experiment:

```sh
docker pull python@sha256:78387bc3881b8273120a12ebe6c1ab22b018ccc2c9adf565ae1ac9b536e184ea
cargo build -p gloop-cli
python3 benchmarks/context-efficiency/benchmark.py prepare \
  --root benchmarks/context-efficiency/work/my-run \
  --dataset /tmp/gloop-polyglot-benchmark \
  --gloop target/debug/gloop
python3 benchmarks/context-efficiency/benchmark.py run \
  --root benchmarks/context-efficiency/work/my-run
python3 benchmarks/context-efficiency/analyze.py \
  --root benchmarks/context-efficiency/work/my-run \
  --output benchmarks/context-efficiency/work/my-run/report
```

Preparation copies the gloop binary so another build cannot change the measured
executable. It verifies the pinned dataset, deterministic sample, and all public
tests using upstream reference implementations in separate folders. Reference
solutions and full test sources are never sent to the model. Generated code runs
in a network-disabled, read-only, resource-limited Docker container. Original
test hashes are checked after each workflow and again during analysis.

The run uses at most 48 model calls and one repair per task. Calls time out after
120 seconds and tests after 30 seconds. A run has a 40-minute deadline from
preparation and stops after three consecutive workflow errors. No failed model
call is silently retried. A failing exercise remains a benchmark failure even
when gloop successfully completes the workflow and produces a PR draft.

## Measurements and evidence

- Input tokens come from each Codex `turn.completed.usage.input_tokens`. This
  field already includes cached input; cached tokens are not added a second time.
- Output tokens include reasoning tokens. Reasoning is also recorded separately
  and is not added twice.
- API-equivalent cost is calculated from uncached input, cached input, and output
  at $0.75, $0.075, and $4.50 per million tokens, respectively, using the
  [official model pricing](https://developers.openai.com/api/docs/models/gpt-5.4-mini)
  checked on 2026-09-08. It is **not actual ChatGPT subscription billing**.
- Task wall time includes CLI startup, model calls, Docker tests, and gloop
  orchestration where applicable. The runs are sequential; this is not a
  parallel-execution speed benchmark.
- Raw prompts, JSONL events, stage artifacts, test logs, outcomes, and gloop run
  summaries remain under the ignored `work/` directory. `results.json` preserves
  per-call usage, evidence hashes, session IDs, task outcomes, and gloop run IDs.
  `calls.csv` provides the per-call numerical data.

The analyzer verifies session reuse/isolation, zero observed model tool events,
unmodified public tests, usage arithmetic, and six successfully executed nodes
with one attempt per node for each completed gloop run. Configuration warnings
are preserved: `skip_host_skill_discovery` is an under-development CLI feature.

The first attempt used Claude Haiku but received HTTP 429 before generating any
exercise solution. After three failures the circuit breaker stopped it. Its
frozen settings are retained in
[aborted-claude-protocol.json](aborted-claude-protocol.json); those unavailable
calls are not scored as model quality failures or zero-token successes.

Six exercises with one run per arm can reveal workflow behavior but cannot
establish statistical superiority, generalize to large repositories, or settle
a comparison with native subagents. Public exercises may also be present in
model training data. We measure the quality of the generated Python code using
the upstream tests; PR prose has no standardized quality score here.

CLI JSONL and resume behavior follow the
[official non-interactive mode documentation](https://learn.chatgpt.com/docs/non-interactive-mode).
Dataset contents remain in the upstream checkout; see its original license and
Exercism attribution. This directory does not vendor reference implementations.
