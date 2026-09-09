# Context-efficiency improvement work — 2026-09-09

The three performance targets are **not yet established together**. The
implemented changes reduce repeated input and preserve verification evidence.
A development run improved task passes, but the independent fork experiment did
not improve pass count and did not provide dependable cache reuse. Pi/GPT-5.5 now reuses a shared prefix in a live gloop documentation graph.
That is not yet a matched coding quality/token comparison. Explicit Responses
API prefix caching is also implemented and covered by local HTTP tests; that
separate route still needs an API-key connection for live measurement.

## Paired measurements

All arrows below mean **retained conversation → gloop**, within the same row.
Both arms used `gpt-5.6-luna`, low effort, Codex CLI 0.153.4, the same public
exercise inputs, unchanged upstream tests, four model calls and at most one
repair. Do not compare the two rows as an isolated before/after change: they use
different tasks and different context strategies.

| Sample / gloop strategy | Total input tokens | Cached / total input | Final task passes | API list-price equivalent |
| --- | --- | --- | --- | --- |
| Development: fresh artifact handoff | 284,911 → 255,379 (-10.4%) | 64.2% → 18.5% | 3/6 → 5/6 | $0.036595 → $0.054187 |
| Holdout: experimental design-session forks | 267,813 → 260,374 (-2.8%) | 70.8% → 46.0% | 4/6 → 4/6 | $0.027197 → $0.039677 |

These are six single-run pairs per sample, not an official Aider score or a
statistical superiority claim. Costs use the published Luna rates, including
cached input, and are **not actual ChatGPT subscription charges**. Input totals
already include cached input. Reasoning tokens are included in output totals.

- [Development protocol](protocol.json), [results](development-results/results.json), [calls](development-results/calls.csv).
- [Preselected holdout](holdout-selection.json), [fork protocol](fork-holdout/protocol.json), [results](fork-holdout/results/results.json), [calls](fork-holdout/results/calls.csv).
- [Runtime receipt audit](receipt-audit.json): all 48 gloop node usage receipts in these two runs match the underlying CLI call metrics. The analyzers also check test hashes, model-tool absence, session behavior and workflow outcomes.

The original `gpt-5.4-mini` pilot remains unchanged in the parent directory.
That model became unavailable to this ChatGPT account before the development
rerun generated any solution. Both comparison arms were therefore rerun with
Luna; the older model's results are not used as a matched baseline.

## Current product changes

1. Render shared context files first, then compact dependency JSON and the
   current task. Keep complete strings, fields and failure evidence; enforce
   the byte bound including headers.
2. Send an output JSON Schema before generation, then validate the response
   afterward. Record provider token receipts before runtime output validation.
3. Keep complete JSON in task handoffs. When output bodies exceed the handoff
   bound, omit only bodies that have durable artifact references, retaining
   step status, errors and artifact paths. Reject oversized essential metadata
   instead of silently clipping final verification results.
4. Record total input, cached input, cache writes and reasoning separately.
   OpenAI input already includes cached input; Anthropic's separate cache
   fields are added to its ordinary-input field.
5. Add opt-in OpenAI Responses support with `cache_prefix = true`. Place the
   explicit breakpoint after the exact shared file prefix and keep changing
   dependency/task/schema text outside it. Preserve user-role content. Use a
   workspace/prefix-derived routing key, and default Responses storage to false.

6. Add optional command `cache_key_args` to pass the shared-prefix routing key
   to Pi while `--no-session` keeps each node's conversation empty. Correct Pi
   usage accounting and extract completed assistant text after thinking blocks;
   reject provider errors and prevent user prompt echoes from becoming answers.

The command-based fork prototype was removed from the product after evaluation.
Its [archived patch](fork-holdout/experimental-fork.patch.gz) records that experiment;
it is not a supported current graph feature. Each measured run retains its
frozen binary, SHA-256, raw provider events and test logs under the ignored
`../work/` directory.

## Verification

The isolated v0.9.0 release tree passed `cargo test --workspace -- --test-threads=1`:
553 tests across 24 test-result groups, no failures or ignored tests.
[Release verification](release-verification.json).

The earlier development workspace passed 559 tests and the example graphs passed
CLI validation and dry runs. These local checks verify behavior and configuration;
live Pi cache evidence is recorded separately below. The live Responses API route
remains untested.
[Development verification](verification.json), [OpenAI example checks](example-validation.json),
[cache probe receipts](cache-probes.json), [Pi example checks](pi-example-validation.json).

## Cache routing experiments and remaining limits

A two-call Codex fork probe showed 75–85% cached input on one development
example, but that was not dependable in the complete holdout run. A session
fork alone does not establish cache reuse. The current Codex source derives
cache identity from session context; normal independent forks do not provide a
caller-controlled cache namespace.

A separate Pi probe could set a stable routing key using its supported CLI
options. The three short Luna requests reported zero cache reads. Adding the
Responses explicit-cache parameters through Pi's documented payload hook was
rejected by the ChatGPT endpoint with `Unsupported parameter:
prompt_cache_options` on all three bounded attempts; that route was stopped.
Those rejected calls' zero-filled usage objects are **not** successful inference
or zero-cost measurements. A short GPT-5.5 routing-key probe also reported zero
hits, but its roughly 1.4k-token prefix was below that model's 2,048-token
breakpoint interval, so it does not establish failure of larger-prefix caching. The subsequent
real-document probe below resolved that uncertainty for GPT-5.5.

A subsequent three-call GPT-5.5 probe used the actual gloop schema document,
without padding. The first call reported 0 cached tokens; a fresh conversation
with the same routing key reported 1,792 cached out of 2,242 input tokens
(79.9%); a third fresh conversation with a different key reported 0. This is
small-sample evidence for a usable routing path, not a cache guarantee.

The new command adapter was then exercised through the real gloop runtime:

| Two-node documentation graph | First node cached / input | Second node cached / input | Node success |
| --- | --- | --- | --- |
| Pi / GPT-5.5 | 0 / 2,635 | 1,792 / 2,727 (65.7%) | 2/2 |
| Pi / GPT-5.6 Luna | 0 / 2,636 | 0 / 2,703 | 2/2 |

All four `node_usage` receipts match their underlying Pi JSON events, including
ordinary input plus cache reads/writes. Both graphs used fresh, ephemeral Pi
conversations and the same shared files within each workspace. These are
schema-document questions, **not coding pass-rate measurements**. A matched
coding comparison on the Pi/GPT-5.5 route remains unrun. The earlier Luna
coding results cannot be used as its baseline.

[Pi evidence and raw hashes](pi-cache-results.json),
[example profile](../../../examples/pi-cache-profiles.toml),
[example graph](../../../examples/pi-cache-prefix.yaml).
The first gloop attempt hit the raw event output bound because Pi echoes the
input document; its provider usage is unknown. The successful example allows
262,144 bytes for that event stream. No output was silently truncated.

No configured OpenAI API credential was found in the checked environment or Pi
provider readiness check. No credential value was printed. The new Responses
adapter has therefore been verified against a local HTTP server, not the live
OpenAI API. It rejects incomplete Responses output and invalid UTF-8 prefix
boundaries, and tests the exact request content and cache-token arithmetic.

For a live check, merge [the example profile](../../../examples/openai-cache-profiles.toml)
into a project profile file, provide `OPENAI_API_KEY` outside the repository,
and run [the two-node example](../../../examples/openai-cache-prefix.yaml).
Check actual `node_usage` receipts and include cache-write cost before claiming
an improvement. This example verifies the API route; it is not a substitute for
a subsequent matched coding benchmark and independent quality evaluation.

## Sources

- [OpenAI prompt caching](https://developers.openai.com/api/docs/guides/prompt-caching), checked 2026-09-09: explicit breakpoints, minimum lengths, key routing, cache writes and usage fields.
- [GPT-5.6 Luna pricing](https://developers.openai.com/api/docs/models/gpt-5.6-luna), checked 2026-09-09.
- [Codex session initialization](https://github.com/openai/codex/blob/main/codex-rs/core/src/session/session.rs) and [cache key selection](https://github.com/openai/codex/blob/main/codex-rs/core/src/client.rs), inspected 2026-09-09; these mutable source links explain the investigation, while local CLI receipts establish measured behavior.
