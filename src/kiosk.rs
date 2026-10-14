//! The poll loop: fetch, decode, repaint only on change, and say why not.

use crate::canvas::{self, Buffers, Frame};
use crate::config::{Server, WIFI_PASS};
use crate::epd::Epd;
use crate::http::{ChainCache, FetchError, FetchOutcome};
use crate::net::{FetchTiming, Get, Net, NetError};
use crate::png;
use alloc::{format, string::String};
use esp_hal::{delay::Delay, peripherals::TIMG0, rng::Trng, time::Instant, timer::timg::Wdt};
use esp_println::println;
use smoltcp::wire::Ipv4Address;

const POLL_INTERVAL_S: u32 = 60;
/// A long-poll hold: timed from the request, not the handshake.
const HOLD_MS: u64 = 20_000;
const OFFLINE_AFTER_FAILS: u32 = 30;
/// Invisible, as the frame hash survives.
const REBOOT_AFTER_S: u64 = 24 * 3600;
const PERSIST_MAGIC: u64 = 0x1A6E_9A9E_5EED_F00D;

/// (magic, hash of the frame shown), kept over `software_reset`.
#[esp_hal::ram(unstable(rtc_fast, persistent))]
static mut PERSIST: [u64; 2] = [0; 2];

fn shown_hash() -> u64 {
    let persist = &raw const PERSIST;
    // SAFETY: plain data, used only by this one task.
    let [magic, hash] = unsafe { persist.read() };
    if magic == PERSIST_MAGIC { hash } else { 0 }
}

fn set_shown_hash(hash: u64) {
    let persist = &raw mut PERSIST;
    // SAFETY: as in `shown_hash`.
    unsafe { persist.write([PERSIST_MAGIC, hash]) };
}

/// False if the panel's power never came up.
pub fn paint(epd: &mut Epd<'_>, bufs: &Buffers, frame: Frame) -> bool {
    if !epd.power_on() {
        println!("inkplate5: panel power failed, not painting");
        return false;
    }
    epd.reset_white();
    match frame {
        Frame::Mono => epd.render_mono(bufs.fb),
        Frame::Grey => epd.render_grey(bufs.levels),
    }
    epd.power_off();
    true
}

pub fn status(epd: &mut Epd<'_>, bufs: &mut Buffers, lines: &[&str]) {
    set_shown_hash(0);
    canvas::status_text(bufs.fb, lines);
    paint(epd, bufs, Frame::Mono);
}

/// Grey if any pixel is, else the faster two-tone path.
fn decode(bufs: &mut Buffers, n: usize) -> Result<Frame, png::PngError> {
    let body = bufs.body.get(..n).ok_or(png::PngError::Truncated)?;
    let image = png::decode(body, canvas::D_COLS, canvas::D_ROWS, bufs.scratch)?;
    if image.to_levels(bufs.levels)? {
        Ok(Frame::Grey)
    } else {
        image.to_1bpp(bufs.fb)?;
        Ok(Frame::Mono)
    }
}

fn timing(t: &FetchTiming, chain: &ChainCache) -> String {
    let ms = |v: Option<u64>| v.map_or_else(|| String::from("-"), |v| format!("{v}ms"));
    let tls = if chain.last_hit {
        String::from("chain memo")
    } else {
        format!("chain verified/{}", chain.last_attempts)
    };
    format!(
        "setup {}, {tls}, response {}",
        ms(t.setup_ms),
        ms(t.response_ms)
    )
}

pub struct Parts<'a> {
    pub epd: Epd<'a>,
    pub bufs: Buffers,
    pub net: Net<'a>,
    pub wdt: Wdt<TIMG0<'static>>,
    pub delay: Delay,
    pub trng: Trng,
    pub server: Server,
    pub ssid: &'static str,
    /// `software_reset` doesn't zero the time counter.
    pub t_boot: Instant,
}

struct Kiosk<'a> {
    hw: Parts<'a>,
    /// Reused through transient DNS failures.
    last_ip: Option<Ipv4Address>,
    chain: ChainCache,
    shown: u64,
    fails: u32,
    offline_shown: bool,
    refusal_shown: Option<u16>,
}

pub fn run(hw: Parts<'_>) -> ! {
    let mut kiosk = Kiosk {
        last_ip: None,
        hw,
        chain: ChainCache::new(),
        shown: shown_hash(),
        fails: 0,
        offline_shown: false,
        refusal_shown: None,
    };
    kiosk.join();
    let server = &kiosk.hw.server;
    println!("inkplate5: polling https://{}{}", server.host, server.path);
    if kiosk.shown != 0 {
        println!("inkplate5: resumed frame hash from rtc ram");
    }
    loop {
        kiosk.hw.wdt.feed();
        let result = kiosk.fetch();
        let last = kiosk.hw.net.last_timing;
        let timing = timing(&last, &kiosk.chain);
        let held = result.is_ok() && last.response_ms.unwrap_or(0) >= HOLD_MS;
        kiosk.handle(result, &timing);
        kiosk.reboot_if_due();
        if !held {
            kiosk.idle();
        }
    }
}

impl Kiosk<'_> {
    fn join(&mut self) {
        let ssid = self.hw.ssid;
        println!("inkplate5: joining {ssid}");
        let mut joined = self.hw.net.connect_wifi(ssid, WIFI_PASS);
        let mut shown_fail = false;
        while !joined {
            self.hw.wdt.feed();
            println!("inkplate5: wifi join failed, retrying in 15s");
            if !shown_fail {
                self.status(&["WIFI FAILED:", ssid]);
                shown_fail = true;
            }
            self.hw.delay.delay_millis(15_000);
            self.hw.wdt.feed();
            joined = self.hw.net.reconnect();
        }
        println!("inkplate5: wifi associated");
        let ip = loop {
            self.hw.wdt.feed();
            if let Some(ip) = self.hw.net.wait_ip(20_000, self.hw.delay) {
                break ip;
            }
            println!("inkplate5: dhcp timeout, retrying");
        };
        println!("inkplate5: ip {ip}");
    }

    fn fetch(&mut self) -> Result<FetchOutcome, NetError> {
        let Parts {
            net,
            wdt,
            delay,
            trng,
            server,
            bufs,
            ..
        } = &mut self.hw;
        net.last_timing = FetchTiming::default();
        self.chain.last_hit = false;
        self.chain.last_attempts = 0;
        let mut feed = || wdt.feed();
        let ip = match net.resolve(server.host, 10_000, *delay, &mut feed) {
            Ok(ip) => {
                if self.last_ip != Some(ip) {
                    println!("inkplate5: dns {} -> {ip}", server.host);
                }
                self.last_ip = Some(ip);
                ip
            }
            Err(e) => self.last_ip.ok_or(e)?,
        };
        let get = Get {
            ip,
            host: server.host,
            path: &server.path,
            bearer: server.bearer,
            rng: trng,
            verify: server.tls(&mut self.chain),
        };
        net.get(get, bufs.body, *delay, &mut feed)
    }

    fn handle(&mut self, result: Result<FetchOutcome, NetError>, timing: &str) {
        match result {
            Ok(FetchOutcome::Fetched(n)) => match decode(&mut self.hw.bufs, n) {
                Ok(frame) => self.show(frame, n, timing),
                Err(e) => {
                    self.hw.net.forget_etag();
                    self.failed(&format!("unusable body ({n} bytes): {e:?}"));
                }
            },
            Ok(FetchOutcome::NotModified) => {
                self.fails = 0;
                self.offline_shown = false;
                println!("inkplate5: frame unchanged (304, {timing})");
            }
            Err(NetError::Fetch(FetchError::HttpStatus(code))) if (400..500).contains(&code) => {
                self.refused(code, timing);
            }
            Err(e) => {
                self.failed(&format!("fetch failed ({e:?}, {timing})"));
                if e == NetError::WifiDown {
                    self.hw.net.reconnect();
                }
            }
        }
    }

    fn show(&mut self, frame: Frame, n: usize, timing: &str) {
        self.fails = 0;
        self.offline_shown = false;
        self.refusal_shown = None;
        let hash = frame.hash(&self.hw.bufs);
        if hash == self.shown {
            println!("inkplate5: frame unchanged (same pixels, {timing})");
            return;
        }
        let kind = if frame == Frame::Grey { "grey" } else { "mono" };
        println!("inkplate5: new {kind} frame ({n} bytes, {timing}), rendering");
        let t0 = Instant::now();
        if paint(&mut self.hw.epd, &self.hw.bufs, frame) {
            self.shown = hash;
            set_shown_hash(hash);
            println!("inkplate5: rendered in {}ms", t0.elapsed().as_millis());
        } else {
            self.hw.net.forget_etag();
        }
    }

    /// Wrong URL, size or token: say so once, never "offline".
    fn refused(&mut self, code: u16, timing: &str) {
        self.fails = 0;
        println!("inkplate5: server refused: HTTP {code} ({timing})");
        if self.refusal_shown != Some(code) {
            let hint = if matches!(code, 401 | 403) {
                "CHECK SERVER TOKEN"
            } else {
                "CHECK SERVER URL"
            };
            self.status(&[&format!("SERVER SAID {code}"), hint]);
            self.refusal_shown = Some(code);
            self.offline_shown = false;
        }
    }

    fn failed(&mut self, what: &str) {
        self.fails += 1;
        println!("inkplate5: {what}, {} consecutive", self.fails);
        if self.fails >= OFFLINE_AFTER_FAILS && !self.offline_shown {
            self.status(&["SERVER OFFLINE", "RETRYING EVERY MINUTE"]);
            self.offline_shown = true;
            self.refusal_shown = None;
        }
    }

    /// Forgets the `ETag`, or a recovered server's 304s leave this up.
    fn status(&mut self, lines: &[&str]) {
        status(&mut self.hw.epd, &mut self.hw.bufs, lines);
        self.shown = 0;
        self.hw.net.forget_etag();
    }

    fn reboot_if_due(&self) {
        let uptime_s = self.hw.t_boot.elapsed().as_secs();
        if uptime_s > REBOOT_AFTER_S {
            println!("inkplate5: maintenance reboot (uptime {uptime_s}s)");
            self.hw.delay.delay_millis(100);
            esp_hal::system::software_reset();
        }
    }

    /// Servicing the stack: DHCP renews.
    fn idle(&mut self) {
        for _ in 0..POLL_INTERVAL_S * 20 {
            self.hw.wdt.feed();
            self.hw.net.poll();
            self.hw.delay.delay_millis(50);
        }
    }
}
