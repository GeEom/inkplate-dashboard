// SPDX-License-Identifier: LGPL-3.0-only
// Copyright (c) 2026 David Gathercole; derived from Soldered's Inkplate library.

//! Panel driver, after Soldered's Inkplate5 and 5V2 drivers. Verify register
//! changes on glass: the panel ignores a wrong sequence silently.

use crate::canvas::{D_COLS, D_ROWS, FB_ROW_BYTES, LEVELS_ROW_BYTES};
use crate::config::{BOARD, Board};
use core::sync::atomic::{Ordering, compiler_fence};
use esp_hal::{Blocking, delay::Delay, i2c::master::I2c};
use esp_println::println;
use static_cell::ConstStaticCell;

#[derive(Clone, Copy)]
struct Reg(usize);

impl Reg {
    fn read(self) -> u32 {
        // SAFETY: an aligned, always-mapped TRM register, read without side effects.
        unsafe { core::ptr::with_exposed_provenance::<u32>(self.0).read_volatile() }
    }

    fn write(self, value: u32) {
        // SAFETY: as for `read`; the only DMA set up (I2S out) just reads `DMA`.
        unsafe { core::ptr::with_exposed_provenance_mut::<u32>(self.0).write_volatile(value) }
    }

    fn set(self, mask: u32) {
        self.write(self.read() | mask);
    }

    fn clear(self, mask: u32) {
        self.write(self.read() & !mask);
    }

    /// Set then clear `mask`, from one read.
    fn pulse(self, mask: u32) {
        let value = self.read();
        self.write(value | mask);
        self.write(value & !mask);
    }
}

const fn bit(n: u32) -> u32 {
    1 << n
}

const fn field(value: u32, shift: u32) -> u32 {
    value << shift
}

const GPIO_OUT_W1TS: Reg = Reg(0x3FF4_4008);
const GPIO_OUT_W1TC: Reg = Reg(0x3FF4_400C);
const GPIO_OUT1_W1TS: Reg = Reg(0x3FF4_4014);
const GPIO_OUT1_W1TC: Reg = Reg(0x3FF4_4018);
const GPIO_ENABLE_W1TS: Reg = Reg(0x3FF4_4024);
const GPIO_FUNC_OUT_SEL: usize = 0x3FF4_4530;
const IO_MUX: usize = 0x3FF4_9000;
const DPORT_PERIP_CLK_EN: Reg = Reg(0x3FF0_00C0);
const DPORT_PERIP_RST_EN: Reg = Reg(0x3FF0_00C4);
const DPORT_I2S1: u32 = bit(21);

const I2S1: usize = 0x3FF6_D000;
const I2S_CONF: Reg = Reg(I2S1 + 0x08);
const I2S_INT_RAW: Reg = Reg(I2S1 + 0x0C);
const I2S_INT_CLR: Reg = Reg(I2S1 + 0x18);
const I2S_TIMING: Reg = Reg(I2S1 + 0x1C);
const I2S_FIFO_CONF: Reg = Reg(I2S1 + 0x20);
const I2S_CONF_CHAN: Reg = Reg(I2S1 + 0x2C);
const I2S_OUT_LINK: Reg = Reg(I2S1 + 0x30);
const I2S_LC_CONF: Reg = Reg(I2S1 + 0x60);
const I2S_CONF1: Reg = Reg(I2S1 + 0xA0);
const I2S_CONF2: Reg = Reg(I2S1 + 0xA8);
const I2S_CLKM_CONF: Reg = Reg(I2S1 + 0xAC);
const I2S_SAMPLE_RATE_CONF: Reg = Reg(I2S1 + 0xB0);
const I2S_STATE: Reg = Reg(I2S1 + 0xBC);

const TX_RESET: u32 = bit(0);
const RX_RESET: u32 = bit(1);
const TX_FIFO_RESET: u32 = bit(2);
const RX_FIFO_RESET: u32 = bit(3);
const TX_START: u32 = bit(4);
const RIGHT_FIRST: u32 = bit(8) | bit(9);
const OUT_TOTAL_EOF: u32 = bit(16);
const OUTLINK_ADDR: u32 = 0x000F_FFFF;
const OUTLINK_STOP: u32 = bit(28);
const OUTLINK_START: u32 = bit(29);
const IN_RST: u32 = bit(0);
const OUT_RST: u32 = bit(1);
const OUTDSCR_BURST_EN: u32 = bit(9);
const OUT_DATA_BURST_EN: u32 = bit(11);
const TX_PCM_BYPASS: u32 = bit(3);
const TX_STOP_EN: u32 = bit(8);
const LCD_TX_WRX2_EN: u32 = bit(1);
const LCD_EN: u32 = bit(5);
const TX_IDLE: u32 = bit(0);

/// (GPIO, I2S1 signal, `IO_MUX` offset).
const I2S_PINS: [(u8, u32, usize); 9] = [
    (0, 24, 0x44),
    (4, 166, 0x48),
    (5, 167, 0x6C),
    (18, 168, 0x70),
    (19, 169, 0x74),
    (23, 170, 0x8C),
    (25, 171, 0x24),
    (26, 172, 0x28),
    (27, 173, 0x2C),
];

const EPD_LE: u32 = bit(2);
const EPD_CKV: u32 = bit(0);
const EPD_SPH: u32 = bit(1);

const PCAL_ADDR: u8 = 0x20;
const PCAL_OUT0: u8 = 0x02;
const PCAL_OUT1: u8 = 0x03;
const PCAL_CFG0: u8 = 0x06;
const PCAL_CFG1: u8 = 0x07;
const EXP_OE: u8 = 0x01;
const EXP_GMODE: u8 = 0x02;
const EXP_SPV: u8 = 0x04;
const EXP_WAKEUP: u8 = 0x08;
const EXP_PWRUP: u8 = 0x10;
const EXP_VCOM: u8 = 0x20;
/// 1 = input: `P0_6` PMIC interrupt, `P0_7`, `P1_2` microSD power.
const EXP_DIR0: u8 = 0xC0;
const EXP_DIR1: u8 = 0x04;
/// `P1_0` passes CL to the panel; without it every frame is ignored.
const EXP1_GPIO0_ENABLE: u8 = 0x01;

const TPS_ADDR: u8 = 0x48;
const TPS_ENABLE: u8 = 0x01;
/// Alone: PWRUP sequences the rest.
const TPS_V3P3_EN: u8 = 0x20;
const TPS_VCOM1: u8 = 0x03;
const TPS_VCOM2: u8 = 0x04;
const TPS_UPSEQ0: u8 = 0x09;
const TPS_DWNSEQ0: u8 = 0x0B;
const TPS_PG: u8 = 0x0F;
const TPS_ALL_GOOD: u8 = 0xFA;

const LINE_BYTES: usize = D_COLS / 4;
const DMA_LEN: u32 = match BOARD {
    Board::Inkplate5 => 248,
    Board::Inkplate5V2 => 328,
};
const _: () = assert!(DMA_LEN as usize == LINE_BYTES + 8);
/// `lldesc_t`: size, length, SOSF, EOF, DMA-owned.
const DESC_CONFIG: u32 = field(DMA_LEN, 0) | field(DMA_LEN, 12) | bit(29) | bit(30) | bit(31);

/// The I2S DMA can't reach PSRAM.
#[repr(C, align(4))]
struct Dma {
    desc: [u32; 3],
    line: [u8; LINE_BYTES],
    tail: [u8; 8],
}

static DMA: ConstStaticCell<Dma> = ConstStaticCell::new(Dma {
    desc: [0; 3],
    line: [0; LINE_BYTES],
    tail: [0; 8],
});

const WHITE: u8 = 0xAA;
const BLACK: u8 = 0x55;
const DISCHARGE: u8 = 0x00;
const LEAVE: u8 = 0xFF;

const BOTTOM_UP: bool = matches!(BOARD, Board::Inkplate5);
const CLEARS: u8 = match BOARD {
    Board::Inkplate5 => 14,
    Board::Inkplate5V2 => 11,
};

/// Soldered's `clean()` sequence.
const RESET_SEQ: [(u8, u8); 9] = [
    (WHITE, 1),
    (BLACK, CLEARS),
    (DISCHARGE, 1),
    (WHITE, CLEARS),
    (DISCHARGE, 1),
    (BLACK, CLEARS),
    (DISCHARGE, 1),
    (WHITE, CLEARS),
    (
        DISCHARGE,
        match BOARD {
            Board::Inkplate5 => 1,
            Board::Inkplate5V2 => 0,
        },
    ),
];

/// Nibble (first pixel in bit 3) to 2bpp: Soldered's LUT2 and LUTB.
const LUT_DRIVE: [u8; 16] = [
    0xAA, 0xA9, 0xA6, 0xA5, 0x9A, 0x99, 0x96, 0x95, 0x6A, 0x69, 0x66, 0x65, 0x5A, 0x59, 0x56, 0x55,
];
const LUT_BLACK: [u8; 16] = [
    0xFF, 0xFD, 0xF7, 0xF5, 0xDF, 0xDD, 0xD7, 0xD5, 0x7F, 0x7D, 0x77, 0x75, 0x5F, 0x5D, 0x57, 0x55,
];
const MONO_PASSES: &[&[u8; 16]] = match BOARD {
    Board::Inkplate5 => &[
        &LUT_BLACK, &LUT_BLACK, &LUT_BLACK, &LUT_BLACK, &LUT_BLACK, &LUT_DRIVE,
    ],
    Board::Inkplate5V2 => &[&LUT_BLACK, &LUT_BLACK, &LUT_BLACK, &LUT_DRIVE],
};

const GREY_PHASES: usize = 9;
/// Soldered's WAVEFORM3BIT, `[level][phase]`: 0 leave, 1 black, 2 white.
const GREY_WAVEFORM: [[u8; GREY_PHASES]; 8] = match BOARD {
    Board::Inkplate5 => [
        [0, 0, 1, 1, 0, 1, 1, 1, 0],
        [0, 1, 1, 1, 1, 2, 0, 1, 0],
        [1, 2, 2, 0, 2, 1, 1, 1, 0],
        [1, 1, 1, 2, 0, 1, 1, 2, 0],
        [0, 1, 1, 1, 2, 0, 1, 2, 0],
        [0, 0, 0, 1, 1, 2, 1, 2, 0],
        [1, 1, 1, 2, 0, 2, 1, 2, 0],
        [0, 0, 0, 0, 0, 0, 0, 0, 0],
    ],
    Board::Inkplate5V2 => [
        [0, 0, 1, 1, 2, 1, 1, 1, 0],
        [1, 1, 2, 2, 1, 2, 1, 1, 0],
        [0, 1, 2, 2, 1, 1, 2, 1, 0],
        [0, 0, 1, 1, 1, 1, 1, 2, 0],
        [1, 2, 1, 2, 1, 1, 1, 2, 0],
        [0, 1, 1, 1, 2, 0, 1, 2, 0],
        [1, 1, 1, 2, 2, 2, 1, 2, 0],
        [0, 0, 0, 0, 0, 0, 0, 0, 0],
    ],
};

/// Soldered's GLUT: a level byte to 4 bits, odd pixel in bits 3-2.
fn grey_lut(phase: usize) -> [u8; 256] {
    let wave = |level: usize| {
        GREY_WAVEFORM
            .get(level & 7)
            .and_then(|codes| codes.get(phase))
            .copied()
            .unwrap_or_default()
    };
    core::array::from_fn(|byte| (wave(byte) << 2) | wave(byte >> 4))
}

fn addr<T>(ptr: *const T) -> u32 {
    u32::try_from(ptr.expose_provenance()).unwrap_or_default()
}

pub struct Epd<'a> {
    i2c: I2c<'a, Blocking>,
    delay: Delay,
    olat: u8,
    dma: &'static mut Dma,
}

impl<'a> Epd<'a> {
    pub fn new(i2c: I2c<'a, Blocking>, delay: Delay) -> Self {
        Self {
            i2c,
            delay,
            olat: 0,
            dma: DMA.take(),
        }
    }

    fn write(&mut self, device: u8, reg: u8, value: u8) {
        self.i2c.write(device, &[reg, value]).unwrap_or(());
    }

    fn read(&mut self, device: u8, reg: u8) -> Option<u8> {
        let mut value = [0];
        self.i2c.write_read(device, &[reg], &mut value).ok()?;
        let [v] = value;
        Some(v)
    }

    fn olat_set(&mut self, bits: u8) {
        self.olat |= bits;
        self.write(PCAL_ADDR, PCAL_OUT0, self.olat);
    }

    fn olat_clear(&mut self, bits: u8) {
        self.olat &= !bits;
        self.write(PCAL_ADDR, PCAL_OUT0, self.olat);
    }

    /// Soldered's einkOn: false, and off again, without power-good.
    pub fn power_on(&mut self) -> bool {
        // Latches power up high: set them first.
        self.write(PCAL_ADDR, PCAL_OUT0, self.olat);
        self.write(PCAL_ADDR, PCAL_OUT1, EXP1_GPIO0_ENABLE);
        self.write(PCAL_ADDR, PCAL_CFG0, EXP_DIR0);
        self.write(PCAL_ADDR, PCAL_CFG1, EXP_DIR1);
        self.olat_set(EXP_WAKEUP);
        self.delay.delay_millis(5);
        self.write(TPS_ADDR, TPS_ENABLE, TPS_V3P3_EN);
        self.write(TPS_ADDR, TPS_UPSEQ0, 0xE4);
        self.write(TPS_ADDR, TPS_DWNSEQ0, 0x1B);
        GPIO_OUT_W1TC.write(EPD_LE);
        GPIO_OUT1_W1TS.write(EPD_SPH);
        GPIO_OUT1_W1TC.write(EPD_CKV);
        self.olat_set(EXP_GMODE | EXP_SPV);
        self.olat_set(EXP_PWRUP);
        if !self.power_good_is(TPS_ALL_GOOD, 500) {
            self.power_off();
            return false;
        }
        self.olat_set(EXP_VCOM);
        self.olat_set(EXP_OE);
        true
    }

    /// Soldered's einkOff.
    pub fn power_off(&mut self) {
        self.olat_clear(EXP_OE);
        self.olat_clear(EXP_GMODE);
        GPIO_OUT1_W1TC.write(EPD_CKV | EPD_SPH);
        self.olat_clear(EXP_SPV);
        self.olat_clear(EXP_VCOM);
        self.olat_clear(EXP_PWRUP);
        self.power_good_is(0, 250);
        self.olat_clear(EXP_WAKEUP);
        self.write(TPS_ADDR, TPS_ENABLE, 0);
    }

    fn power_good_is(&mut self, want: u8, ms: u32) -> bool {
        (0..ms).any(|_| {
            self.delay.delay_millis(1);
            self.read(TPS_ADDR, TPS_PG) == Some(want)
        })
    }

    /// Before the radio: both read-modify-write the DPORT clock registers.
    pub fn start(&mut self) {
        println!("inkplate5: power_good={}", self.power_on());
        let cfg0 = self.read(PCAL_ADDR, PCAL_CFG0);
        let cfg1 = self.read(PCAL_ADDR, PCAL_CFG1);
        let out1 = self.read(PCAL_ADDR, PCAL_OUT1);
        if let (Some(cfg0), Some(cfg1), Some(out1)) = (cfg0, cfg1, out1) {
            println!("inkplate5: expander cfg0={cfg0:#04x} cfg1={cfg1:#04x} out1={out1:#04x}");
        } else {
            println!("inkplate5: expander not responding at {PCAL_ADDR:#04x}");
        }
        // Factory VCOM in 10mV steps, from the PMIC's EEPROM: never written.
        let lo = self.read(TPS_ADDR, TPS_VCOM1);
        let hi = self.read(TPS_ADDR, TPS_VCOM2);
        if let (Some(lo), Some(hi)) = (lo, hi) {
            let vcom = (u16::from(hi & 1) << 8) | u16::from(lo);
            println!(
                "inkplate5: vcom -{}.{:02}V (tps65186 eeprom)",
                vcom / 100,
                vcom % 100
            );
        } else {
            println!("inkplate5: vcom read failed");
        }
        i2s_setup();
        self.power_off();
    }

    fn ckv(high: bool) {
        if high {
            GPIO_OUT1_W1TS.write(EPD_CKV);
        } else {
            GPIO_OUT1_W1TC.write(EPD_CKV);
        }
    }

    /// A CKV clock: the gate driver misses lows under ~1us.
    fn ckv_clock(&self) {
        Self::ckv(false);
        self.delay.delay_micros(1);
        Self::ckv(true);
    }

    /// The scan token: SPV low across a CKV pulse.
    fn vscan_start(&mut self) {
        Self::ckv(true);
        self.delay.delay_micros(7);
        self.olat_clear(EXP_SPV);
        self.delay.delay_micros(10);
        self.ckv_clock();
        self.delay.delay_micros(8);
        self.olat_set(EXP_SPV);
        self.delay.delay_micros(10);
        self.ckv_clock();
        self.delay.delay_micros(18);
        self.ckv_clock();
        self.delay.delay_micros(18);
        self.ckv_clock();
    }

    fn stop_outlink() {
        I2S_OUT_LINK.write((I2S_OUT_LINK.read() | OUTLINK_STOP) & !OUTLINK_START);
    }

    fn send_row(&mut self) {
        Self::stop_outlink();
        I2S_CONF.clear(TX_START);
        I2S_CONF.pulse(TX_FIFO_RESET);
        I2S_LC_CONF.pulse(OUT_RST);
        I2S_CONF.pulse(TX_RESET);
        I2S_LC_CONF.write(OUT_DATA_BURST_EN | OUTDSCR_BURST_EN);
        self.dma.desc = [DESC_CONFIG, addr(self.dma.line.as_ptr()), 0];
        compiler_fence(Ordering::SeqCst);
        let desc = addr(self.dma.desc.as_ptr());
        I2S_OUT_LINK.write((I2S_OUT_LINK.read() & !OUTLINK_ADDR) | (desc & OUTLINK_ADDR));
        I2S_OUT_LINK.set(OUTLINK_START);
        GPIO_OUT1_W1TC.write(EPD_SPH);
        self.delay.delay_micros(15);
        Self::ckv(true);
        I2S_CONF.set(TX_START);
        // At EOF up to a row is still in the FIFO: wait for idle.
        while I2S_INT_RAW.read() & OUT_TOTAL_EOF == 0 {}
        while I2S_STATE.read() & TX_IDLE == 0 {}
        GPIO_OUT1_W1TS.write(EPD_SPH);
        I2S_INT_CLR.write(I2S_INT_RAW.read());
        Self::stop_outlink();
        Self::ckv(false);
        GPIO_OUT_W1TS.write(EPD_LE);
        GPIO_OUT_W1TC.write(EPD_LE);
    }

    fn pass(&mut self, mut fill: impl FnMut(usize, &mut [u8; LINE_BYTES])) {
        self.vscan_start();
        for scan in 0..D_ROWS {
            let row = if BOTTOM_UP { D_ROWS - 1 - scan } else { scan };
            fill(row, &mut self.dma.line);
            self.send_row();
        }
        self.delay.delay_micros(230);
    }

    fn pass_uniform(&mut self, data: u8) {
        self.dma.line.fill(data);
        self.pass(|_, _| {});
    }

    pub fn reset_white(&mut self) {
        for (data, repeats) in RESET_SEQ {
            for _ in 0..repeats {
                self.pass_uniform(data);
            }
        }
    }

    pub fn render_mono(&mut self, fb: &[u8]) {
        for lut in MONO_PASSES {
            self.pass(|row, line| mono_row(fb, row, lut, line));
        }
        self.pass_uniform(DISCHARGE);
        self.finish();
    }

    pub fn render_grey(&mut self, levels: &[u8]) {
        for phase in 0..GREY_PHASES {
            let lut = grey_lut(phase);
            self.pass(|row, line| grey_row(levels, row, &lut, line));
        }
        self.pass_uniform(LEAVE);
        self.finish();
    }

    fn finish(&mut self) {
        if BOARD == Board::Inkplate5 {
            self.vscan_start();
        }
    }
}

/// Right to left: from the last byte, bit-reversed, in pairs swapped for the
/// I2S FIFO's half-words.
fn mono_row(fb: &[u8], row: usize, lut: &[u8; 16], line: &mut [u8; LINE_BYTES]) {
    let src = fb
        .get(row * FB_ROW_BYTES..(row + 1) * FB_ROW_BYTES)
        .unwrap_or_default();
    let code = |nibble: u8| {
        lut.get(usize::from(nibble & 0x0F))
            .copied()
            .unwrap_or_default()
    };
    for (out, pair) in line
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src.rchunks_exact(2))
    {
        let &[b1, b0] = pair else { continue };
        let (d0, d1) = (b0.reverse_bits(), b1.reverse_bits());
        out.copy_from_slice(&[code(d1 >> 4), code(d1), code(d0 >> 4), code(d0)]);
    }
}

/// Soldered's display3b.
fn grey_row(levels: &[u8], row: usize, lut: &[u8; 256], line: &mut [u8; LINE_BYTES]) {
    let src = levels
        .get(row * LEVELS_ROW_BYTES..(row + 1) * LEVELS_ROW_BYTES)
        .unwrap_or_default();
    let code = |byte: u8| lut.get(usize::from(byte)).copied().unwrap_or_default();
    let px4 = |hi: u8, lo: u8| (code(hi) << 4) | code(lo);
    for (out, bytes) in line
        .as_chunks_mut::<4>()
        .0
        .iter_mut()
        .zip(src.rchunks_exact(8))
    {
        let &[c0, c1, c2, c3, c4, c5, c6, c7] = bytes else {
            continue;
        };
        out.copy_from_slice(&[px4(c3, c2), px4(c1, c0), px4(c7, c6), px4(c5, c4)]);
    }
}

fn i2s_setup() {
    DPORT_PERIP_CLK_EN.set(DPORT_I2S1);
    DPORT_PERIP_RST_EN.pulse(DPORT_I2S1);
    I2S_CONF.pulse(TX_FIFO_RESET | RX_FIFO_RESET);
    I2S_LC_CONF.pulse(IN_RST | OUT_RST);
    I2S_CONF.pulse(TX_RESET | RX_RESET);
    I2S_CONF2.write(LCD_EN | LCD_TX_WRX2_EN);
    // BCK /2, 8-bit samples.
    I2S_SAMPLE_RATE_CONF.write(field(2, 0) | field(2, 6) | field(8, 12) | field(8, 18));
    // Clock /5, a=1 b=0.
    I2S_CLKM_CONF.write(field(5, 0) | field(0, 8) | field(1, 14) | field(0, 21));
    // Descriptors, TX FIFO mode 1 forced.
    I2S_FIFO_CONF.write(bit(0) | bit(6) | bit(12) | bit(13) | bit(19) | bit(20));
    I2S_CONF1.write(TX_PCM_BYPASS);
    I2S_CONF_CHAN.write(bit(0) | bit(3));
    I2S_CONF.clear(RIGHT_FIRST);
    I2S_TIMING.write(0);
    for (pin, signal, mux) in I2S_PINS {
        Reg(GPIO_FUNC_OUT_SEL + 4 * usize::from(pin)).write(signal);
        GPIO_ENABLE_W1TS.write(bit(u32::from(pin)));
        let mux = Reg(IO_MUX + mux);
        mux.write(0);
        // Strongest drive, GPIO function.
        mux.write(field(3, 10) | field(2, 12));
    }
    I2S_CONF1.write(TX_PCM_BYPASS | TX_STOP_EN);
}
