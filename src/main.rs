//! Inkplate 5 kiosk: polls a server-rendered image onto the e-paper panel.

#![no_std]
#![no_main]

extern crate alloc;

mod canvas;
mod config;
mod epd;
mod http;
mod kiosk;
mod net;
mod png;
mod roots;

use alloc::format;
use canvas::{Buffers, Frame};
use config::Server;
use embassy_crypto_rustcrypto as _;
use epd::Epd;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    interrupt::software::SoftwareInterruptControl,
    main,
    psram::{Psram, PsramConfig},
    rng::{Trng, TrngSource},
    time::{Duration, Instant, Rate},
    timer::timg::{MwdtStage, TimerGroup},
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

/// At most 50: esp-hal counts 80MHz ticks in a u32, wrapping past ~53s.
const WDT_TIMEOUT_S: u64 = 50;

/// embassy-crypto's RNG, so TLS keys come from the TRNG.
struct TrngDriver;

impl embedded_tls::embassy_crypto::driver::Rng for TrngDriver {
    fn fill_bytes(buf: &mut [u8]) {
        // The source is held for the life of `main`.
        match Trng::try_new() {
            Ok(trng) => trng.read(buf),
            Err(_) => halt("trng source gone"),
        }
    }
}

embedded_tls::embassy_crypto::rng_impl!(TrngDriver);

fn halt(why: &str) -> ! {
    println!("inkplate5: {why}, halting");
    loop {
        Delay::new().delay_millis(1000);
    }
}

fn test_pattern(epd: &mut Epd<'_>, bufs: &mut Buffers, pattern: &str) -> ! {
    println!("inkplate5: rendering test pattern {pattern}");
    let t0 = Instant::now();
    let frame = if pattern == "grey" {
        if canvas::grey_test_pattern(bufs.levels, bufs.scratch).is_err() {
            halt("grey fixture failed to decode");
        }
        Frame::Grey
    } else {
        canvas::mono_test_pattern(bufs.fb);
        Frame::Mono
    };
    if !kiosk::paint(epd, bufs, frame) {
        halt("test pattern not painted");
    }
    halt(&format!(
        "test pattern rendered in {}ms",
        t0.elapsed().as_millis()
    ));
}

#[main]
fn main() -> ! {
    let peripherals = esp_hal::init(esp_hal::Config::default().with_cpu_clock(CpuClock::max()));
    let delay = Delay::new();
    delay.delay_millis(500);
    println!(
        "inkplate5: boot, built for a {}x{} panel",
        canvas::D_COLS,
        canvas::D_ROWS
    );
    let t_boot = Instant::now();
    let psram = Psram::new(peripherals.PSRAM, PsramConfig::default());
    let (base, len) = psram.raw_parts();
    println!("inkplate5: psram {}KB at {base:p}", len / 1024);
    let Some(mut bufs) = Buffers::carve(psram) else {
        halt("psram too small for the frame buffers")
    };
    // The stack gets what statics and heap leave: keep 12KB (see README).
    esp_alloc::heap_allocator!(size: 48 * 1024);

    let i2c_config = I2cConfig::default().with_frequency(Rate::from_khz(100));
    let Ok(i2c) = I2c::new(peripherals.I2C0, i2c_config) else {
        halt("i2c init failed")
    };
    let out = OutputConfig::default();
    let _pins = [
        Output::new(peripherals.GPIO4, Level::Low, out),
        Output::new(peripherals.GPIO5, Level::Low, out),
        Output::new(peripherals.GPIO18, Level::Low, out),
        Output::new(peripherals.GPIO19, Level::Low, out),
        Output::new(peripherals.GPIO23, Level::Low, out),
        Output::new(peripherals.GPIO25, Level::Low, out),
        Output::new(peripherals.GPIO26, Level::Low, out),
        Output::new(peripherals.GPIO27, Level::Low, out),
        Output::new(peripherals.GPIO0, Level::Low, out),
        Output::new(peripherals.GPIO2, Level::Low, out),
        Output::new(peripherals.GPIO32, Level::Low, out),
        Output::new(peripherals.GPIO33, Level::High, out),
    ];
    let i2c = i2c
        .with_sda(peripherals.GPIO21)
        .with_scl(peripherals.GPIO22);
    let mut epd = Epd::new(i2c, delay);
    epd.start();
    if let Some(pattern) = config::TEST_PATTERN {
        test_pattern(&mut epd, &mut bufs, pattern);
    }

    let Some(ssid) = config::WIFI_SSID else {
        kiosk::status(
            &mut epd,
            &mut bufs,
            &["NO WIFI CONFIG", "BUILD WITH WIFI.ENV"],
        );
        halt("no wifi config (build with INKPLATE_WIFI_SSID set)");
    };
    let server = match Server::from_env() {
        Ok(server) => server,
        Err(reason) => {
            kiosk::status(&mut epd, &mut bufs, &["CONFIG ERROR", reason]);
            halt(&format!("config error: {reason}"));
        }
    };

    let timg0 = TimerGroup::new(peripherals.TIMG0);
    let mut wdt = timg0.wdt;
    wdt.set_timeout(MwdtStage::Stage0, Duration::from_secs(WDT_TIMEOUT_S));
    wdt.enable();
    let sw_ints = SoftwareInterruptControl::new(peripherals.SW_INTERRUPT);
    esp_rtos::start(timg0.timer0, sw_ints.software_interrupt0);
    // TLS keys come from the TRNG; its ADC1 source must live on.
    let _trng_source = TrngSource::new(peripherals.RNG, peripherals.ADC1);
    let Ok(trng) = Trng::try_new() else {
        halt("trng unavailable")
    };
    let seed = (u64::from(trng.random()) << 32) | u64::from(trng.random());
    // Not the default 32/32: this moves ~100KB a minute.
    let wifi_config = esp_radio::wifi::ControllerConfig::default()
        .with_dynamic_rx_buf_num(16)
        .with_dynamic_tx_buf_num(16);
    let station = esp_radio::wifi::Interface::station();
    let Ok(controller) = esp_radio::wifi::WifiController::new(peripherals.WIFI, wifi_config) else {
        halt("wifi controller failed")
    };
    println!("inkplate5: wifi interface up");
    let net = net::Net::new(controller, station, seed);
    kiosk::run(kiosk::Parts {
        epd,
        bufs,
        net,
        wdt,
        delay,
        server,
        ssid,
        t_boot,
    })
}
