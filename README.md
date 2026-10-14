# inkplate-dashboard

Bare-metal Rust firmware (`no_std`,
[esp-hal](https://github.com/esp-rs/esp-hal)) that turns a Soldered
[Inkplate 5 V2](https://soldered.com/) e-paper board into a self-updating display
for an image served over HTTPS. It long-polls one URL over TLS 1.3 and
repaints only when the picture changes, so the server decides what is shown.

## Setup

- Rust for Xtensa: `espup install` (the `esp` channel; rustc 1.95 or later)
  and `espflash`. The host tests use stable Rust.
- Python 3 with `pyserial` for the flash script; `openssl` for the host tests.
- `cp wifi.env.example wifi.env` and fill it in: the 2.4GHz WPA2 network, the
  image's URL and, if the server wants one, a bearer token.

## Build and flash

```bash
python3 scripts/flash_and_monitor.py [seconds]
```

Builds with the settings in `wifi.env`, flashes `/dev/ttyUSB0`, resets the
board and shows the serial log (115200 baud) for `seconds` (default 15).
Variables already set in the shell win over `wifi.env`, which is how one file
serves several boards:

```bash
INKPLATE_PORT=/dev/ttyUSB1 INKPLATE_SERVER_URL=https://example.org/board.png \
    python3 scripts/flash_and_monitor.py
```

Under WSL the script attaches a missing port with `usbipd`; bind the board
once first (`usbipd bind`, as administrator), and set `INKPLATE_BUSID` if
several are bound. With the same variables exported, `cargo run --release`
also builds and flashes.

`INKPLATE_TEST_PATTERN=1` draws a two-tone geometry pattern and halts; `=grey`
draws a sample screen with eight grey swatches. Neither needs WiFi.

## The server

The firmware requests the URL with `width=<W>&height=<H>&tones=4&wait=55`
added to its query, the last `ETag` as `If-None-Match`, and the token, if set,
as `Authorization: Bearer`. The server should answer with:

- 200 and a greyscale PNG (depth 1 to 8) of exactly the panel's size, at most
  256KB, ending at its `Content-Length` or the close (chunked encoding isn't
  supported), ideally with an `ETag`;
- 304 when the `ETag` still matches, after holding the request for up to
  `wait` seconds while nothing changes, if it can;
- 401 or 403 for a bad token, 404 for a bad URL.

`tones` asks for at most that many grey levels; the panel shows up to eight.
`wait=55` stays inside the 60s idle cut common to hosting proxies. The
server's certificate must be ECDSA and chain to one of the roots in
`src/roots/`, as Let's Encrypt's ECDSA certificates do.

Makespace's [members-app](https://github.com/Makespace/members-app) implements
this for its machines' trouble-ticket screens, at
`/equipment/<machine>/trouble-tickets.png`.

## What the panel shows

A held request is followed at once by the next; a quick answer or a failure,
after a minute. A new image is repainted only if its pixels differ from what's
on the panel: through the eight-level waveform if it has any grey, else the
faster two-tone passes. The hash of the frame on show is kept in RTC RAM, so
the daily maintenance reboot doesn't repaint an unchanged picture.

Otherwise the panel shows one of these, each drawn once. Text is drawn in
capitals; characters other than letters, digits and `:.-/` are left blank.

| Screen | Meaning |
|---|---|
| `NO WIFI CONFIG` | Built without `INKPLATE_WIFI_SSID`; halts |
| `CONFIG ERROR` | No server URL, or one that isn't `https://host/path` without a port; halts |
| `WIFI FAILED: <SSID>` | Not joined yet; it retries every 15s |
| `SERVER SAID 401` or `403` | Wrong or missing token (`CHECK SERVER TOKEN`) |
| `SERVER SAID <other 4xx>` | Usually a wrong URL (`CHECK SERVER URL`) |
| `SERVER OFFLINE` | 30 failures in a row, including 5xx answers and undecodable images; it keeps trying |

## Tests

```bash
scripts/host_tls_test.sh
```

Builds the firmware's HTTP, TLS and PNG code for the host (`host/`) and runs
it against:

- local TLS 1.3 and HTTP servers, with a test PKI minted for each run:
  protocol and framing, an impostor certificate, a chain forged from an
  ordinary certificate, and the chain memo;
- Makespace's members-app as a live reference server, when `wifi.env` has
  `TEST_MAKESPACE_TOKEN` (`LIVE=0` skips it);
- PNG fixtures, synthetic and server-rendered, checked byte for byte against
  independently computed outputs;
- `cargo test`: the root store's pinned SHA-256s.

`cargo clippy --release` applies the lint policy in `Cargo.toml`.

## Design notes

- TLS: embedded-tls with one cipher suite (`TLS_AES_128_GCM_SHA256`) and
  ECDSA chains (P-256, P-384), verified against the five roots in
  `src/roots/` (see its README). Every certificate above the leaf must be a
  CA, which embedded-tls doesn't check itself. The board has no clock, so
  expiry and revocation aren't checked. The last verified chain is
  remembered, which cuts a repeat handshake from about 9s to under 1s.
- Secrets: `wifi.env` is compiled into the firmware and the ESP32's flash is
  readable over USB, so treat a flashed board as holding the WiFi password and
  the token. The token goes in a header, never in the logged URL.
- Build profile: the radio needs `opt-level` 2 or 3, and fat LTO with
  `opt-level = "s"` broke the WiFi blob, so releases use `opt-level = 3` and
  `lto = false`.
- DRAM: statics, including the 48KB heap, fill DRAM from the bottom, and the
  main stack gets what's left below 0x3FFE0000. Keep at least 12KB between
  `_stack_end` and that (measured on a build with `wifi.env`); with less,
  smoltcp overflows into the radio's globals.
- Order: the panel's I2S setup read-modify-writes DPORT clock registers the
  radio also writes, so the display starts before the radio.
- Watchdog: 50s at most, as esp-hal counts 80MHz ticks in a u32, which wraps
  past about 53s.
- VCOM is per panel, set at the factory in the TPS65186's EEPROM. The firmware
  reads and logs it, and never writes it.
- PSRAM holds the frame buffers, but the panel's DMA can't reach it, so each
  row is copied to internal SRAM.

## Licence

MIT (`LICENSE`), except the panel driver, `src/epd.rs`: it transliterates the
waveforms, pass sequences and scan timing of Soldered's
[Inkplate Arduino library](https://github.com/SolderedElectronics/Inkplate-Arduino-library),
so it is LGPL-3.0-only like that library (texts in `LICENSES/`).
