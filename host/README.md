# Host tests

The firmware's `src/http.rs` and `src/png.rs`, compiled for the host through
`#[path]` with the strict lints in `Cargo.toml`, and run against a real server
and fixtures:

- a local plain HTTP server: protocol and framing;
- PNG fixtures, checked byte for byte against independently computed
  outputs.

`scripts/host_tls_test.sh` runs it all. It needs stable Rust and Python 3; no
hardware.
