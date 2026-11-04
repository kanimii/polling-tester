# Polling Rate Checker

A portable Windows app that measures the report rate a mouse and keyboard
actually deliver. No installer, runtime or administrator rights.

## Use

Run `PollingRateChecker.exe` and keep its window focused.

- **Mouse:** move the mouse quickly in large circles for about 5 seconds.
- **Keyboard:** roll your fingers across letter keys, pressing and releasing
  fast, until at least 40 gaps are collected. Avoid Windows and Alt.

Windows only sends input while the window is focused, so the app cannot see
typing in other programs.

## Build

Needs Rust's GNU toolchain, which `rust-toolchain.toml` selects:

```bash
rustup toolchain install stable-x86_64-pc-windows-gnu --profile minimal
cargo test --lib
cargo build --release
```

If the project sits in a very deep folder, set `CARGO_TARGET_DIR` to a short
path, or build scripts fail with "The system cannot find the path specified".

## Developer tool

`prc-dev` lists devices and injects input at exact rates:

```bash
prc-dev devices
prc-dev selftest mouse 1000 3
prc-dev selftest key 1000 200
```

`PollingRateChecker.exe --test-mode` accepts injected input and keeps running
unfocused, for testing the UI. It sees input sent to other programs while it
runs, so it is for development only.
