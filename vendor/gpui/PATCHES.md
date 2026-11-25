# Local patches to gpui 0.2.2

This is gpui 0.2.2 from crates.io, without `examples/`, with one change.

- `src/platform/linux/x11/client.rs` and `src/platform/linux/wayland/client.rs` no longer stop
  the event loop when the last window closes. The fff-gpui daemon must keep running with no
  window open; one-shot pickers quit themselves when their window closes.

Upstream Zed made this configurable with `Application::with_quit_mode` (zed PR #42391). Remove
this directory and the `[patch.crates-io]` entry in the root `Cargo.toml` once a crates.io gpui
release includes it.
