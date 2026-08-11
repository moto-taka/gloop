# Contributing to gloop

## Scope of contributions

- Keep changes limited to documented v0.1 areas: CLI behavior, graph IR, provider config types, artifacts, and replay/documentation.
- Do not add broad scope changes not tied to user-visible goals.

## Workflow

1. Keep edits focused and documented.
2. Prefer type-safe refactors over ad-hoc parsing.
3. Preserve deterministic graph behavior and strict validation.
4. Add/update tests when changing behavior in core parsing/validation paths.
5. Update docs in the same change when CLI behavior or schema changes.

## Repository layout

- `crates/gloop-core`: graph types and validation
- `crates/gloop-cli`: CLI commands and user-facing flows
- `crates/gloop-provider`: provider profiles and adapter types
- `crates/gloop-runtime`: artifacts, journals, and replay support
- `examples/*.yaml`: reference graphs
- `docs/*`: documentation

## PR checklist

- Validate requested features against README's execution model and known limitations.
- If changing graph schema, update:
  - `docs/SCHEMA.md`
  - example graphs as needed
  - schema compatibility notes in README
- If changing provider config, update:
  - config examples
  - validation/error handling text
  - docs/ARCHITECTURE.md

## Compatibility policy

- Avoid breaking serialized graph shape without an upgrade strategy.
- Keep `apiVersion` compatibility explicit.

## Code style

- Use existing crate lints.
- Prefer small, explicit changes in public API.
- Avoid `unwrap()` in non-test code.
