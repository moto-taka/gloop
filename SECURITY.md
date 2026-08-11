# Security Policy

## Reporting

Report suspected vulnerabilities or security issues privately (not in public PR descriptions).
Include:
- command and environment
- reproduction steps
- affected component (`gloop-core`, `gloop-cli`, `gloop-provider`, `gloop-runtime`)
- artifact/run data examples if applicable

## Threat model (implemented)

- Local, foreground execution model.
- Graphs are validated before use, with strict schema and unknown-field rejection.
- Graph validation rejects excessive parallelism, retry/loop counts, nesting depth, nested node count, and checked-workload overflow before execution.
- Credentials are not embedded in code; provider profiles resolve secrets from environment keys (`*_env`).
- Journal replay verifies schema version, sequence order, run id consistency, and state transitions.
- CLI returns explicit unsupported/bad-usage codes rather than silently ignoring invalid inputs.
- Artifact paths are validated per run and emitted as relative references under run root.
- Provider profile loading ignores `<project>/.gloop/profiles.toml` unless `--trust-project-profiles` is passed.
- Ambiguous provider outcomes (transport failure, timeout, process failure, invalid output, and 408/409/425/5xx) are not retried, even through profile rebinding. Only explicit rate-limit rejection and failures known to occur before invocation are retry candidates.
- Every runtime Git invocation disables repository hooks, external filesystem monitors, and external diff/textconv helpers, and Git runs in a killable process group. Worktree mode re-checks and rejects repository-local clean/smudge/process filters and diff/textconv drivers before status, checkout, staging, or inspection.

## Known limits

- Runtime executor and provider adapter paths are active; remaining risk is in policy compliance and secrets handling.
- Provider commands and graph execution require valid credentials, reachable endpoints, and explicit user trust for command profiles.
- Command profiles retain `HOME` to inherit normal CLI authentication. A configured harness therefore runs with the same-user read access of the gloop process; it is not a credential-isolated sandbox.
- Automatic redaction covers credentials explicitly mapped through provider configuration. Gloop cannot reliably redact unrelated secrets that a trusted executable reads from user files and chooses to print, so run artifacts must remain private.
- Provider version probes do not receive `env_from` credential mappings. A trusted command can still print unrelated values it obtains from its own files, so probe output should not be treated as secret storage.
- Model-call budgets and per-attempt output caps are enforced for provider prompts/outputs; run-level retained-output budget enforcement is implemented and accounts for stdout/stderr/raw-output/artifact bytes across attempts.
- Foreground execution does not force a global wall-time when `wall_time_seconds` is omitted; untrusted graphs should set explicit budgets.
- `readonly` is not a functional filesystem sandbox and is rejected unless sandbox enforcement exists.
- Same-user path replacement/TOCTOU around canonicalized paths is tracked as outside the baseline local-trusted threat model; retain command/output artifacts when reviewing suspicious runs.
- Journal chains and artifact references use unkeyed SHA-256 for corruption/partial-tamper detection, not authentication. A same-user attacker who can rewrite the complete run directory can recompute them; protect run-directory permissions when provenance is security-sensitive.
- HTTP profile `base_url` values intentionally support arbitrary HTTP(S) endpoints, including private-network services. Treat user/project profile files as trusted configuration to avoid SSRF-style access through a hostile endpoint definition.
- An explicitly supplied inspect/logs path may traverse operating-system or caller-owned ancestor symlinks before the protected run directory. Direct run/journal and standard `.gloop/runs` symlinks are rejected; broader same-user ancestor replacement remains part of the documented path/TOCTOU boundary.

## Safe operation recommendations

- Keep provider definitions in user/project config files with restricted filesystem permissions.
- Use least-privilege environment variables for API keys.
- Run only trusted command harnesses and inspect project profiles before enabling `--trust-project-profiles`.
- Review generated command vectors and arguments before enabling command-kind nodes.
- Keep shell command nodes constrained to trusted execution roots.
- Ignore run artifacts from untrusted sources; inspect and replay before reusing.
