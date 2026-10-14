//! Inkplate 5 V2 bring-up: a test pattern on the e-paper panel.

#![no_std]
#![no_main]

extern crate alloc;

mod canvas;
mod epd;

use alloc::format;
use canvas::Buffers;
use epd::Epd;
use esp_backtrace as _;
use esp_hal::{
    clock::CpuClock,
    delay::Delay,
    gpio::{Level, Output, OutputConfig},
    i2c::master::{Config as I2cConfig, I2c},
    main,
    psram::{Psram, PsramConfig},
    time::{Instant, Rate},
};
use esp_println::println;

esp_bootloader_esp_idf::esp_app_desc!();

fn halt(why: &str) -> ! {
    println!("inkplate5: {why}, halting");
    loop {
        Delay::new().delay_millis(1000);
    }
}

/// False if the panel's power never came up.
fn paint(epd: &mut Epd<'_>, fb: &[u8]) -> bool {
    if !epd.power_on() {
        println!("inkplate5: panel power failed, not painting");
        return false;
    }
    epd.reset_white();
    epd.render_mono(fb);
    epd.power_off();
    true
}

fn test_pattern(epd: &mut Epd<'_>, bufs: &mut Buffers) -> ! {
    println!("inkplate5: rendering test pattern");
    let t0 = Instant::now();
    canvas::mono_test_pattern(bufs.fb);
    if !paint(epd, bufs.fb) {
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
    let psram = Psram::new(peripherals.PSRAM, PsramConfig::default());
    let (base, len) = psram.raw_parts();
    println!("inkplate5: psram {}KB at {base:p}", len / 1024);
    let Some(mut bufs) = Buffers::carve(psram) else {
        halt("psram too small for the frame buffers")
    };
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
    test_pattern(&mut epd, &mut bufs);
}
