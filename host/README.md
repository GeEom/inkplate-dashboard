# Host tests

The firmware's `src/png.rs`, compiled for the host through `#[path]` with the
strict lints in `Cargo.toml`, and checked byte for byte against PNG fixtures
with independently computed outputs.

`scripts/host_tls_test.sh` runs it. It needs stable Rust and Python 3; no
hardware.
