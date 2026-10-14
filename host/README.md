# Host tests

The firmware's `src/http.rs`, `src/png.rs` and `src/roots.rs`, compiled for
the host through `#[path]` with the strict lints in `Cargo.toml`, and run
against real servers and fixtures:

- local TLS 1.3 and plain HTTP servers: protocol, framing, an impostor, the
  chain memo;
- the live Makespace endpoint, when `wifi.env` has `TEST_MAKESPACE_TOKEN`;
- PNG fixtures, checked byte for byte against independently computed
  outputs;
- `cargo test`: the root store's pinned hashes.

`scripts/host_tls_test.sh` runs it all (`LIVE=0` skips the live checks). It
needs stable Rust, Python 3 and `openssl`; no hardware.
