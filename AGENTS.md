# AGENTS.md

## Project

fff-gpui: a macOS desktop file finder built with Rust and GPUI.

## Setup

Requirements:
- macOS (Apple Silicon or Intel)
- Rust stable via rustup
- Xcode Command Line Tools
- CMake
- Zig 0.16.0 (required by zlob); disable zlob feature to build without it

Install dependencies and build:

```sh
cargo build
```

Release build:

```sh
cargo build --release
```

## Test

Run all tests:

```sh
cargo test
```

Run a single test:

```sh
cargo test <test_name>
```

## Lint and Format

Format:

```sh
cargo fmt
```

Check formatting:

```sh
cargo fmt --check
```

Lint:

```sh
cargo clippy -- -D warnings
```

## CI

Pull requests run `.github/workflows/macos-test-build.yaml` (build + tests on macOS). Releases are triggered by `v*` tags via `.github/workflows/release-macos.yaml`.
