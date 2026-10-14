//! The PSRAM buffers, and drawing into them.

use crate::png;
use alloc::format;
use esp_hal::psram::Psram;

pub const D_COLS: usize = 1280;
pub const D_ROWS: usize = 720;
const SCALE: usize = D_COLS / 160;
pub const FB_ROW_BYTES: usize = D_COLS / 8;
pub const LEVELS_ROW_BYTES: usize = D_COLS / 2;
const FB_BYTES: usize = FB_ROW_BYTES * D_ROWS;
const LEVELS_BYTES: usize = png::levels_len(D_COLS, D_ROWS);
const BODY_BYTES: usize = 256 * 1024;
const SCRATCH_BYTES: usize = png::raw_len(D_COLS, D_ROWS, 8);

/// CPU-only: PSRAM's DMA limits never apply.
pub struct Buffers {
    _psram: Psram,
    /// 1bpp, MSB leftmost, 1 = black.
    pub fb: &'static mut [u8],
    /// 4bpp panel levels, even x in the high nibble, 0 = black.
    pub levels: &'static mut [u8],
    pub body: &'static mut [u8],
    pub scratch: &'static mut [u8],
}

impl Buffers {
    /// Not zeroed: every reader writes first.
    pub fn carve(psram: Psram) -> Option<Self> {
        let (base, len) = psram.raw_parts();
        // SAFETY: mapped for good (`Psram` has no `Drop`); consuming the only
        // `Psram` makes this the sole view, as PSRAM isn't in the heap.
        let mut mem = unsafe { core::slice::from_raw_parts_mut(base, len) };
        Some(Self {
            fb: mem.split_off_mut(..FB_BYTES)?,
            levels: mem.split_off_mut(..LEVELS_BYTES)?,
            body: mem.split_off_mut(..BODY_BYTES)?,
            scratch: mem.split_off_mut(..SCRATCH_BYTES)?,
            _psram: psram,
        })
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Frame {
    Mono,
    Grey,
}

impl Frame {
    /// FNV-1a: the same picture is never repainted.
    pub fn hash(self, bufs: &Buffers) -> u64 {
        let data: &[u8] = match self {
            Self::Mono => bufs.fb,
            Self::Grey => bufs.levels,
        };
        data.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| {
            (h ^ u64::from(b)).wrapping_mul(0x0100_0000_01b3)
        })
    }
}

fn fb_set(fb: &mut [u8], x: usize, y: usize) {
    if x < D_COLS
        && let Some(byte) = fb.get_mut(y * FB_ROW_BYTES + x / 8)
    {
        *byte |= 0x80 >> (x % 8);
    }
}

fn fb_fill(fb: &mut [u8], (x0, y0): (usize, usize), (w, h): (usize, usize)) {
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            fb_set(fb, x, y);
        }
    }
}

fn level_set(levels: &mut [u8], x: usize, y: usize, level: u8) {
    if x < D_COLS
        && let Some(byte) = levels.get_mut(y * LEVELS_ROW_BYTES + x / 2)
    {
        *byte = if x.is_multiple_of(2) {
            (*byte & 0x0F) | (level << 4)
        } else {
            (*byte & 0xF0) | level
        };
    }
}

fn level_fill(levels: &mut [u8], (x0, y0): (usize, usize), (w, h): (usize, usize), level: u8) {
    for y in y0..y0 + h {
        for x in x0..x0 + w {
            level_set(levels, x, y, level);
        }
    }
}

/// 5x7, MSB leftmost; drawn upper-cased, others blank.
const FONT: [(char, [u8; 7]); 40] = [
    ('A', [0x70, 0x88, 0x88, 0xF8, 0x88, 0x88, 0x88]),
    ('B', [0xF0, 0x88, 0x88, 0xF0, 0x88, 0x88, 0xF0]),
    ('C', [0x70, 0x88, 0x80, 0x80, 0x80, 0x88, 0x70]),
    ('D', [0xF0, 0x88, 0x88, 0x88, 0x88, 0x88, 0xF0]),
    ('E', [0xF8, 0x80, 0x80, 0xF0, 0x80, 0x80, 0xF8]),
    ('F', [0xF8, 0x80, 0x80, 0xF0, 0x80, 0x80, 0x80]),
    ('G', [0x70, 0x88, 0x80, 0x98, 0x88, 0x88, 0x78]),
    ('H', [0x88, 0x88, 0x88, 0xF8, 0x88, 0x88, 0x88]),
    ('I', [0xF8, 0x20, 0x20, 0x20, 0x20, 0x20, 0xF8]),
    ('J', [0x38, 0x10, 0x10, 0x10, 0x10, 0x90, 0x60]),
    ('K', [0x88, 0x90, 0xA0, 0xC0, 0xA0, 0x90, 0x88]),
    ('L', [0x80, 0x80, 0x80, 0x80, 0x80, 0x80, 0xF8]),
    ('M', [0x88, 0xD8, 0xA8, 0xA8, 0x88, 0x88, 0x88]),
    ('N', [0x88, 0xC8, 0xA8, 0x98, 0x88, 0x88, 0x88]),
    ('O', [0x70, 0x88, 0x88, 0x88, 0x88, 0x88, 0x70]),
    ('P', [0xF0, 0x88, 0x88, 0xF0, 0x80, 0x80, 0x80]),
    ('Q', [0x70, 0x88, 0x88, 0x88, 0xA8, 0x90, 0x68]),
    ('R', [0xF0, 0x88, 0x88, 0xF0, 0xA0, 0x90, 0x88]),
    ('S', [0x78, 0x80, 0x80, 0x70, 0x08, 0x08, 0xF0]),
    ('T', [0xF8, 0x20, 0x20, 0x20, 0x20, 0x20, 0x20]),
    ('U', [0x88, 0x88, 0x88, 0x88, 0x88, 0x88, 0x70]),
    ('V', [0x88, 0x88, 0x88, 0x88, 0x88, 0x50, 0x20]),
    ('W', [0x88, 0x88, 0x88, 0xA8, 0xA8, 0xA8, 0x50]),
    ('X', [0x88, 0x88, 0x50, 0x20, 0x50, 0x88, 0x88]),
    ('Y', [0x88, 0x88, 0x50, 0x20, 0x20, 0x20, 0x20]),
    ('Z', [0xF8, 0x08, 0x10, 0x20, 0x40, 0x80, 0xF8]),
    ('0', [0x70, 0x88, 0x98, 0xA8, 0xC8, 0x88, 0x70]),
    ('1', [0x20, 0x60, 0x20, 0x20, 0x20, 0x20, 0xF8]),
    ('2', [0x70, 0x88, 0x08, 0x10, 0x20, 0x40, 0xF8]),
    ('3', [0x70, 0x88, 0x08, 0x30, 0x08, 0x88, 0x70]),
    ('4', [0x10, 0x30, 0x50, 0x90, 0xF8, 0x10, 0x10]),
    ('5', [0xF8, 0x80, 0xF0, 0x08, 0x08, 0x88, 0x70]),
    ('6', [0x30, 0x40, 0x80, 0xF0, 0x88, 0x88, 0x70]),
    ('7', [0xF8, 0x08, 0x10, 0x20, 0x40, 0x40, 0x40]),
    ('8', [0x70, 0x88, 0x88, 0x70, 0x88, 0x88, 0x70]),
    ('9', [0x70, 0x88, 0x88, 0x78, 0x08, 0x10, 0x60]),
    (':', [0x00, 0x20, 0x20, 0x00, 0x20, 0x20, 0x00]),
    ('.', [0x00, 0x00, 0x00, 0x00, 0x00, 0x60, 0x60]),
    ('-', [0x00, 0x00, 0x00, 0xF8, 0x00, 0x00, 0x00]),
    ('/', [0x08, 0x08, 0x10, 0x20, 0x40, 0x80, 0x80]),
];

fn draw_text(
    text: &str,
    (x0, y0): (usize, usize),
    scale: usize,
    set: &mut dyn FnMut(usize, usize),
) {
    for (i, ch) in text.chars().enumerate() {
        let Some((_, glyph)) = FONT.iter().find(|(c, _)| *c == ch.to_ascii_uppercase()) else {
            continue;
        };
        let x = x0 + 6 * scale * i;
        for (r, bits) in glyph.iter().enumerate() {
            for c in (0..8).filter(|c| bits & (0x80 >> c) != 0) {
                for dy in 0..scale {
                    for dx in 0..scale {
                        set(x + c * scale + dx, y0 + r * scale + dy);
                    }
                }
            }
        }
    }
}

pub fn status_text(fb: &mut [u8], lines: &[&str]) {
    fb.fill(0);
    for (i, line) in lines.iter().enumerate() {
        let at = (SCALE * 15 / 2, SCALE * (15 + 20 * i));
        draw_text(line, at, SCALE, &mut |x, y| fb_set(fb, x, y));
    }
}

/// Border (cropping), corner labels (mirroring), fine detail at odd column
/// phases (nibble order).
pub fn mono_test_pattern(fb: &mut [u8]) {
    fb.fill(0);
    fb_fill(fb, (0, 0), (D_COLS, 3));
    fb_fill(fb, (0, D_ROWS - 3), (D_COLS, 3));
    fb_fill(fb, (0, 0), (3, D_ROWS));
    fb_fill(fb, (D_COLS - 3, 0), (3, D_ROWS));
    let right = |text: &str| D_COLS - 13 - text.len() * 24;
    let centre = |text: &str, scale| (D_COLS - text.len() * 6 * scale) / 2;
    let (bottom, middle) = (D_ROWS - 13 - 28, D_ROWS / 2);
    let mut text = |text: &str, at, scale| draw_text(text, at, scale, &mut |x, y| fb_set(fb, x, y));
    text("TOP LEFT", (13, 13), 4);
    text("TOP RIGHT", (right("TOP RIGHT"), 13), 4);
    text("BOTTOM LEFT", (13, bottom), 4);
    text("BOTTOM RIGHT", (right("BOTTOM RIGHT"), bottom), 4);
    text("INKPLATE 5", (centre("INKPLATE 5", 12), middle - 130), 12);
    let size = format!("{D_COLS}X{D_ROWS}");
    text(&size, (centre(&size, 8), middle), 8);
    for phase in 1..=3 {
        let at = (50 * SCALE + phase, middle + 80 + phase * 30);
        text("FINE TEXT 0123456789", at, 3);
    }
    for i in 0..15 * SCALE {
        for k in 0..4 {
            fb_set(fb, (140 + 30 * k) * SCALE / 8 + i, middle + 110 + i);
        }
    }
}

pub fn grey_test_pattern(levels: &mut [u8], scratch: &mut [u8]) -> Result<(), png::PngError> {
    const TICKETS: &[u8] =
        include_bytes!("../host/fixtures/png/makespace_tickets_1280x720_4tone.png");
    png::decode(TICKETS, D_COLS, D_ROWS, scratch)?.to_levels(levels)?;
    let (w, gap, h, label) = (8 * SCALE, SCALE, SCALE * 25 / 4, SCALE * 3 / 8);
    let top = D_ROWS - SCALE * 45 / 4;
    let x0 = D_COLS - 20 - (8 * w + 7 * gap);
    for (i, level) in (0..8u8).enumerate() {
        let x = x0 + i * (w + gap);
        level_fill(levels, (x, top), (w, h), 0);
        level_fill(levels, (x + 2, top + 2), (w - 4, h - 4), level);
        let digit = char::from(b'0' + level);
        let mut set = |px, py| level_set(levels, px, py, 0);
        draw_text(
            digit.encode_utf8(&mut [0; 4]),
            (x + w / 2 - label * 5 / 2, top + h + 6),
            label,
            &mut set,
        );
    }
    Ok(())
}
