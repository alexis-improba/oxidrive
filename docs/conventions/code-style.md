# Code conventions

This document defines the style and quality rules for the **oxidrive** repository (Rust).

---

## Error handling

- Use **`thiserror`** to define a primary error type (e.g. `OxidriveError`) with explicit variants and stable user-facing messages when relevant.
- Public functions and most internal functions return **`Result<T, E>`** (or a `crate::error::Result<T>` alias) rather than panicking.
- **Avoid `unwrap()` and `expect()`** in production code; reserve these for tests, documented invariants, or cases where failure is structurally impossible (with a brief comment if needed).
- Propagate errors with **`?`**; add context (`map_err`, error chaining) when the call stack alone is not enough to diagnose.

---

## Naming

- **Functions, variables, modules**: `snake_case`.
- **Types, traits, enums**: `CamelCase` (PascalCase).
- **Constants**: `SCREAMING_SNAKE_CASE`.
- Prefer **verbose but clear** names for functions that perform effects (`download_file`, `open_database`) rather than obscure abbreviations.

---

## Documentation

- Every **public item** (crate, `pub` modules, structs, enums, traits, functions, significant public fields) must have **`rustdoc`** documentation (`///`) explaining its role, important invariants, and, if useful, a short example.
- Modules may start with a `//!` module comment when the grouping warrants an introduction.
- Keep comments **in sync with the code**: update or remove a comment that has become wrong.

---

## Tests

- Unit tests live **in the same file** as the code under test, under `#[cfg(test)] mod tests { ... }`.
- Prefer **focused** tests per function or per sync matrix case (as in `decision.rs`) rather than large monolithic tests.
- For network or filesystem dependencies, use **doubles** (`tempfile`, `wiremock`, etc.) when feasible to keep CI fast and deterministic.

---

## Logging

- Use the **`tracing`** crate (`tracing::info!`, `debug!`, `warn!`, `error!`) instead of `println!` for anything related to diagnostics or execution tracing.
- Console lines follow a **Git-like** form (`warning:` / `error:` prefixes; no per-line clock or `INFO`/`WARN` labels).
- Choose the **level** consistently:
  - **error**: failure that blocks an operation or sync; requires user attention (visible by default).
  - **warn**: abnormal but recoverable situation (retry, ignored file, soft quota exceeded); visible by default.
  - **info**: user-visible milestones (sync start/end, counts) shown with `--verbose`.
  - **debug** / **trace**: extra detail with `--verbose --verbose` (same human format; not a crate dump).
- Honor **`RUST_LOG`** when no CLI verbosity flags are set, plus `--verbose` / `--quiet`. Default console filter is `warn`.

---

## Concurrency

- The default async runtime is **Tokio** (multi-thread) for network operations and orchestration.
- Access to **redb** (and more generally any heavy blocking disk I/O) must avoid blocking the runtime indefinitely: wrap in **`tokio::task::spawn_blocking`** (or an equivalent documented approach) when the call is synchronous and potentially slow.
- Share state across tasks with safe primitives (`Arc`, channels, appropriate `Send`/`Sync` types); document any thread constraint if a type is not `Sync`.

---

## Formatting and clippy

- Code must pass **`cargo fmt`** with no diff.
- **`cargo clippy`** with `-D warnings` is the target on PRs (see the Git workflow).

For Git workflow and code review, see [git-workflow.md](git-workflow.md).
