# inkplate-dashboard

Bare metal Rust firmware (`no_std`,
[esp-hal](https://github.com/esp-rs/esp-hal)) that turns a Soldered
[Inkplate 5](https://soldered.com/) e-paper board into a self-updating display
for an image served over HTTPS. It long polls one URL over TLS 1.3 and
repaints only when the picture changes, so the server decides what is shown.

## Hardware

| Board | Panel | Build |
|---|---|---|
| Inkplate 5 V2 | 1280x720 | default |
| Inkplate 5, first generation | 960x540 | `INKPLATE_BOARD=5` |

The firmware can't tell the two panels apart at runtime, so the build names
the board.

## Setup

- `espup install` for Rust on Xtensa (the `esp` channel, rustc 1.99 or
  later) and `espflash`, plus stable Rust for the host tests.
- Python 3 and `openssl` for the host tests.
- `cp wifi.env.example wifi.env` and fill it in: the 2.4GHz WPA2 network, the
  image's URL and, if the server wants one, a bearer token.

## Build and flash

```bash
scripts/flash.sh
```

It compiles with the settings in `wifi.env`, flashes the board with `espflash`
and then shows the serial log. Variables already set in the shell win over `wifi.env`,
which is how one file serves several boards, and any arguments go to
`espflash`:

```bash
INKPLATE_BOARD=5 INKPLATE_SERVER_URL=https://example.org/board.png \
    scripts/flash.sh --port /dev/ttyUSB1
```

`INKPLATE_TEST_PATTERN=1` paints a two-tone geometry pattern and halts. With
`=grey` it paints eight grey swatches instead, and neither needs WiFi.

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
`wait=55` keeps under the 60s idle cut common to hosting proxies. The
server's certificate must be ECDSA and chain to one of the roots in
`src/roots/`, as Let's Encrypt's ECDSA certificates do.

Makespace's [members-app](https://github.com/Makespace/members-app) implements
this for its machines' trouble-ticket screens, at `/e-ink/<n>.png`. Each board
is flashed once with its own number. The app's `/e-ink` page says which
machine each number shows, so a board can move without a reflash.

## What the panel shows

After a held request the next one goes out at once. After a quick answer or a
failure the board waits a minute. The panel repaints a new image only when its
pixels differ from the picture on show. It uses the eight-level waveform for
an image with grey and the faster two-tone passes for one without. The frame's
hash lives in RTC RAM, so the daily reboot doesn't repaint an unchanged
picture.

Otherwise the panel shows one of these screens, each drawn once. Text is in
capitals. Characters other than letters, digits and `:.-/` come out blank.

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

This compiles the firmware's HTTP, TLS and PNG code for the host (`host/`) and
runs it against these.

- local TLS 1.3 and HTTP servers with a fresh test PKI each run. They try
  the protocol, an impostor certificate, a chain forged from an ordinary
  certificate and the chain memo;
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
  CA, which embedded-tls doesn't check itself. The board has no clock. It
  checks neither expiry nor revocation. The last verified chain is
  remembered, which cuts a repeat handshake from about 9s to under 1s.
- `wifi.env` is compiled into the firmware, and the ESP32's flash can be read
  over USB, so a flashed board carries the WiFi password and the token. The
  token goes in a header, never in the logged URL.
- The radio needs `opt-level` 2 or 3. Fat LTO with `opt-level = "s"` broke
  the WiFi blob, so releases use `opt-level = 3` and `lto = false`.
- Statics, the 48KB heap among them, fill DRAM from the bottom. The main
  stack gets what's left below 0x3FFE0000. Keep at least 12KB between
  `_stack_end` and that (measured on a build with `wifi.env`); with less,
  smoltcp overflows into the radio's globals.
- The display starts first. Its I2S setup read-modify-writes DPORT clock
  registers that the radio also writes.
- The watchdog is capped at 50s. esp-hal counts 80MHz ticks in a u32, which
  wraps past about 53s.
- VCOM is per panel, set at the factory in the TPS65186's EEPROM. The firmware
  reads and logs it, and never writes it.
- The frame buffers sit in PSRAM. The panel's DMA can't reach PSRAM, so each
  row is copied to internal SRAM first.
- The older panel scans from the bottom up. It has its own clear count, pass
  sequence and grey waveform, picked at compile time.

## Licence

MIT (`LICENSE`), except the panel driver, `src/epd.rs`: it transliterates the
waveforms, pass sequences and scan timing of Soldered's
[Inkplate Arduino library](https://github.com/SolderedElectronics/Inkplate-Arduino-library),
so it is LGPL-3.0-only like that library (texts in `LICENSES/`).
