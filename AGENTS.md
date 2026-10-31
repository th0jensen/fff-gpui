# AGENTS.md

.rules

# Rust coding guidelines

* Prioritize code correctness and clarity. Speed and efficiency are secondary priorities unless otherwise specified.
* Do not write comments that summarize the code. Comments should only explain "why" when the reason is tricky or non-obvious.
* Prefer implementing functionality in existing files unless it is a new logical component. Avoid creating many small files.
* Avoid using functions that panic like `unwrap()`; prefer `?` to propagate errors.
* Be careful with operations like indexing which may panic if the indexes are out of bounds.
* Never silently discard errors with `let _ =` on fallible operations. Always handle errors appropriately:
  - Propagate errors with `?` when the calling function should handle them
  - Use explicit error handling with `match` or `if let Err(...)` when you need custom logic
* When implementing async operations that may fail, ensure errors propagate so users get meaningful feedback.
* Never create files with `mod.rs` paths; prefer `src/some_module.rs`.
* Use full words for variable names (no abbreviations).
* Use variable shadowing to scope clones in async contexts for clarity.
* Prefer `cargo fmt` and `cargo clippy` (see `.github/workflows`) before committing.

# Build

```sh
cargo build
cargo build --release
```

# Test

```sh
cargo test
cargo test <test_name>
```

In tests, prefer GPUI executor timers over `smol::Timer::after(...)` when relying on `run_until_parked()`.

# Lint and Format

```sh
cargo fmt --check
cargo clippy -- -D clippy::unwrap_used -D clippy::panic -D clippy::dbg_macro -D unused_must_use
```

# CI

Pull requests run `.github/workflows/macos-test-build.yaml`. Releases are triggered by `v*` tags via `.github/workflows/release-macos.yaml`.
